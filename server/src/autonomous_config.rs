//! Autonomous task definitions, loaded from `<workspace>/autonomous/*.md`.
//!
//! The third user of the shape `<workspace>/heartbeat/*.md` established
//! and `<workspace>/agents/*.md` followed: YAML frontmatter for the
//! control fields, the body for the instruction. The body is the whole
//! first user message of every turn the task runs.
//!
//! Unlike the other two, a task is not a one-shot prompt or a delegate:
//! it is a unit of *work* that may span several turns across several
//! nights. State that outlives a turn — cooldown, how many turns have
//! been spent — is derived from the session store rather than kept here
//! (see the design doc, decisions 3 and 5), so a definition file holds
//! only what a human decides.

use serde::Deserialize;
use std::path::Path;
use tracing::warn;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutonomousTaskMeta {
    #[serde(default = "default_true")]
    enabled: bool,
    /// Lower runs first. Default 100, so an un-tuned task queues behind
    /// the ones someone thought about.
    #[serde(default = "default_priority")]
    priority: i64,
    /// Days that must pass after the task's last activity before it is
    /// due again. `0` means "as soon as the previous session ended".
    #[serde(default)]
    cooldown_days: u32,
    /// Turns one session may spend before it is closed. Raised to 1 when
    /// declared as 0.
    #[serde(default = "default_max_turns")]
    max_turns: usize,
}

fn default_true() -> bool {
    true
}

fn default_priority() -> i64 {
    100
}

fn default_max_turns() -> usize {
    3
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutonomousTask {
    /// The file stem, which is also the session's `room_id`.
    pub name: String,
    pub enabled: bool,
    pub priority: i64,
    pub cooldown_days: u32,
    pub max_turns: usize,
    /// The Markdown body, used verbatim as the instruction.
    pub body: String,
}

/// Load every task under `dir`, skipping the ones that cannot be read.
/// A missing directory is no tasks, not an error.
pub fn load_autonomous_dir(dir: &Path) -> Vec<AutonomousTask> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to read autonomous task {}: {e}", path.display());
                continue;
            }
        };
        match parse_task(name.to_string(), &raw) {
            Some(t) => out.push(t),
            None => warn!(
                "autonomous task {} skipped (no/invalid frontmatter, or an empty body)",
                path.display()
            ),
        }
    }
    out.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

fn parse_task(name: String, raw: &str) -> Option<AutonomousTask> {
    let (fm, body) = crate::frontmatter::split(raw)?;
    let meta: AutonomousTaskMeta = match serde_norway::from_str(fm) {
        Ok(m) => m,
        Err(e) => {
            warn!("autonomous task {name}: yaml parse error: {e}");
            return None;
        }
    };
    let body = body.trim_start_matches(['\n', '\r']).to_string();
    // An instruction-less task would burn a session and every turn it
    // allows on nothing.
    if body.trim().is_empty() {
        return None;
    }
    Some(AutonomousTask {
        name,
        enabled: meta.enabled,
        priority: meta.priority,
        cooldown_days: meta.cooldown_days,
        max_turns: meta.max_turns.max(1),
        body,
    })
}

/// Parse one definition the way the loader does, but hand the failure
/// back instead of skipping the file.
///
/// `load_autonomous_dir` swallows a broken file on purpose — one typo
/// must not take the other tasks down with it — but a caller that is
/// *about to write* the file has the opposite need: it must refuse what
/// the loader would silently drop, or the model writes a task that never
/// fires and cannot tell why.
pub fn parse_definition(name: &str, raw: &str) -> Result<AutonomousTask, String> {
    // `split` only for the message: no frontmatter at all is the one case
    // worth naming precisely, since that is what a model gets wrong when
    // it writes a bare markdown file.
    crate::frontmatter::split(raw)
        .ok_or_else(|| "no YAML frontmatter: the file must start with a `---` line".to_string())?;
    parse_task(name.to_string(), raw).ok_or_else(|| {
        "cannot be parsed as an autonomous task: check the frontmatter YAML and that the body is not empty"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_definition_reports_what_the_loader_would_skip() {
        assert!(parse_definition("journal", "---\npriority: 50\n---\nWrite it.\n").is_ok());
        assert!(parse_definition("journal", "---\npriority: 50\n---\n\n").is_err()); // empty body
    }

    fn write(dir: &Path, name: &str, raw: &str) {
        std::fs::write(dir.join(name), raw).unwrap();
    }

    #[test]
    fn a_missing_directory_is_no_tasks() {
        let d = tempfile::tempdir().unwrap();
        assert!(load_autonomous_dir(&d.path().join("nope")).is_empty());
    }

    /// Every field has a default, so a body-only task is a valid task.
    #[test]
    fn the_defaults_match_the_spec() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "journal.md", "---\n---\n\nDo the thing.\n");

        let tasks = load_autonomous_dir(d.path());
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.name, "journal");
        assert!(t.enabled);
        assert_eq!(t.priority, 100);
        assert_eq!(t.cooldown_days, 0);
        assert_eq!(t.max_turns, 3);
        assert_eq!(t.body, "Do the thing.\n");
    }

    #[test]
    fn the_frontmatter_is_parsed_onto_the_task() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "refactor.md",
            "---\nenabled: false\npriority: 10\ncooldown_days: 30\nmax_turns: 5\n---\nRefactor.\n",
        );

        let tasks = load_autonomous_dir(d.path());
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert!(!t.enabled);
        assert_eq!(t.priority, 10);
        assert_eq!(t.cooldown_days, 30);
        assert_eq!(t.max_turns, 5);
    }

    /// `max_turns: 0` would mean "make a session and close it without a
    /// turn", which is not a thing anyone wants.
    #[test]
    fn max_turns_zero_is_raised_to_one() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "t.md", "---\nmax_turns: 0\n---\nBody.\n");
        assert_eq!(load_autonomous_dir(d.path())[0].max_turns, 1);
    }

    /// A broken file is skipped with a warning, and the rest still load.
    #[test]
    fn a_broken_definition_does_not_take_the_others_with_it() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "good.md", "---\n---\nGood.\n");
        write(d.path(), "bad.md", "no frontmatter at all\n");
        write(d.path(), "typo.md", "---\npriorit: 1\n---\nTypo.\n");
        write(d.path(), "empty.md", "---\n---\n");
        write(d.path(), "notes.txt", "ignored");

        let tasks = load_autonomous_dir(d.path());
        assert_eq!(
            tasks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["good"]
        );
    }

    /// `priority` alone is not an order — two tasks at the same priority
    /// must not depend on `read_dir`.
    #[test]
    fn tasks_come_back_sorted_by_priority_then_name() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "b.md", "---\npriority: 10\n---\nB.\n");
        write(d.path(), "a.md", "---\npriority: 10\n---\nA.\n");
        write(d.path(), "z.md", "---\npriority: 1\n---\nZ.\n");

        let names: Vec<String> = load_autonomous_dir(d.path())
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["z", "a", "b"]);
    }
}
