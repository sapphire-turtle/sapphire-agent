//! Subagent definitions, loaded from `<workspace>/agents/*.md`.
//!
//! Same shape as `<workspace>/heartbeat/*.md`: YAML frontmatter for the
//! metadata, the body for the prompt. Reusing that convention rather
//! than inventing a second one is the whole reason for the file layout.
//!
//! A definition's `description` is load-bearing in a way the others are
//! not: it is what the parent model reads to decide whether to delegate,
//! and it is the only thing it sees before choosing.

use serde::Deserialize;
use std::path::Path;
use tracing::warn;

#[derive(Debug, Clone, Deserialize)]
struct AgentMeta {
    description: String,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    subagents: Option<Vec<String>>,
    /// Name of the `[profiles.<name>]` entry this agent runs on. `None`
    /// means "the parent turn's provider, unchanged" — the default and
    /// the pre-feature behaviour.
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    /// The file stem — what the model passes as `agent`.
    pub name: String,
    pub description: String,
    /// `None` means "whatever the parent can see". `Some(vec![])` means
    /// no tools at all, which is a legitimate definition.
    pub tools: Option<Vec<String>>,
    /// `None` means "inherit whatever the delegating turn can see".
    /// `Some(vec![])` means never delegate further, which is a
    /// legitimate definition.
    pub subagents: Option<Vec<String>>,
    /// The body, which becomes the whole system prompt.
    pub prompt: String,
    /// The name in `[profiles.<n>]` this agent runs on, or `None` for
    /// "the parent's provider". The name is validated against
    /// `Config::profiles` at startup (`Config::validate_subagent_profiles`)
    /// — a server config with an unknown name fails to boot rather than
    /// silently running the agent on a different model.
    pub profile: Option<String>,
}

