//! `POST /audio/ingest` — admission for ambient capture segments.
//!
//! The body is raw audio and the metadata is in query parameters. This is
//! deliberately not JSON-RPC on `/rpc`: base64 would inflate the payload by
//! a third and force a microcontroller to assemble JSON around a
//! multi-kilobyte blob, and radio-on time is the device's dominant battery
//! cost. `curl` must be able to speak this.
//!
//! Admission is separate from processing. Reconnecting after a day offline
//! dumps a day of spooled segments at once; the handler enqueues and
//! returns so the device can put its radio back to sleep.
//!
//! This module owns its own axum state ([`AmbientState`]) rather than
//! sharing `crate::serve::ServeState`. `ServeState` exists to serve LLM
//! turns; nothing here may start one, so it needs none of the machinery
//! (provider registry, workspace, tool set, session stores) that
//! `ServeState` carries. [`routes`] therefore applies its own
//! `.with_state()` and hands back a plain `Router` for a later task to
//! merge into the main one.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::AmbientConfig;
use crate::device_auth::DeviceAuth;
use crate::voice::PIPELINE_SAMPLE_RATE;

/// Content types accepted in v1. `audio/opus` is the documented
/// forward-compatibility seam and is deliberately absent until the
/// device's power draw has been measured.
const ACCEPTED_CONTENT_TYPES: &[&str] = &["audio/l16", "audio/wav"];

/// One admitted segment on its way to the processing worker.
#[derive(Debug, Clone)]
pub struct Segment {
    pub segment: String,
    /// Resolved from the bearer token, never from the request.
    pub device: String,
    /// When the audio was recorded.
    pub started_at: DateTime<Utc>,
    /// True only for realtime audio. Replayed spool is false, which is what
    /// keeps a future wake-word path from answering hours-old speech.
    pub live: bool,
    pub pcm: Vec<i16>,
}

/// Everything the ambient ingest routes need. Deliberately separate from
/// `crate::serve::ServeState` — see the module doc.
pub struct AmbientState {
    pub config: AmbientConfig,
    /// Shared with `ServeState` so there is exactly one answer to "who is
    /// this token" in the process.
    pub devices: Arc<DeviceAuth>,
    pub tx: mpsc::Sender<Segment>,
    /// Segment ids already admitted, for idempotency.
    seen: Mutex<HashSet<String>>,
}

