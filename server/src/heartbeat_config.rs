//! Heartbeat task definitions loaded from `<workspace>/heartbeat/*.md`.
//!
//! Each task file is a Markdown document with a YAML frontmatter block:
//!
//! ```markdown
//! ---
//! schedule: "0 8 * * *"
//! room_id: "..."          # optional chat target, defaults to channel default
//! device: "laptop"         # optional: the host that runs it, when synced
//! voice:                   # optional voice satellite target
//!   device_id: "01J..."   # the satellite's device id (see sapphire-agent-cli)
//! enabled: true            # optional, default true
//! ---
//!
//! # Morning Call
//! ...body...
//! ```
//!
//! When both `voice:` and `room_id` are set, the cron loop tries the
//! voice satellite first and falls back to the chat room if the
//! satellite is offline. The body is used verbatim as the trigger
//! prompt fed to the agent.

use chrono::{DateTime, Local};
use cron::Schedule;
use serde::Deserialize;
use std::path::Path;
use std::str::FromStr;
use tracing::warn;

#[derive(Debug, Deserialize)]
pub struct HeartbeatTaskMeta {
    /// Cron expression. The `cron` crate uses 6- or 7-field syntax (with seconds).
    /// We accept the standard 5-field form and prefix `"0 "` for seconds.
    pub schedule: String,
    #[serde(default)]
    pub room_id: Option<String>,
    /// The device that runs this task, by name or id, when the workspace
    /// is synced with other hosts. Without it the task runs where it can be
    /// delivered (see `placement::heartbeat_runner`).
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional voice satellite target. When set, the cron loop tries
    /// to push the rendered TTS audio to the matching `voice/subscribe`
    /// subscriber. A satellite that's offline causes a fall-through to
    /// `room_id` when both are set; otherwise the fire is logged and
    /// dropped.
    #[serde(default)]
    pub voice: Option<HeartbeatVoiceTarget>,
}

/// Voice satellite target, identified by `device_id`. The
/// `room_profile` to use is reverse-looked-up from the active
/// `voice/subscribe` registration, so heartbeat tasks don't have to
/// duplicate it. A device only ever has one active voice session at a
/// time, which makes the lookup unambiguous.
#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatVoiceTarget {
    pub device_id: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug)]
pub struct HeartbeatTask {
    /// File stem (e.g. `morning_call`).
    pub name: String,
    pub meta: HeartbeatTaskMeta,
    /// Markdown body — used verbatim as the trigger prompt.
    pub body: String,
}

impl HeartbeatTask {
    /// Parse the cron schedule, normalising 5-field standard cron to the
    /// 6-field form expected by the `cron` crate (which has seconds first).
    pub fn parsed_schedule(&self) -> Option<Schedule> {
        let raw = self.meta.schedule.trim();
        let normalised = normalise_cron(raw);
        match Schedule::from_str(&normalised) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(
                    "heartbeat task {}: invalid schedule {:?}: {e}",
                    self.name, raw
                );
                None
            }
        }
    }

    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        self.parsed_schedule()?.after(&after).next()
    }
}

/// Convert a standard 5-field cron expression to the 6-field form (with leading
/// `0 ` seconds) used by the `cron` crate. Pass through `@`-shortcuts and
/// already-6/7-field forms unchanged.
fn normalise_cron(raw: &str) -> String {
    if raw.starts_with('@') {
        return raw.to_string();
    }
    let n_fields = raw.split_whitespace().count();
    if n_fields == 5 {
        format!("0 {raw}")
    } else {
        raw.to_string()
    }
}

/// Load all heartbeat tasks from `<workspace>/heartbeat/*.md`.
/// Returns an empty vector if the directory does not exist.
pub fn load_heartbeat_dir(dir: &Path) -> Vec<HeartbeatTask> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                warn!("failed to read heartbeat task {}: {e}", path.display());
                continue;
            }
        };
        match parse_task(name, &raw) {
            Some(t) => tasks.push(t),
            None => warn!(
                "heartbeat task {} skipped (no/invalid frontmatter)",
                path.display()
            ),
        }
    }
    tasks
}

fn parse_task(name: String, raw: &str) -> Option<HeartbeatTask> {
    let (fm, body) = crate::frontmatter::split(raw)?;
    let meta: HeartbeatTaskMeta = match serde_yaml::from_str(fm) {
        Ok(m) => m,
        Err(e) => {
            warn!("heartbeat task {name}: yaml parse error: {e}");
            return None;
        }
    };
    Some(HeartbeatTask {
        name,
        meta,
        body: body.trim_start_matches(['\n', '\r']).to_string(),
    })
}

