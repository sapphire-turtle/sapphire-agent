//! Per-device segment routing.
//!
//! Only [`Disposition::RecordOnly`] is reachable today: the worker pins
//! every device to [`DeviceState::Idle`]. The fork exists now because S4
//! (server-side wake word, live conversation) changes what a segment
//! *means*, and introducing that decision point later would mean threading
//! it through the whole pipeline. Adding a branch to an existing fork is
//! cheap; creating the fork is not.

use super::ingest::Segment;

/// What a device is currently doing. Pinned to `Idle` until S4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Idle,
    // Constructed only by S4 (server-side wake word). No caller sets this
    // yet; delete this attribute once S4 adds one.
    #[allow(dead_code)]
    Conversing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Transcribe and store. No LLM turn.
    RecordOnly,
    /// Transcribe, store, and feed the conversation.
    // Returned only once a device can reach `DeviceState::Conversing`,
    // which is S4's job. Delete this attribute once that path exists.
    #[allow(dead_code)]
    RecordAndConverse,
}

/// Decide what to do with `seg`.
///
/// `live` gates the conversation branch unconditionally. This is the
/// safety property the explicit flag exists for: the agent must never
/// answer something said six hours ago because the recording of it just
/// arrived.
pub fn route(seg: &Segment, state: DeviceState) -> Disposition {
    if !seg.live {
        return Disposition::RecordOnly;
    }
    match state {
        DeviceState::Idle => Disposition::RecordOnly,
        DeviceState::Conversing => Disposition::RecordAndConverse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn seg(live: bool) -> crate::ambient::ingest::Segment {
        crate::ambient::ingest::Segment {
            segment: "seg".into(),
            device: "pendant".into(),
            started_at: Utc::now(),
            live,
            pcm: vec![0; 16_000],
        }
    }

    #[test]
    fn replayed_audio_never_reaches_the_conversation_branch() {
        // Even if the device were somehow marked Conversing, audio recorded
        // hours ago must not be answered.
        assert_eq!(
            route(&seg(false), DeviceState::Conversing),
            Disposition::RecordOnly
        );
    }

    #[test]
    fn live_audio_from_an_idle_device_is_recorded_only() {
        assert_eq!(
            route(&seg(true), DeviceState::Idle),
            Disposition::RecordOnly
        );
    }

    #[test]
    fn live_audio_from_a_conversing_device_also_converses() {
        assert_eq!(
            route(&seg(true), DeviceState::Conversing),
            Disposition::RecordAndConverse
        );
    }
}