impl AmbientState {
    pub fn new(config: AmbientConfig, devices: Arc<DeviceAuth>, tx: mpsc::Sender<Segment>) -> Self {
        Self {
            config,
            devices,
            tx,
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// Record `id` as admitted. Returns false when it was already present.
    ///
    /// Claiming the id *before* the body is decoded and enqueued is what
    /// serialises two concurrent POSTs of the same segment; the loser sees
    /// the id already taken and discards its body. The cost is that every
    /// path which then fails to enqueue must give the id back — see
    /// [`AmbientState::forget`].
    fn admit_once(&self, id: &str) -> bool {
        self.seen
            .lock()
            .expect("ambient seen-set poisoned")
            .insert(id.to_string())
    }

    /// Release an id claimed by [`AmbientState::admit_once`] when the
    /// request did not end up enqueuing anything.
    ///
    /// Without this, a segment refused with 429 (queue full) is answered
    /// `200 OK` on the retry the spec tells the device to make, and its
    /// body is discarded as a duplicate: the audio is lost and the device
    /// believes it was delivered. That fires precisely during a reconnect
    /// burst, which is the case the bounded queue exists to handle.
    fn forget(&self, id: &str) {
        self.seen
            .lock()
            .expect("ambient seen-set poisoned")
            .remove(id);
    }
}

#[derive(Debug, Deserialize)]
struct IngestParams {
    segment: String,
    started_at: i64,
    #[serde(default = "default_rate")]
    rate: u32,
    #[serde(default)]
    live: u8,
}

fn default_rate() -> u32 {
    PIPELINE_SAMPLE_RATE
}

/// Build the ambient ingest routes, bound to their own state.
pub fn routes(state: Arc<AmbientState>) -> Router {
    Router::new()
        .route("/audio/ingest", post(handle_ingest))
        .route("/audio/hello", post(handle_hello))
        .with_state(state)
}

/// Resolve the bearer to a device name, or the error response to return.
///
/// Every refusal `DeviceAuth::resolve` can give (unknown token, retired
/// device, device not allowed to use the agent, device unrouted) collapses
/// to the same 401 — the distinction is logged, not returned. A bridge that
/// cannot be asked is 503, so the device retries rather than concluding its
/// key is wrong.
///
/// The `enabled` check here is **defence in depth, not the mechanism**:
/// `ambient::startup::build` returns `None` when `[ambient].enabled` is
/// false, and `main` then never mounts these routes at all, so a disabled
/// subsystem 404s by router omission. This guard only matters if some
/// future caller mounts the routes with a disabled config, and it answers
/// 404 rather than 401 so that case stays indistinguishable from the
/// unmounted one — a device probing for the feature must never be told its
/// key is wrong.
async fn authenticate(state: &AmbientState, headers: &HeaderMap) -> Result<String, StatusCode> {
    if !state.config.enabled {
        return Err(StatusCode::NOT_FOUND);
    }
    let token = crate::serve::extract_bearer(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    match state.devices.resolve(&token).await {
        Ok(r) => Ok(r.device.name),
        Err(crate::device_auth::AuthError::Unavailable) => Err(StatusCode::SERVICE_UNAVAILABLE),
        Err(crate::device_auth::AuthError::Refused) => {
            debug!(
                "ambient: rejected bearer (unknown, retired, not allowed, or bound to no room profile)"
            );
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

async fn handle_ingest(
    State(state): State<Arc<AmbientState>>,
    Query(params): Query<IngestParams>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let device = match authenticate(&state, &headers).await {
        Ok(d) => d,
        Err(status) => return status.into_response(),
    };

    if params.rate != PIPELINE_SAMPLE_RATE {
        return (
            StatusCode::BAD_REQUEST,
            format!("rate must be {PIPELINE_SAMPLE_RATE}; resampling is not performed"),
        )
            .into_response();
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !ACCEPTED_CONTENT_TYPES.contains(&content_type.as_str()) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("accepted: {}", ACCEPTED_CONTENT_TYPES.join(", ")),
        )
            .into_response();
    }

    // A replay of an already-admitted segment is a normal condition, not an
    // error: spool replay and live delivery share one path.
    if !state.admit_once(&params.segment) {
        debug!("ambient: duplicate segment {} discarded", params.segment);
        return StatusCode::OK.into_response();
    }

    // From here on the id is claimed, so every exit that does not enqueue
    // must release it again — otherwise the retry the device is told to
    // make is answered as a duplicate and its audio is silently dropped.
    let decoded = match content_type.as_str() {
        "audio/wav" => decode_wav(&body),
        _ => decode_l16(&body),
    };
    let pcm = match decoded {
        Ok(pcm) => pcm,
        Err(e) => {
            state.forget(&params.segment);
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    };

    let Some(started_at) = Utc.timestamp_millis_opt(params.started_at).single() else {
        state.forget(&params.segment);
        return (StatusCode::BAD_REQUEST, "started_at out of range").into_response();
    };

    let seg = Segment {
        segment: params.segment.clone(),
        device,
        started_at,
        live: params.live != 0,
        pcm,
    };

    match state.tx.try_send(seg) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(mpsc::error::TrySendError::Full(_)) => {
            state.forget(&params.segment);
            warn!("ambient: admission queue full; asking the device to retry");
            StatusCode::TOO_MANY_REQUESTS.into_response()
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            state.forget(&params.segment);
            warn!("ambient: worker gone; refusing ingest");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn handle_hello(
    State(state): State<Arc<AmbientState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let device = match authenticate(&state, &headers).await {
        Ok(d) => d,
        Err(status) => return status.into_response(),
    };
    Json(json!({
        "device": device,
        "sample_rate": PIPELINE_SAMPLE_RATE,
        "accepts": ["audio/L16", "audio/wav"],
        // Reserved as GET /audio/events for S4; not implemented.
        "downlink": false,
    }))
    .into_response()
}

/// Decode raw s16le. An odd-length body means a truncated transmission —
/// rejected rather than silently dropping the trailing byte, the same
/// failure class as accepting the wrong WAV bit depth in [`decode_wav`].
fn decode_l16(bytes: &[u8]) -> anyhow::Result<Vec<i16>> {
    if !bytes.len().is_multiple_of(2) {
        anyhow::bail!(
            "audio/L16 body has odd length {} bytes; s16le must be a whole number of samples",
            bytes.len()
        );
    }
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect())
}

/// Decode a WAV blob, rejecting anything that isn't already exactly
/// 16 kHz mono 16-bit PCM. Unlike
/// [`crate::voice::providers::wav_stream::decode_wav`] this deliberately
/// does not scale other bit depths into range: an 8-bit capture silently
/// reinterpreted as 16-bit is corrupted audio, not merely low-quality
/// audio, and the "16 kHz mono s16le only" constraint exists precisely to
/// keep that kind of reinterpretation from happening unnoticed.
fn decode_wav(bytes: &[u8]) -> anyhow::Result<Vec<i16>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes))?;
    let spec = reader.spec();
    if spec.channels != 1 {
        anyhow::bail!("wav must be mono, got {} channels", spec.channels);
    }
    if spec.sample_rate != PIPELINE_SAMPLE_RATE {
        anyhow::bail!(
            "wav must be {PIPELINE_SAMPLE_RATE} Hz, got {}",
            spec.sample_rate
        );
    }
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        anyhow::bail!(
            "wav must be 16-bit PCM, got {:?} at {} bits",
            spec.sample_format,
            spec.bits_per_sample
        );
    }
    Ok(reader.samples::<i16>().collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// Build a router plus the receiving end of the admission queue.
    fn harness() -> (Router, mpsc::Receiver<Segment>) {
        harness_with_queue_depth(16)
    }

    fn harness_with_queue_depth(depth: usize) -> (Router, mpsc::Receiver<Segment>) {
        harness_with(depth, true)
    }

    /// Build a `DeviceAuth` binding one device ("pendant") to a token
    /// ("sa-dev-good"), routed to room_profile "home". A static verifier
    /// stands in for the bridge, so the fixed bearer literal used
    /// throughout these tests keeps resolving.
    fn device_auth_fixture() -> Arc<crate::device_auth::DeviceAuth> {
        let mut room_profiles = std::collections::HashMap::new();
        room_profiles.insert(
            "home".to_string(),
            crate::config::RoomProfileConfig {
                profile: "sonnet".into(),
                devices: vec!["pendant".to_string()],
                ..Default::default()
            },
        );
        crate::device_auth::testing::device_auth(&[("sa-dev-good", "pendant")], &room_profiles)
    }

    fn harness_with(depth: usize, enabled: bool) -> (Router, mpsc::Receiver<Segment>) {
        let device_auth = device_auth_fixture();
        let (tx, rx) = mpsc::channel(depth);
        let cfg = AmbientConfig {
            enabled,
            ..Default::default()
        };
        let state = Arc::new(AmbientState::new(cfg, device_auth, tx));
        (routes(state), rx)
    }

    fn pcm_body(samples: usize) -> Body {
        Body::from(vec![0u8; samples * 2])
    }

    fn ingest_uri(segment: &str, live: bool) -> String {
        format!(
            "/audio/ingest?segment={segment}&started_at=1787000000000&rate=16000&live={}",
            if live { 1 } else { 0 }
        )
    }

    #[tokio::test]
    async fn accepts_a_well_formed_segment_and_enqueues_it() {
        let (app, mut rx) = harness();
        let res = app
            .oneshot(
                Request::post(ingest_uri("seg-1", true))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/L16")
                    .body(pcm_body(16_000))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let seg = rx.try_recv().expect("segment enqueued");
        assert_eq!(seg.segment, "seg-1");
        assert_eq!(
            seg.device, "pendant",
            "identity comes from the key, not the URL"
        );
        assert!(seg.live);
        assert_eq!(seg.pcm.len(), 16_000);
        assert_eq!(
            seg.started_at.timestamp_millis(),
            1_787_000_000_000,
            "started_at round-trips from the query param, in milliseconds"
        );
    }

    #[tokio::test]
    async fn a_repeated_segment_id_is_accepted_and_discarded() {
        let (app, mut rx) = harness();
        for _ in 0..2 {
            let res = app
                .clone()
                .oneshot(
                    Request::post(ingest_uri("seg-dup", false))
                        .header("authorization", "Bearer sa-dev-good")
                        .header("content-type", "audio/L16")
                        .body(pcm_body(1_600))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::OK,
                "replay is normal, not an error"
            );
        }
        assert!(rx.try_recv().is_ok(), "first delivery enqueued");
        assert!(rx.try_recv().is_err(), "duplicate not enqueued twice");
    }

    #[tokio::test]
    async fn rejects_an_unknown_bearer_with_401() {
        let (app, _rx) = harness();
        let res = app
            .oneshot(
                Request::post(ingest_uri("seg-2", false))
                    .header("authorization", "Bearer sa-dev-nope")
                    .header("content-type", "audio/L16")
                    .body(pcm_body(1_600))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_an_unsupported_content_type_with_415() {
        let (app, _rx) = harness();
        let res = app
            .oneshot(
                Request::post(ingest_uri("seg-3", false))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/opus")
                    .body(pcm_body(1_600))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "opus is the documented forward-compat seam, not a v1 format"
        );
    }

    #[tokio::test]
    async fn rejects_a_non_16k_rate_rather_than_resampling() {
        let (app, _rx) = harness();
        let res = app
            .oneshot(
                Request::post(
                    "/audio/ingest?segment=seg-4&started_at=1787000000000&rate=48000&live=0",
                )
                .header("authorization", "Bearer sa-dev-good")
                .header("content-type", "audio/L16")
                .body(pcm_body(1_600))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn answers_429_when_the_queue_is_full() {
        // Capacity 2: verified empirically (a throwaway probe test, removed
        // after use) that `mpsc::channel(N)` accepts exactly N `try_send`s
        // before `Full` — so two requests must land before the third
        // overflows. Asserting only the last request (as the original test
        // did) would also pass an implementation that answered 429
        // unconditionally; asserting the whole sequence pins down exactly
        // when the queue starts refusing.
        let (app, _rx) = harness_with_queue_depth(2);
        for i in 0..3 {
            let res = app
                .clone()
                .oneshot(
                    Request::post(ingest_uri(&format!("seg-q{i}"), false))
                        .header("authorization", "Bearer sa-dev-good")
                        .header("content-type", "audio/L16")
                        .body(pcm_body(1_600))
                        .unwrap(),
                )
                .await
                .unwrap();
            if i < 2 {
                assert_eq!(res.status(), StatusCode::OK, "request {i} should still fit");
            } else {
                assert_eq!(
                    res.status(),
                    StatusCode::TOO_MANY_REQUESTS,
                    "request {i} should overflow the queue"
                );
            }
        }
    }

    /// The spec tells the device to spool and retry after a 429, so the
    /// retry must actually deliver. `answers_429_when_the_queue_is_full`
    /// uses three *distinct* ids and so cannot see this: the id of the
    /// refused segment is what matters. If admission burns the id before
    /// the enqueue succeeds, the retry is answered `200 OK` as a
    /// "duplicate" and the audio is dropped while the device believes it
    /// was delivered — silent data loss in exactly the reconnect burst
    /// the bounded queue exists to absorb.
    #[tokio::test]
    async fn a_segment_refused_with_429_still_enqueues_on_retry() {
        let (app, mut rx) = harness_with_queue_depth(1);

        let ok = app
            .clone()
            .oneshot(
                Request::post(ingest_uri("seg-first", false))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/L16")
                    .body(pcm_body(1_600))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        let full = app
            .clone()
            .oneshot(
                Request::post(ingest_uri("seg-retried", false))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/L16")
                    .body(pcm_body(1_600))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(full.status(), StatusCode::TOO_MANY_REQUESTS);

        // The worker drains one; the device retries the segment it was
        // told to retry.
        assert_eq!(rx.try_recv().unwrap().segment, "seg-first");
        let retry = app
            .oneshot(
                Request::post(ingest_uri("seg-retried", false))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/L16")
                    .body(pcm_body(1_600))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::OK);
        assert_eq!(
            rx.try_recv()
                .expect("the retried segment must be enqueued, not discarded as a duplicate")
                .segment,
            "seg-retried"
        );
    }

    /// A body that fails to decode must not burn its id either: the
    /// device gets a 400 and may legitimately re-send the same segment
    /// once it has fixed its framing.
    #[tokio::test]
    async fn a_rejected_body_does_not_burn_its_segment_id() {
        let (app, mut rx) = harness();
        let bad = app
            .clone()
            .oneshot(
                Request::post(ingest_uri("seg-refix", false))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/L16")
                    .body(Body::from(vec![0u8; 1_601]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        let good = app
            .oneshot(
                Request::post(ingest_uri("seg-refix", false))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/L16")
                    .body(pcm_body(800))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(good.status(), StatusCode::OK);
        assert_eq!(
            rx.try_recv()
                .expect("a re-sent segment must be admitted after a decode failure")
                .segment,
            "seg-refix"
        );
    }

    #[tokio::test]
    async fn disabled_ambient_answers_404_rather_than_401() {
        // `[ambient].enabled = false` must be distinguishable from a bad
        // bearer: 404 says "this feature is off", 401 says "your
        // credentials are wrong" — a device probing for the feature
        // should not be told its key is bad.
        let (app, _rx) = harness_with(16, false);
        let res = app
            .oneshot(
                Request::post(ingest_uri("seg-disabled", false))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/L16")
                    .body(pcm_body(1_600))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_an_odd_length_l16_body_rather_than_dropping_the_last_byte() {
        let (app, _rx) = harness();
        let res = app
            .oneshot(
                Request::post(ingest_uri("seg-odd", false))
                    .header("authorization", "Bearer sa-dev-good")
                    .header("content-type", "audio/L16")
                    .body(Body::from(vec![0u8; 1_601])) // odd: not a whole number of i16 samples
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "a truncated s16le body must be rejected, not silently short by one sample"
        );
    }

    /// Build an in-memory WAV file at the given bit depth. 16-bit samples
    /// are widened/narrowed as `hound`'s own `Sample` impls do.
    fn wav_bytes(bits_per_sample: u16, samples: &[i16]) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: PIPELINE_SAMPLE_RATE,
            bits_per_sample,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            match bits_per_sample {
                8 => {
                    for &s in samples {
                        writer.write_sample((s >> 8) as i8).unwrap();
                    }
                }
                16 => {
                    for &s in samples {
                        writer.write_sample(s).unwrap();
                    }
                }
                other => panic!("test helper does not support {other}-bit WAVs"),
            }
            writer.finalize().unwrap();
        }
        cursor.into_inner()
    }

    #[tokio::test]
    async fn accepts_a_16_bit_mono_16k_wav_segment() {
        let (app, mut rx) = harness();
        let body = wav_bytes(16, &[0i16; 1_600]);
        let res = app
            .oneshot(
                Request::post(
                    "/audio/ingest?segment=seg-wav&started_at=1787000000000&rate=16000&live=0",
                )
                .header("authorization", "Bearer sa-dev-good")
                .header("content-type", "audio/wav")
                .body(Body::from(body))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let seg = rx.try_recv().expect("segment enqueued");
        assert_eq!(seg.pcm.len(), 1_600);
    }

    #[tokio::test]
    async fn rejects_an_8_bit_wav_rather_than_silently_reinterpreting_it() {
        // hound reads 8-bit PCM successfully (it errors only on 24- and
        // 32-bit input), widening each unsigned byte into an i16 without
        // rescaling to the 16-bit range. Accepting that silently would
        // enqueue corrupted audio under a 16-bit label — precisely what
        // "16 kHz mono s16le only" is meant to prevent.
        let (app, _rx) = harness();
        let body = wav_bytes(8, &[0i16; 1_600]);
        let res = app
            .oneshot(
                Request::post(
                    "/audio/ingest?segment=seg-wav8&started_at=1787000000000&rate=16000&live=0",
                )
                .header("authorization", "Bearer sa-dev-good")
                .header("content-type", "audio/wav")
                .body(Body::from(body))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "8-bit PCM must not be silently reinterpreted as 16-bit"
        );
    }

    #[tokio::test]
    async fn hello_reports_ingest_parameters_without_being_required() {
        let (app, _rx) = harness();
        let res = app
            .oneshot(
                Request::post("/audio/hello")
                    .header("authorization", "Bearer sa-dev-good")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["device"], "pendant");
        assert_eq!(v["sample_rate"], 16000);
        assert_eq!(v["accepts"][0], "audio/L16");
        assert_eq!(v["downlink"], false, "reserved for S4, not implemented");
    }
}