/// Parse one definition the way the loader does, but hand the failure
/// back instead of skipping the file.
///
/// `load_heartbeat_dir` swallows a broken file on purpose — one typo
/// must not take the other tasks down with it — but a caller that is
/// *about to write* the file has the opposite need: it must refuse what
/// the loader would silently drop, or the model writes a task that never
/// fires and cannot tell why.
///
/// A task whose `schedule:` does not parse is *not* refused here: the
/// loader keeps it and `next_due` skips it, which is a different
/// failure. `ConfigTool::validate` adds that check where it matters.
pub fn parse_definition(name: &str, raw: &str) -> Result<HeartbeatTask, String> {
    // `split` only for the message: no frontmatter at all is the one case
    // worth naming precisely, since that is what a model gets wrong when
    // it writes a bare markdown file.
    crate::frontmatter::split(raw)
        .ok_or_else(|| "no YAML frontmatter: the file must start with a `---` line".to_string())?;
    parse_task(name.to_string(), raw).ok_or_else(|| {
        "cannot be parsed as a task: check the frontmatter YAML and that the body is not empty"
            .to_string()
    })
}

/// From a list of tasks, find the next due time and the tasks scheduled for it.
/// Tasks scheduled within a 1-second window are batched together.
pub fn next_due(
    tasks: &[HeartbeatTask],
    now: DateTime<Local>,
) -> Option<(DateTime<Local>, Vec<&HeartbeatTask>)> {
    let mut earliest: Option<DateTime<Local>> = None;
    let mut next_for: Vec<(DateTime<Local>, &HeartbeatTask)> = Vec::new();
    for t in tasks {
        if let Some(next) = t.next_after(now) {
            next_for.push((next, t));
            earliest = Some(match earliest {
                Some(e) if e <= next => e,
                _ => next,
            });
        }
    }
    let earliest = earliest?;
    let due: Vec<&HeartbeatTask> = next_for
        .into_iter()
        .filter(|(t, _)| (*t - earliest).num_seconds().abs() <= 1)
        .map(|(_, t)| t)
        .collect();
    Some((earliest, due))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_definition_reports_what_the_loader_would_skip() {
        assert!(parse_definition("morning", "---\nschedule: \"0 8 * * *\"\n---\nHi\n").is_ok());
        let err = parse_definition("broken", "# no frontmatter\n").unwrap_err();
        assert!(err.contains("frontmatter"), "{err}");
        assert!(parse_definition("broken", "---\nschedule: [oops\n---\nHi\n").is_err());
    }

    #[test]
    fn parse_basic() {
        let raw = "---\nschedule: \"0 8 * * *\"\n---\n\nHello body\n";
        let task = parse_task("morning".to_string(), raw).unwrap();
        assert_eq!(task.meta.schedule, "0 8 * * *");
        assert_eq!(task.body, "Hello body\n");
        assert!(task.meta.enabled);
        assert!(task.parsed_schedule().is_some());
    }

    #[test]
    fn parse_with_room() {
        let raw =
            "---\nschedule: \"@hourly\"\nroom_id: \"!room:example\"\nenabled: false\n---\nbody";
        let task = parse_task("t".to_string(), raw).unwrap();
        assert_eq!(task.meta.room_id.as_deref(), Some("!room:example"));
        assert!(!task.meta.enabled);
        assert!(task.meta.voice.is_none());
    }

    #[test]
    fn parse_with_voice_target() {
        // NB: raw string — line-continuation `\` would otherwise eat
        // the leading indentation under `voice:` and reparent the
        // fields to the top level.
        let raw = r#"---
schedule: "0 7 * * *"
room_id: "!chat:example"
voice:
  device_id: "01J..."
---
body"#;
        let task = parse_task("morning".to_string(), raw).unwrap();
        let voice = task.meta.voice.as_ref().unwrap();
        assert_eq!(voice.device_id, "01J...");
        // room_id remains as the chat fallback target.
        assert_eq!(task.meta.room_id.as_deref(), Some("!chat:example"));
    }

    #[test]
    fn no_frontmatter() {
        assert!(parse_task("x".to_string(), "no frontmatter here").is_none());
    }
}
