//! The `init` subcommand: seed a workspace with the files the agent reads.
//!
//! The read side of a workspace is spread across several modules — [`crate::workspace`]
//! for the root Markdown files, [`crate::periodic_log`] for the memory tree,
//! [`crate::agents`] and [`crate::heartbeat_config`] for the two definition
//! directories, and [`crate::config`] for the `.sapphire-agent/` layer. This module is the one place that writes them, so [`TEMPLATES`] doubles as the
//! inventory of what a workspace is made of.
//!
//! Seeding never overwrites. A workspace that already has a file keeps it,
//! which makes `init` safe to re-run against a workspace that has grown since
//! and lets a new file added to the list reach existing workspaces.

use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

/// Workspace-relative destination -> template body.
///
/// Compiled in rather than read from disk so a released binary can seed a
/// workspace with no support files beside it.
const TEMPLATES: &[(&str, &str)] = &[
    (
        "AGENTS.md",
        include_str!("../templates/workspace/AGENTS.md"),
    ),
    ("SOUL.md", include_str!("../templates/workspace/SOUL.md")),
    (
        "IDENTITY.md",
        include_str!("../templates/workspace/IDENTITY.md"),
    ),
    ("USER.md", include_str!("../templates/workspace/USER.md")),
    ("TOOLS.md", include_str!("../templates/workspace/TOOLS.md")),
    (
        "BOOTSTRAP.md",
        include_str!("../templates/workspace/BOOTSTRAP.md"),
    ),
    (
        "memory/default/MEMORY.md",
        include_str!("../templates/workspace/MEMORY.md"),
    ),
    (
        ".sapphire-agent/config.toml",
        include_str!("../templates/workspace/config.toml"),
    ),
    (
        "agents/example-reviewer.md",
        include_str!("../templates/workspace/example-reviewer.md"),
    ),
    (
        "heartbeat/example-morning.md",
        include_str!("../templates/workspace/example-morning.md"),
    ),
    (
        "autonomous/example-autonomous.md",
        include_str!("../templates/workspace/example-autonomous.md"),
    ),
];

/// Directories created empty. The periodic-log writers fill them at their own
/// cadence, but an operator reading a fresh workspace should be able to see
/// the shape of the memory tree before anything has been written into it.
const EMPTY_DIRS: &[&str] = &[
    "memory/default/daily",
    "memory/default/weekly",
    "memory/default/monthly",
    "memory/default/yearly",
];

/// What one `init` run did. Paths are workspace-relative, `/`-separated.
#[derive(Debug, Default, PartialEq)]
pub struct SeedReport {
    pub created: Vec<String>,
    pub skipped: Vec<String>,
}

/// Create every workspace file that is missing under `root`, leaving every
/// existing one untouched.
pub fn seed_workspace(root: &Path) -> Result<SeedReport> {
    let mut report = SeedReport::default();

    for (rel, body) in TEMPLATES {
        let path = root.join(rel);
        if path.exists() {
            report.skipped.push((*rel).to_string());
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, body)
            .with_context(|| format!("failed to write {}", path.display()))?;
        report.created.push((*rel).to_string());
    }

    for rel in EMPTY_DIRS {
        let path = root.join(rel);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
    }

    Ok(report)
}

/// The host-local TOML `init` prints for the operator to paste. This command
/// deliberately writes no file outside the workspace — credentials and machine
/// paths are the host's to place — so the snippet is the whole handover.
pub fn host_config_snippet(workspace_dir: &Path) -> String {
    // Through `toml::Value` rather than `format!("{:?}")` so a Windows path's
    // backslashes are escaped the way TOML wants them, not the way Rust's
    // debug formatter happens to.
    let dir = toml::Value::String(workspace_dir.display().to_string());
    format!(
        "# Host-local configuration for sapphire-agent.\n\
         #\n\
         # Everything the workspace may not carry: the API key, bind addresses,\n\
         # MCP servers, channel credentials, and where the workspace itself is.\n\
         # See config.example.toml for the rest.\n\
         \n\
         workspace_dir = {dir}\n\
         \n\
         [anthropic]\n\
         api_key = \"sk-ant-...\"\n"
    )
}

