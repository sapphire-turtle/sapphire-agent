//! Concrete STT/TTS provider implementations.
//!
//! Local providers (sherpa-onnx STT/TTS) are gated behind the
//! `voice-sherpa` cargo feature so the default build doesn't pay
//! sherpa-onnx-sys's C++ compile cost. Mock providers and the
//! OpenAI TTS client (HTTP-only, works against the public endpoint
//! or any self-hosted server that speaks the same shape) are
//! always available.

mod mock;
mod openai_tts;
#[cfg(feature = "voice-sherpa")]
pub(crate) mod sherpa_download;
#[cfg(feature = "voice-sherpa")]
mod sherpa_stt;
#[cfg(feature = "voice-sherpa")]
mod sherpa_tts;
mod wav_stream;

// `MockStt` is re-exported at `pub(crate)` (not `pub(super)` like the
// rest of this module) because `crate::voice::mod.rs` re-exports it a
// second hop out for `ambient::worker`'s tests. A `pub(super) use` here
// caps MockStt's effective visibility at "visible in `voice`"; a
// `pub(crate) use` one level up cannot widen that after the fact — it
// can only forward visibility the item already has. `MockTts` has no
// such second-hop caller, so it keeps the tighter `pub(super)`.
pub(crate) use mock::MockStt;
pub(super) use mock::MockTts;
pub(super) use openai_tts::OpenAiTts;
#[cfg(feature = "voice-sherpa")]
pub(super) use sherpa_stt::SherpaOnnxStt;
#[cfg(feature = "voice-sherpa")]
pub(super) use sherpa_tts::SherpaOnnxTts;
