//! Always-on ambient audio capture ingest.
//!
//! Deliberately separate from [`crate::voice`]. `voice` is the
//! **interactive** pipeline: audio in, LLM turn, audio out. `ambient`
//! **records without answering** — nothing in this module may start an
//! LLM turn. See
//! `docs/superpowers/specs/2026-08-26-ambient-audio-ingest-design.md`.

pub mod audio;
pub mod cache;
pub mod ingest;
pub mod models;
pub mod router;
pub mod speaker;
pub mod startup;
pub mod transcript;
pub mod worker;