/// Seed the workspace at `path` (default: the current directory), then tell the
/// operator what happened and what is still theirs to write.
pub fn run(path: Option<PathBuf>) -> Result<()> {
    let target = path.unwrap_or_else(|| PathBuf::from("."));
    if !target.exists() {
        std::fs::create_dir_all(&target)
            .with_context(|| format!("failed to create {}", target.display()))?;
    }
    // `absolute` rather than `canonicalize`: the path is going into a config
    // file a human will read and edit, and canonicalize hands back Windows'
    // `\?\C:\...` verbatim form.
    let root = std::path::absolute(&target)
        .with_context(|| format!("failed to resolve {}", target.display()))?;

    let report = seed_workspace(&root)?;

    println!("Workspace: {}", root.display());
    for rel in &report.created {
        println!("  created  {rel}");
    }
    for rel in &report.skipped {
        println!("  kept     {rel}");
    }
    if report.created.is_empty() {
        println!("  (everything was already there)");
    }

    if report.created.iter().any(|r| r == "BOOTSTRAP.md") {
        println!();
        println!("BOOTSTRAP.md drives the first conversation: the agent asks what to");
        println!("call it, writes its answers into IDENTITY.md and SOUL.md, then");
        println!("deletes the file. The other templates are yours to fill in.");
    }

    println!();
    println!("`init` does not write the host-local config — it holds credentials");
    println!("and machine paths. Put this at");
    println!("{}", crate::config::Config::default_path().display());
    println!("(or pass --config), then run `sapphire-agent verify`:");
    println!();
    print!("{}", host_config_snippet(&root));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file the agent's readers look for, plus the two example
    /// definitions. If a reader learns a new filename, this list is where
    /// the scaffolder finds out about it.
    const EXPECTED_FILES: &[&str] = &[
        "AGENTS.md",
        "SOUL.md",
        "IDENTITY.md",
        "USER.md",
        "TOOLS.md",
        "BOOTSTRAP.md",
        "memory/default/MEMORY.md",
        ".sapphire-agent/config.toml",
        "agents/example-reviewer.md",
        "heartbeat/example-morning.md",
        "autonomous/example-autonomous.md",
    ];

    #[test]
    fn a_fresh_directory_gets_every_workspace_file() {
        let d = tempfile::tempdir().unwrap();

        let report = seed_workspace(d.path()).unwrap();

        for rel in EXPECTED_FILES {
            assert!(
                d.path().join(rel).is_file(),
                "{rel} was not created; created = {:?}",
                report.created
            );
        }
        assert!(report.skipped.is_empty());
    }

    /// Seeding is how a *new* file reaches an *old* workspace, so `init` has
    /// to be safe to re-run. Anything the user has since made their own must
    /// survive it untouched.
    #[test]
    fn a_second_run_keeps_what_the_user_wrote_and_reports_it_as_skipped() {
        let d = tempfile::tempdir().unwrap();
        seed_workspace(d.path()).unwrap();
        let soul = d.path().join("SOUL.md");
        std::fs::write(&soul, "# SOUL.md\n\nMine now.\n").unwrap();

        let report = seed_workspace(d.path()).unwrap();

        assert!(report.created.is_empty(), "created = {:?}", report.created);
        assert_eq!(report.skipped.len(), EXPECTED_FILES.len());
        assert_eq!(
            std::fs::read_to_string(&soul).unwrap(),
            "# SOUL.md\n\nMine now.\n"
        );
    }

    /// True of a `# key = value` or `# [table]` line — a setting the template
    /// documents by commenting out — and false of the prose around them.
    fn is_setting_line(s: &str) -> bool {
        if s.starts_with('[') && s.ends_with(']') {
            return true;
        }
        match s.split_once(" = ") {
            Some((key, _)) => {
                !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            }
            None => false,
        }
    }

    fn uncomment_settings(raw: &str) -> String {
        raw.lines()
            .map(|line| match line.trim_start().strip_prefix("# ") {
                Some(rest) if is_setting_line(rest) => rest.to_string(),
                _ => line.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The template tells the operator which settings this layer may set, and
    /// the allowlist decides. A key documented here but refused there would be
    /// silently dropped with a startup warning — so the template is checked
    /// against the allowlist rather than against a copy of it.
    #[test]
    fn every_setting_the_workspace_config_documents_is_one_that_layer_may_set() {
        let d = tempfile::tempdir().unwrap();
        seed_workspace(d.path()).unwrap();
        let raw = std::fs::read_to_string(d.path().join(".sapphire-agent/config.toml")).unwrap();

        let doc: toml::Value = toml::from_str(&uncomment_settings(&raw))
            .expect("the template's documented settings should parse as TOML");
        let (_, rejected) = crate::config_layer::filter_allowed(doc);

        assert!(
            rejected.is_empty(),
            "the workspace layer may not set: {rejected:?}"
        );
    }

    /// The two example definitions are the only documentation of their file
    /// format that a fresh workspace carries. If the loaders stop accepting
    /// them, the examples teach a shape that no longer works.
    #[test]
    fn the_example_definitions_load_through_the_parsers_that_read_them() {
        let d = tempfile::tempdir().unwrap();
        seed_workspace(d.path()).unwrap();

        let agents = crate::agents::load_agents_dir(&d.path().join("agents"));
        assert_eq!(agents.len(), 1, "the example subagent should load");
        assert!(!agents[0].description.is_empty());

        let tasks = crate::heartbeat_config::load_heartbeat_dir(&d.path().join("heartbeat"));
        assert_eq!(tasks.len(), 1, "the example heartbeat task should load");
        assert!(
            tasks[0].parsed_schedule().is_some(),
            "its cron should parse"
        );
        assert!(
            !tasks[0].meta.enabled,
            "a seeded example must not fire on its own"
        );
    }

    /// Seeding a file the prompt builder does not read would be busywork, and
    /// a heading it reads from a file we do not seed is a gap. This asserts
    /// the two lists agree.
    #[tokio::test]
    async fn every_seeded_file_reaches_the_system_prompt() {
        let d = tempfile::tempdir().unwrap();
        seed_workspace(d.path()).unwrap();
        let digest = toml::from_str("").unwrap();

        let ws = crate::workspace::Workspace::new(d.path().to_path_buf(), digest);
        let prompt = ws
            .build_system_prompt(None, 4, &["default".to_string()], None, None)
            .await;

        for heading in [
            "# Agent Instructions",
            "# Soul",
            "# Identity",
            "# User",
            "# Tools",
            "# Bootstrap",
            "# Memory",
        ] {
            assert!(prompt.contains(heading), "prompt is missing {heading}");
        }
    }

    /// The snippet is the one thing `init` hands over for a file it refuses to
    /// write itself. If it does not parse, or does not point at the workspace
    /// just created, it is worse than printing nothing.
    #[test]
    fn the_host_config_snippet_parses_and_points_at_the_new_workspace() {
        let d = tempfile::tempdir().unwrap();

        let snippet = host_config_snippet(d.path());

        let doc: toml::Value = toml::from_str(&snippet).expect("the snippet should parse as TOML");
        let dir = doc
            .get("workspace_dir")
            .and_then(|v| v.as_str())
            .expect("the snippet should set workspace_dir");
        assert_eq!(std::path::Path::new(dir), d.path());
        assert!(
            doc.get("anthropic")
                .and_then(|a| a.get("api_key"))
                .is_some(),
            "the snippet should carry the api_key the workspace layer may not set"
        );
    }
}
