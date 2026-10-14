//! The rules every session store follows at the storage boundary.
//!
//! Two stores write conversations to disk — `SessionStore` (four kinds)
//! and `AcpSessionStore` — with different line formats and the same
//! constraints. What is shared here is not the format; it is the set of
//! facts about the workspace and the Anthropic API that neither store is
//! free to decide for itself:
//!
//! - a lost tool payload gets one specific stand-in, not each store's own
//! - `tool_use` and `tool_result` must be adjacent, whatever gaps a
//!   crash or a partial sync left behind

use crate::provider::{ChatMessage, ContentPart, Role};
use serde_json::Value;
use std::collections::HashSet;

/// What the model is told when a tool result is no longer in the cache.
///
/// The pairing between `tool_use` and `tool_result` is what the API
/// validates, not the content — so a placeholder keeps the history
/// valid and the conversation's shape intact. A session that loads
/// thinner is worth having; one that fails to load is not.
pub const MISSING_RESULT: &str =
    "[this tool result is no longer stored; call the tool again if you need it]";

/// The `input` a `tool_use` gets when its payload is no longer cached.
///
/// An object rather than a sentence, because `input` is typed as JSON
/// and a bare string would not round-trip through the same field. The
/// API does not validate a replayed `input` against the tool's schema,
/// so any object is accepted here.
///
/// This degrades worse than a missing result and it is worth being
/// honest about which: the model can answer a missing result by calling
/// the tool again, but a missing input leaves it knowing it made a call
/// without knowing what it asked for. The `name` beside it is what
/// keeps that from being nothing at all.
pub fn missing_input() -> Value {
    serde_json::json!({
        "_unavailable": "the arguments to this call are no longer stored"
    })
}

/// Make a loaded conversation something the API will accept.
///
/// A `tool_use` can end up on disk without its `tool_result` — the
/// second append failed, the process died between the two, a sync landed
/// only half the pair. The gap is worse than a lost message, because the
/// API requires a `tool_result` to sit in the message *immediately
/// following* its `tool_use`, not merely present somewhere later.
///
/// The check is positional in both directions, and that is the point. A
/// set-based "does this id appear anywhere" check silently accepts
/// pairings the API rejects: two messages carrying the same `tool_use`
/// id where only one is answered, an id answered many messages before
/// the call that (re)issued it, a `tool_result` answering nothing
/// adjacent at all.
///
/// Synthesise rather than drop the `tool_use`: dropping would erase the
/// fact that the agent attempted the call, and `MISSING_RESULT` is
/// exactly the shape a cache miss already produces, so the model sees
/// nothing it does not already handle.
pub fn repair_tool_pairing(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    // Pass 1 — drop every `tool_result` whose `tool_use` is not in the
    // message immediately before it. If that empties a message, drop the
    // message too: an empty message is its own API error. A message that
    // arrived empty is left exactly as it was — this pass only undoes
    // damage it caused, never damage that was already on disk.
    let mut kept: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    for (idx, message) in messages.iter().enumerate() {
        let prev_uses: HashSet<&str> = idx
            .checked_sub(1)
            .and_then(|p| messages.get(p))
            .map(tool_use_ids)
            .unwrap_or_default();
        let parts: Vec<ContentPart> = message
            .parts
            .iter()
            .filter(|p| match p {
                ContentPart::ToolResult { tool_use_id, .. } => {
                    prev_uses.contains(tool_use_id.as_str())
                }
                _ => true,
            })
            .cloned()
            .collect();
        if parts.is_empty() && !message.parts.is_empty() {
            continue;
        }
        kept.push(ChatMessage {
            parts,
            ..message.clone()
        });
    }

    // Pass 2 — answer every `tool_use` the following message left open.
    let mut out: Vec<ChatMessage> = Vec::with_capacity(kept.len() + 1);
    let mut i = 0;
    while i < kept.len() {
        let message = kept[i].clone();
        let uses = ordered_tool_use_ids(&message);
        if uses.is_empty() {
            out.push(message);
            i += 1;
            continue;
        }
        let next = kept.get(i + 1);
        let answered: HashSet<String> = next
            .map(tool_result_ids)
            .unwrap_or_default()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let missing: Vec<ContentPart> = uses
            .iter()
            .filter(|id| !answered.contains(id.as_str()))
            .map(|id| ContentPart::ToolResult {
                tool_use_id: id.clone(),
                content: MISSING_RESULT.to_string(),
            })
            .collect();
        let has_answer = !answered.is_empty();
        out.push(message);
        if missing.is_empty() {
            i += 1;
            continue;
        }
        // #195. `answered` being non-empty means the next message is
        // already this one's `tool_result` message — pass 1 dropped any
        // result that answered something else. Merging into it is what
        // keeps the real results adjacent to their `tool_use`; splicing
        // a message in front would displace them by one and be rejected
        // for the very reason this function exists.
        //
        // That merge target really is `kept[i]`'s original successor,
        // not a message pass 1 shifted into place: pass 1 only ever
        // removes `ToolResult` parts, so a message carrying a `ToolUse`
        // part is never emptied and never dropped by it. So whenever
        // `answered` is non-empty here, `kept[i + 1]` is still adjacent
        // to `kept[i]` exactly as it was on disk.
        if has_answer {
            // Extend in place and advance by one, not two — the merged
            // message becomes `kept[i]` on the next iteration, so its
            // own `ToolUse` parts (it can carry calls of its own) still
            // get scanned and answered. Splicing it straight into `out`
            // would skip that scan and could hand back API-invalid
            // output for a call the merge happened to carry.
            kept[i + 1].parts.extend(missing);
            i += 1;
        } else {
            out.push(ChatMessage {
                role: Role::User,
                parts: missing,
                input_kind: None,
                user_id: None,
            });
            i += 1;
        }
    }
    out
}

