//! Shared YAML frontmatter split/parse/serialize helpers for Markdown files.
//!
//! Files are assumed to start with `---\n`, followed by YAML, terminated by a
//! line containing only `---`. Both LF and CRLF line endings are accepted.

use anyhow::{Context, Result};

/// Split a Markdown file with YAML frontmatter into `(frontmatter, body)`.
/// Returns `None` when the file has no frontmatter. The body is returned
/// verbatim (with any leading newline intact).
pub fn split(raw: &str) -> Option<(&str, &str)> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;
    let mut idx = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            let fm = &rest[..idx];
            let body_start = idx + line.len();
            return Some((fm, &rest[body_start..]));
        }
        idx += line.len();
    }
    None
}

/// Set the top-level `enabled:` key in `raw`'s frontmatter, and touch
/// nothing else.
///
/// `None` when `raw` has no frontmatter block — a file that is not a
/// definition at all is the caller's problem to refuse, not this
/// function's to repair by inventing one.
///
/// Deliberately not `parse_mapping` + `serialize`: that round trip drops
/// every comment and reorders the keys, so a model asked to flip one
/// switch would rewrite a human's file. A definition is a file people
/// hand-edit — the `schedule` line usually has a comment above it saying
/// why — and the tool's job is one line, not the document.
pub fn set_enabled(raw: &str, enabled: bool) -> Option<String> {
    let (fm, _) = split(raw)?;
    // Which delimiter the file already uses. Rewriting a CRLF file as LF
    // turns a one-line change into a whole-file diff.
    let head = if raw.starts_with("---\r\n") {
        "---\r\n"
    } else {
        "---\n"
    };
    let nl = if head == "---\r\n" { "\r\n" } else { "\n" };
    let value = if enabled { "true" } else { "false" };

    let mut out = String::with_capacity(raw.len() + 16);
    out.push_str(head);
    let mut replaced = false;
    for line in fm.split_inclusive('\n') {
        // Top-level only: a leading space is a nested key, not this one.
        if !replaced && line.trim_end_matches(['\n', '\r']).starts_with("enabled:") {
            out.push_str("enabled: ");
            out.push_str(value);
            out.push_str(nl);
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        if !out.ends_with(nl) {
            out.push_str(nl);
        }
        out.push_str("enabled: ");
        out.push_str(value);
        out.push_str(nl);
    }
    // From the closing `---` line onwards, verbatim.
    out.push_str(&raw[head.len() + fm.len()..]);
    Some(out)
}

/// Parse YAML frontmatter into a `serde_norway::Mapping`. Empty or unparseable
/// input yields an empty mapping — convenient when merging catchup updates.
pub fn parse_mapping(fm: &str) -> serde_norway::Mapping {
    serde_norway::from_str(fm).unwrap_or_default()
}

/// Serialize a mapping + body back into a Markdown file with YAML frontmatter.
/// Emits `---\n{yaml}---\n\n{body}`; the body's leading newlines are stripped
/// to guarantee exactly one blank line after the closing `---`.
pub fn serialize(meta: &serde_norway::Mapping, body: &str) -> Result<String> {
    let fm = serde_norway::to_string(meta).context("failed to serialize frontmatter")?;
    let body_trimmed = body.trim_start_matches(['\n', '\r']);
    Ok(format!("---\n{fm}---\n\n{body_trimmed}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lf() {
        let raw = "---\nfoo: 1\nbar: two\n---\n\n# Body\n";
        let (fm, body) = split(raw).unwrap();
        assert_eq!(fm, "foo: 1\nbar: two\n");
        assert_eq!(body, "\n# Body\n");
    }

    #[test]
    fn split_crlf() {
        let raw = "---\r\nfoo: 1\r\n---\r\nbody\r\n";
        let (fm, body) = split(raw).unwrap();
        assert_eq!(fm, "foo: 1\r\n");
        assert_eq!(body, "body\r\n");
    }

    #[test]
    fn split_empty_frontmatter() {
        let raw = "---\n---\nbody\n";
        let (fm, body) = split(raw).unwrap();
        assert_eq!(fm, "");
        assert_eq!(body, "body\n");
    }

    #[test]
    fn split_no_frontmatter() {
        assert!(split("# Just markdown\n").is_none());
        assert!(split("").is_none());
        assert!(split("---\nno closing delimiter\n").is_none());
    }

    #[test]
    fn parse_mapping_accepts_empty() {
        assert!(parse_mapping("").is_empty());
    }

    #[test]
    fn parse_mapping_preserves_keys() {
        let m = parse_mapping("foo: 1\nbar: two\n");
        assert_eq!(m.get("foo").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(m.get("bar").and_then(|v| v.as_str()), Some("two"));
    }

    #[test]
    fn serialize_roundtrip() {
        let original = "---\nfoo: 1\nbar: two\n---\n\nbody text\n";
        let (fm, body) = split(original).unwrap();
        let mapping = parse_mapping(fm);
        let out = serialize(&mapping, body).unwrap();
        // Re-split; keys and body should match.
        let (fm2, body2) = split(&out).unwrap();
        let mapping2 = parse_mapping(fm2);
        assert_eq!(mapping, mapping2);
        assert_eq!(body2.trim(), "body text");
    }

    #[test]
    fn set_enabled_replaces_only_the_key_and_keeps_the_rest() {
        let raw = "---\n# fires the morning call\nschedule: \"0 8 * * *\"\nenabled: true\nroom_id: \"!ops:x\"\n---\n\n# Morning\nCall the room.\n";
        let out = set_enabled(raw, false).unwrap();
        assert!(out.contains("enabled: false"), "{out}");
        assert!(out.contains("# fires the morning call"), "{out}");
        assert!(out.contains("room_id: \"!ops:x\""), "{out}");
        assert!(out.ends_with("# Morning\nCall the room.\n"), "{out}");
        // Still parses as the same document, with one value changed.
        let (fm, _) = split(&out).unwrap();
        assert_eq!(
            parse_mapping(fm).get("enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn set_enabled_inserts_the_key_when_it_is_absent() {
        let raw = "---\nschedule: \"0 8 * * *\"\n---\n\nBody\n";
        let out = set_enabled(raw, false).unwrap();
        let (fm, body) = split(&out).unwrap();
        assert_eq!(
            parse_mapping(fm).get("enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(body, "\nBody\n");
        // And a second call replaces rather than duplicating.
        let again = set_enabled(&out, true).unwrap();
        assert_eq!(again.matches("enabled:").count(), 1, "{again}");
    }

    /// `voice:` may carry a nested `enabled` some day; this must not rewrite it.
    #[test]
    fn set_enabled_leaves_an_indented_enabled_alone() {
        let raw = "---\nschedule: \"0 8 * * *\"\nvoice:\n  device_id: \"01J\"\n  enabled: true\n---\n\nBody\n";
        let out = set_enabled(raw, false).unwrap();
        assert!(out.contains("  enabled: true"), "{out}");
        assert_eq!(out.matches("enabled: false").count(), 1, "{out}");
    }

    #[test]
    fn set_enabled_is_none_without_frontmatter() {
        assert!(set_enabled("# Just markdown\n", true).is_none());
        assert!(set_enabled("", true).is_none());
        assert!(set_enabled("---\nnever closed\n", true).is_none());
    }

    /// CRLF files exist; rewriting them as LF would be a whole-file diff.
    #[test]
    fn set_enabled_preserves_crlf() {
        let raw = "---\r\nschedule: \"0 8 * * *\"\r\n---\r\nBody\r\n";
        let out = set_enabled(raw, false).unwrap();
        assert!(out.ends_with("---\r\nBody\r\n"), "{out:?}");
        assert!(out.contains("enabled: false\r\n"), "{out:?}");
    }
}