/// Load every definition under `dir`, skipping the ones that cannot be
/// read. A missing directory is no agents, not an error: an operator
/// who has not created any is in a normal state.
pub fn load_agents_dir(dir: &Path) -> Vec<AgentDef> {
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
                warn!("failed to read agent definition {}: {e}", path.display());
                continue;
            }
        };
        match parse_agent(name.to_string(), &raw) {
            Some(a) => out.push(a),
            None => warn!(
                "agent definition {} skipped (no/invalid frontmatter, or no description)",
                path.display()
            ),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn parse_agent(name: String, raw: &str) -> Option<AgentDef> {
    let (fm, body) = crate::frontmatter::split(raw)?;
    let meta: AgentMeta = match serde_norway::from_str(fm) {
        Ok(m) => m,
        Err(e) => {
            warn!("agent {name}: yaml parse error: {e}");
            return None;
        }
    };
    Some(AgentDef {
        name,
        description: meta.description,
        tools: meta.tools,
        subagents: meta.subagents,
        prompt: body.trim_start_matches(['\n', '\r']).to_string(),
        profile: meta.profile,
    })
}

/// Parse one definition the way the loader does, but hand the failure
/// back instead of skipping the file.
///
/// `load_agents_dir` swallows a broken file on purpose — one typo must
/// not take the other definitions down with it — but a caller that is
/// *about to write* the file has the opposite need: it must refuse what
/// the loader would silently drop, or the model writes an agent that is
/// never offered for delegation and cannot tell why.
pub fn parse_definition(name: &str, raw: &str) -> Result<AgentDef, String> {
    // `split` only for the message: no frontmatter at all is the one case
    // worth naming precisely, since that is what a model gets wrong when
    // it writes a bare markdown file.
    crate::frontmatter::split(raw)
        .ok_or_else(|| "no YAML frontmatter: the file must start with a `---` line".to_string())?;
    parse_agent(name.to_string(), raw).ok_or_else(|| {
        "cannot be parsed as an agent definition: `description` is required and the frontmatter must be valid YAML"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_definition_reports_what_the_loader_would_skip() {
        assert!(parse_definition("reviewer", "---\ntools: []\n---\nReview.\n").is_err()); // no description
        let def =
            parse_definition("reviewer", "---\ndescription: Reviews.\n---\nReview.\n").unwrap();
        assert_eq!(
            (def.name.as_str(), def.prompt.as_str()),
            ("reviewer", "Review.\n")
        );
    }

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_definition_splits_into_frontmatter_and_prompt() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "reviewer.md",
            "---\ndescription: Reviews a diff.\ntools: [file_read]\n---\nYou are a reviewer.\n",
        );

        let agents = load_agents_dir(d.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "reviewer");
        assert_eq!(agents[0].description, "Reviews a diff.");
        assert_eq!(
            agents[0].tools.as_deref(),
            Some(["file_read".to_string()].as_slice())
        );
        assert_eq!(agents[0].prompt.trim(), "You are a reviewer.");
    }

    /// `tools` absent means "inherit what the parent can see", which is
    /// a different thing from an empty list.
    #[test]
    fn an_omitted_tools_list_is_none_not_empty() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "helper.md",
            "---\ndescription: Thinks.\n---\nThink.\n",
        );

        let agents = load_agents_dir(d.path());
        assert_eq!(agents[0].tools, None);
    }

    /// An empty list is a valid definition: an agent with no tools
    /// answers from its prompt alone, which is enough for a summary or
    /// a judgement.
    #[test]
    fn an_empty_tools_list_is_kept_as_empty() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "judge.md",
            "---\ndescription: Judges.\ntools: []\n---\nJudge.\n",
        );

        let agents = load_agents_dir(d.path());
        assert_eq!(agents[0].tools.as_deref(), Some([].as_slice()));
    }

    /// `subagents` behaves like `tools`: absent means `None` (inherit
    /// whatever the delegating turn can see), and an empty list means
    /// `Some(empty)` (never delegate further).
    #[test]
    fn subagents_field_parses_like_tools_does() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "plain.md",
            "---\ndescription: Thinks.\n---\nThink.\n",
        );
        write(
            d.path(),
            "limited.md",
            "---\ndescription: Plans.\nsubagents: [explorer]\n---\nPlan.\n",
        );

        let agents = load_agents_dir(d.path());
        let plain = agents.iter().find(|a| a.name == "plain").unwrap();
        let limited = agents.iter().find(|a| a.name == "limited").unwrap();
        assert_eq!(plain.subagents, None);
        assert_eq!(
            limited.subagents.as_deref(),
            Some(["explorer".to_string()].as_slice())
        );
    }

    /// One broken file must not take the others with it — the same
    /// rule `load_heartbeat_dir` follows.
    #[test]
    fn a_broken_definition_does_not_hide_the_others() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "good.md", "---\ndescription: Fine.\n---\nFine.\n");
        write(d.path(), "no-frontmatter.md", "just a body\n");
        write(
            d.path(),
            "bad-yaml.md",
            "---\ndescription: [unclosed\n---\nx\n",
        );

        let agents = load_agents_dir(d.path());
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["good"]);
    }

    /// A description is what the parent model reads to decide whether
    /// to delegate. Without one the agent can never be chosen, so the
    /// definition is useless rather than merely incomplete.
    #[test]
    fn a_definition_without_a_description_is_skipped() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "mystery.md", "---\ntools: []\n---\nHello.\n");
        assert!(load_agents_dir(d.path()).is_empty());
    }

    /// `profile:` is parsed onto the definition — it is the whole point
    /// of the field: a definition pins its own provider with it.
    #[test]
    fn a_profile_is_parsed_onto_the_definition() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "reviewer.md",
            "---\ndescription: Reviews.\nprofile: dev\n---\nReview.\n",
        );

        let agents = load_agents_dir(d.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].profile.as_deref(), Some("dev"));
    }

    /// Absent `profile:` means "the parent's provider, unchanged" — that
    /// must be distinguishable from `Some`. Same shape as the `tools` test:
    /// absent is `None`, never an empty string or anything else.
    #[test]
    fn an_omitted_profile_is_none_not_empty() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "helper.md",
            "---\ndescription: Thinks.\n---\nThink.\n",
        );

        let agents = load_agents_dir(d.path());
        assert_eq!(agents[0].profile, None);
    }

    #[test]
    fn a_missing_directory_is_no_agents_rather_than_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(load_agents_dir(&d.path().join("nope")).is_empty());
    }
}