fn tool_use_ids(msg: &ChatMessage) -> HashSet<&str> {
    msg.parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(msg: &ChatMessage) -> HashSet<&str> {
    msg.parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

/// Call order, deduplicated. Two `tool_use` parts that (wrongly) share
/// one id must not produce two placeholders for it.
fn ordered_tool_use_ids(msg: &ChatMessage) -> Vec<String> {
    let mut seen = HashSet::new();
    msg.parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, .. } if seen.insert(id.clone()) => Some(id.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stand-in has to be an object, because it goes back into a
    /// field typed as the tool's JSON `input` — a bare string would not
    /// round-trip through the same slot.
    #[test]
    fn the_missing_input_stand_in_is_a_small_object() {
        let placeholder = missing_input();
        assert!(placeholder.is_object(), "must be an object: {placeholder}");
        assert!(
            placeholder.get("_unavailable").is_some(),
            "missing marker: {placeholder}"
        );
        assert!(serde_json::to_string(&placeholder).unwrap().len() < 200);
    }

    fn tool_use(id: &str) -> ContentPart {
        ContentPart::ToolUse {
            id: id.to_string(),
            name: "file_read".to_string(),
            input: serde_json::json!({}),
        }
    }

    fn tool_result(id: &str, content: &str) -> ContentPart {
        ContentPart::ToolResult {
            tool_use_id: id.to_string(),
            content: content.to_string(),
        }
    }

    fn assistant(parts: Vec<ContentPart>) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            parts,
            input_kind: None,
            user_id: None,
        }
    }

    fn user(parts: Vec<ContentPart>) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            parts,
            input_kind: None,
            user_id: None,
        }
    }

    fn result_ids(msg: &ChatMessage) -> Vec<&str> {
        msg.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A tool_use whose result never made it to disk gets a synthesised
    /// one immediately after it — the API requires the answer in the
    /// very next message, not merely somewhere later.
    #[test]
    fn an_unanswered_tool_use_gets_a_placeholder_right_after_it() {
        let repaired = repair_tool_pairing(vec![
            user(vec![ContentPart::Text("read it".to_string())]),
            assistant(vec![tool_use("c1")]),
        ]);
        assert_eq!(repaired.len(), 3);
        assert_eq!(result_ids(&repaired[2]), vec!["c1"]);
        assert!(
            matches!(&repaired[2].parts[0], ContentPart::ToolResult { content, .. } if content == MISSING_RESULT)
        );
    }

    /// A tool_result whose tool_use is not in the message right before it
    /// is not a valid pairing wherever else its id appears — drop it, and
    /// drop the message if that empties it.
    #[test]
    fn an_orphaned_tool_result_is_dropped() {
        let repaired = repair_tool_pairing(vec![
            user(vec![ContentPart::Text("hi".to_string())]),
            user(vec![tool_result("c1", "stale")]),
        ]);
        assert_eq!(
            repaired.len(),
            1,
            "the orphan message must go: {repaired:?}"
        );
    }

    /// #195: when the next message already answers *some* of the calls,
    /// the placeholders belong inside it. Splicing a new message in front
    /// would push the real result one further from its tool_use — the
    /// exact rejection the repair exists to prevent.
    #[test]
    fn a_partly_answered_tool_use_merges_rather_than_splices() {
        let repaired = repair_tool_pairing(vec![
            assistant(vec![tool_use("c1"), tool_use("c2")]),
            user(vec![tool_result("c1", "the real result")]),
        ]);

        assert_eq!(
            repaired.len(),
            2,
            "no message may be spliced between the pair: {repaired:?}"
        );
        let mut ids = result_ids(&repaired[1]);
        ids.sort();
        assert_eq!(ids, vec!["c1", "c2"], "both ids answer in one message");
        let real = repaired[1].parts.iter().any(
            |p| matches!(p, ContentPart::ToolResult { tool_use_id, content } if tool_use_id == "c1" && content == "the real result"),
        );
        assert!(real, "the real result must survive: {:?}", repaired[1]);
    }

    /// Two tool_use parts that (wrongly) share one id must not produce
    /// two placeholders for it.
    #[test]
    fn a_duplicated_tool_use_id_gets_one_placeholder() {
        let repaired = repair_tool_pairing(vec![assistant(vec![tool_use("c1"), tool_use("c1")])]);
        assert_eq!(result_ids(&repaired[1]), vec!["c1"]);
    }

    /// A well-formed conversation passes through untouched.
    #[test]
    fn a_paired_conversation_is_left_alone() {
        let input = vec![
            assistant(vec![tool_use("c1")]),
            user(vec![tool_result("c1", "ok")]),
        ];
        assert_eq!(repair_tool_pairing(input.clone()), input);
    }

    /// The merge must not swallow whatever follows it.
    #[test]
    fn a_merge_does_not_swallow_the_message_after_it() {
        let repaired = repair_tool_pairing(vec![
            assistant(vec![tool_use("c1"), tool_use("c2")]),
            user(vec![tool_result("c1", "real")]),
            user(vec![ContentPart::Text("next".to_string())]),
        ]);
        assert_eq!(repaired.len(), 3, "{repaired:?}");
        assert!(matches!(&repaired[2].parts[0], ContentPart::Text(t) if t == "next"));
    }

    /// A message can be both an answer to the one before it and a caller in
    /// its own right. Merging a placeholder into it must not exempt it from
    /// the scan that would answer its own call.
    #[test]
    fn a_merged_message_still_gets_its_own_calls_answered() {
        let repaired = repair_tool_pairing(vec![
            assistant(vec![tool_use("c1"), tool_use("c2")]),
            user(vec![tool_result("c1", "real"), tool_use("c3")]),
        ]);
        let answered: std::collections::HashSet<&str> = repaired
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                ContentPart::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            answered.contains("c3"),
            "c3 was never answered: {repaired:?}"
        );
    }
}
