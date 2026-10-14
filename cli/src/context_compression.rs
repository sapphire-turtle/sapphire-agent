//! Context compression: summarize older messages when conversation approaches
//! the model's context window limit.
//!
//! Strategy:
//! 1. Estimate token count of system prompt + messages
//! 2. If above threshold (default 80% of context window), compress
//! 3. Keep the most recent N messages verbatim
//! 4. Summarize everything before that into a single user message
//! 5. Return the compressed history

use crate::config::CompressionConfig;
use crate::provider::{ChatMessage, ContentPart, PromptUsage, Provider, Role, ToolSpec};
use crate::session_storage::{MISSING_RESULT, missing_input};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tracing::{debug, info, warn};

/// Rough token estimate for a string.
///
/// Uses a simple heuristic: ~4 characters per token for ASCII,
/// ~1.5 characters per token for non-ASCII (CJK, etc.).
pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii_chars = 0usize;
    let mut non_ascii_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else {
            non_ascii_chars += 1;
        }
    }
    // Rough estimate: ASCII ~4 chars/token, non-ASCII ~1.5 chars/token
    let ascii_tokens = ascii_chars / 4;
    let non_ascii_tokens = (non_ascii_chars * 2).div_ceil(3); // ceil(n * 2/3)
    ascii_tokens + non_ascii_tokens
}

/// Estimate the total token usage for a system prompt + message history.
pub fn estimate_total_tokens(system: Option<&str>, messages: &[ChatMessage]) -> usize {
    let system_tokens = system.map(estimate_tokens).unwrap_or(0);
    let message_tokens: usize = messages.iter().map(estimate_message_tokens).sum();
    // Add a small overhead for message framing (~4 tokens per message)
    system_tokens + message_tokens + messages.len() * 4
}

/// Estimate the tokens the tool schemas add to every request.
///
/// These are not history and they are never compacted, but the provider
/// counts them: a couple of dozen MCP tools is a fixed five-figure tax on
/// the window that the message estimate alone does not see. Leaving them out
/// is one of the two reasons the budget used to read low.
pub fn estimate_tools_tokens(tools: Option<&[ToolSpec]>) -> usize {
    tools
        .unwrap_or(&[])
        .iter()
        .map(|t| {
            estimate_tokens(&t.name)
                + estimate_tokens(&t.description)
                + estimate_tokens(&t.input_schema.to_string())
                // The JSON wrapper each spec is served in.
                + 8
        })
        .sum()
}

/// What a request costs by our own reckoning: history, system prompt and the
/// tool schemas that ride along with it.
///
/// This is the quantity `record_prompt_usage` calibrates, so it must stay the
/// whole of what the provider is about to count. Estimating one thing and
/// correcting another would make the factor meaningless.
pub fn estimate_request_tokens(
    system: Option<&str>,
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
) -> usize {
    estimate_total_tokens(system, messages) + estimate_tools_tokens(tools)
}

/// Per-provider correction factor: what the provider says a request cost,
/// divided by what `estimate_request_tokens` said it would.
///
/// A chars-per-token rule of thumb cannot be right for every tokenizer and
/// every language at once — Japanese prose and dense JSON both cost far more
/// per character than the 4:1 the estimate assumes. Rather than guess harder,
/// take the number the provider already reports and let it correct the guess.
/// The factor is keyed by tokenizer (provider name), starts at 1.0, and is
/// smoothed so a single odd request cannot swing the budget.
///
/// It lives in memory only: a restart re-learns it from the first response
/// that reports usage.
static CALIBRATION: LazyLock<Mutex<HashMap<String, f64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How much of each observation to fold in. Low enough that one outlier
/// cannot move the budget far, high enough to converge within a handful of
/// turns.
const CALIBRATION_ALPHA: f64 = 0.3;

/// Bounds on a single observation. A ratio outside these says the estimate
/// and the report are measuring different things (an empty prompt, a server
/// reporting nonsense) rather than that the tokenizer is that expensive.
const CALIBRATION_MIN: f64 = 0.5;
const CALIBRATION_MAX: f64 = 8.0;

/// Below this the request is too small for the ratio to mean anything —
/// framing overhead dominates and the number would be noise.
const CALIBRATION_MIN_TOKENS: usize = 200;

/// Fold one provider-reported prompt size into its tokenizer's factor.
///
/// `estimated` must be `estimate_request_tokens` over exactly what was sent.
/// Responses that report no usage (a server that ignores `include_usage`)
/// teach nothing and are dropped.
pub fn record_prompt_usage(estimated: usize, usage: Option<&PromptUsage>) {
    let Some(usage) = usage else { return };
    if estimated < CALIBRATION_MIN_TOKENS || usage.tokens == 0 {
        return;
    }
    let ratio = (usage.tokens as f64 / estimated as f64).clamp(CALIBRATION_MIN, CALIBRATION_MAX);
    let Ok(mut map) = CALIBRATION.lock() else {
        return;
    };
    let entry = map.entry(usage.provider.clone()).or_insert(1.0);
    *entry = *entry * (1.0 - CALIBRATION_ALPHA) + ratio * CALIBRATION_ALPHA;
    debug!(
        "Context estimate calibration for '{}': observed {} actual vs ~{estimated} estimated \
         (ratio {ratio:.2}) → factor {:.2}",
        usage.provider, usage.tokens, *entry
    );
}

/// The factor to apply for a provider that may answer through any of `keys`.
///
/// The largest wins. A fallback chain shares one budget but not one
/// tokenizer, and the budget has to hold for whichever member actually takes
/// the call — which, by the time we find out, is too late to compact.
fn calibration_for(keys: &[&str]) -> f64 {
    let Ok(map) = CALIBRATION.lock() else {
        return 1.0;
    };
    keys.iter()
        .filter_map(|k| map.get(*k).copied())
        .fold(1.0f64, f64::max)
}

/// Estimate tokens for a single ChatMessage.
fn estimate_message_tokens(msg: &ChatMessage) -> usize {
    msg.parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(t) => estimate_tokens(t),
            // Anthropic charges per ~750x750 image tile; rough flat approximation.
            ContentPart::Image { .. } => 1600,
            // ImageRef is the compact form held in long-lived history.
            // If hydration succeeds at provider-call time, the bytes
            // count as ~Image; until then it's a hash marker. Estimate
            // as Image so we don't under-budget when the cache is hot.
            ContentPart::ImageRef { .. } => 1600,
            ContentPart::ToolUse { name, input, .. } => {
                estimate_tokens(name) + estimate_tokens(&input.to_string())
            }
            ContentPart::ToolResult { content, .. } => estimate_tokens(content),
            // Hydration happens before this runs on any real path, so
            // these are the un-hydrated forms only — a stand-in's worth.
            ContentPart::ToolUseRef { name, .. } => {
                estimate_tokens(name) + estimate_tokens(&missing_input().to_string())
            }
            ContentPart::ToolResultRef { .. } => estimate_tokens(MISSING_RESULT),
        })
        .sum()
}

/// A compaction summary rendered back into the conversation.
///
/// One generator, three callers: `maybe_compress`, the day-boundary
/// compaction in `Agent`, and every store's restore path. They used to
/// have two wordings between them, and a restore has no way to know
/// which one produced the summary it is reading — so there is one.
///
/// The wording is not load-bearing; being the same everywhere is.
pub fn compaction_stub(summary: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::Text(format!(
                "[Context Summary — earlier messages were compressed]\n\n{summary}"
            ))],
            input_kind: None,
            user_id: None,
        },
        ChatMessage::assistant("Understood. I have the context from our earlier conversation."),
    ]
}

/// Outcome of a compression attempt.
pub struct CompressionResult {
    pub compressed: Vec<ChatMessage>,
    /// The summary that replaced the absorbed prefix, or `None` when
    /// nothing was summarized and the pass only trimmed oversized parts
    /// out of the preserved window. There is no checkpoint to record in
    /// that case — the log still holds every message, untrimmed.
    pub summary: Option<String>,
    /// How many trailing messages survived verbatim. The store turns
    /// this into a checkpoint cursor by counting back from its file's
    /// tip — the caller has no way to map an in-memory index onto a line
    /// number, and should not have to.
    pub keep_recent: usize,
}

/// Check whether compression is needed and, if so, compress the history.
///
/// `tools` is the spec list this turn will send. It is not history and
/// nothing here compacts it, but it is part of what the provider counts, so
/// it is part of the budget.
///
/// Returns `Ok(None)` if no compression was needed.
/// Returns `Ok(Some(CompressionResult))` with the new message history and the
/// raw summary text (to be persisted as a `SummaryLine`) if compressed.
pub async fn maybe_compress(
    provider: &dyn Provider,
    system: Option<&str>,
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
    config: &CompressionConfig,
) -> anyhow::Result<Option<CompressionResult>> {
    if !config.enabled {
        return Ok(None);
    }

    // Everything from here on is in two currencies. `raw_*` is what the local
    // estimate says; the threshold is in the provider's tokens. `factor`
    // converts between them, and the two must never be compared directly.
    let factor = calibration_for(&provider.calibration_keys());
    let tools_tokens = estimate_tools_tokens(tools);
    let raw_total = estimate_total_tokens(system, messages) + tools_tokens;
    let total_tokens = (raw_total as f64 * factor) as usize;
    let threshold_tokens = (config.context_window as f64 * config.threshold) as usize;

    if total_tokens < threshold_tokens {
        return Ok(None);
    }

    info!(
        "Context compression triggered: ~{total_tokens} tokens estimated \
         (~{raw_total} raw × {factor:.2} calibration, of which ~{tools_tokens} raw is tool \
         schemas; threshold: {threshold_tokens}, window: {})",
        config.context_window
    );

    let split = find_safe_split_point(messages, config.preserve_recent);

    let (mut compressed, summary, keep_recent) = if split == 0 {
        // Nothing is old enough to summarize: the whole history sits inside
        // the window we promised to keep verbatim. That is not a reason to
        // give up — one oversized tool result is enough to push a
        // five-message session past the window — so fall through to the
        // trim with the history as it stands.
        (messages.to_vec(), None, messages.len())
    } else {
        let to_summarize = &messages[..split];
        let to_keep = &messages[split..];

        let summary = generate_summary(provider, to_summarize).await?;

        info!(
            "Compressed {} messages into summary ({} → ~{} tokens)",
            split,
            estimate_total_tokens(None, to_summarize),
            estimate_tokens(&summary),
        );

        let mut compressed = compaction_stub(&summary);
        compressed.extend_from_slice(to_keep);
        (compressed, Some(summary), to_keep.len())
    };

    // A summary only shrinks what it absorbed. The preserved window goes to
    // the provider verbatim, and a single tool result that arrived inside it
    // can be larger than the entire budget — which is how a turn overflows
    // *after* a successful compaction. Trim what survived so the call we are
    // about to make actually fits.
    //
    // The budget goes back into raw currency first, because the trim measures
    // what it cuts with the same estimate that under-counted to begin with.
    // The tool schemas come off the top: they are part of the budget, and none
    // of them is ours to cut.
    let raw_budget = ((threshold_tokens as f64 / factor) as usize).saturating_sub(tools_tokens);
    let trimmed = trim_to_budget(system, &mut compressed, raw_budget);

    if summary.is_none() && !trimmed {
        // Nothing summarized, nothing trimmed: no new history to hand back.
        return Ok(None);
    }

    Ok(Some(CompressionResult {
        compressed,
        summary,
        keep_recent,
    }))
}

/// Note left where a trim removed text, so the model can tell a short tool
/// result from a shortened one.
fn trim_marker(omitted_chars: usize) -> String {
    format!("\n… [trimmed: {omitted_chars} chars omitted to fit the context window]")
}

/// What `trim_marker` costs, generously rounded up. Subtracted from every
/// per-part cap so appending the marker cannot push a part back over the cap
/// it was just truncated to.
const TRIM_MARKER_TOKENS: usize = 24;

/// Cut `text` down to roughly `max_tokens` by `estimate_tokens`, with a
/// marker naming what went missing.
///
/// The cut walks characters rather than bytes: the estimator prices ASCII and
/// non-ASCII differently, and slicing at a byte index inside a CJK tool
/// result panics (the same trap `generate_summary` documents).
fn truncate_to_token_budget(text: &str, max_tokens: usize) -> String {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    let mut end = 0usize;
    let mut kept = 0usize;
    for (i, ch) in text.char_indices() {
        let (a, n) = if ch.is_ascii() {
            (ascii + 1, non_ascii)
        } else {
            (ascii, non_ascii + 1)
        };
        if a / 4 + (n * 2).div_ceil(3) > max_tokens {
            break;
        }
        ascii = a;
        non_ascii = n;
        end = i + ch.len_utf8();
        kept += 1;
    }
    let omitted = text.chars().count() - kept;
    format!("{}{}", &text[..end], trim_marker(omitted))
}

/// How much of a part is text we are allowed to cut, in tokens.
///
/// `None` marks a part with nothing to give: images cost a flat tile price,
/// and the un-hydrated `*Ref` forms are already stand-ins.
fn trimmable_tokens(part: &ContentPart) -> Option<usize> {
    match part {
        ContentPart::Text(t) => Some(estimate_tokens(t)),
        ContentPart::ToolResult { content, .. } => Some(estimate_tokens(content)),
        // The call's arguments, not its name — the name is what keeps a
        // trimmed call legible, and it is tiny. A file-sized argument is as
        // real an overflow as a file-sized result.
        ContentPart::ToolUse { input, .. } => Some(estimate_tokens(&input.to_string())),
        ContentPart::Image { .. }
        | ContentPart::ImageRef { .. }
        | ContentPart::ToolUseRef { .. }
        | ContentPart::ToolResultRef { .. } => None,
    }
}

/// The largest per-part cap under which every trimmable part fits the budget
/// — the classic water-filling level.
///
/// Capping every part at one ceiling is what keeps a trim proportionate: the
/// single 300k-token tool result is cut down to the level, and the dozens of
/// small results around it are left alone.
fn water_fill_cap(sizes: &[usize], fixed: usize, budget: usize) -> usize {
    let mut sizes = sizes.to_vec();
    sizes.sort_unstable();
    let mut remaining = budget.saturating_sub(fixed);
    for (i, &size) in sizes.iter().enumerate() {
        let above = sizes.len() - i;
        if size.saturating_mul(above) <= remaining {
            remaining -= size;
        } else {
            return remaining / above;
        }
    }
    usize::MAX
}

/// Shrink `messages` in place until the estimate fits `budget`, by truncating
/// the text of its largest parts. Returns whether anything changed.
///
/// Messages are never dropped and never reordered: `keep_recent` is a count
/// of trailing messages that the store turns into a checkpoint cursor, and
/// tool-use/tool-result pairing is what the provider validates. Cutting
/// contents leaves both intact.
///
/// Callers write the result back into the in-memory history, so a trimmed
/// part stays trimmed for the rest of the process. The session log keeps
/// every message at full length either way: nothing is lost on disk, and a
/// reload starts from the full text and trims again only if it still has to.
fn trim_to_budget(system: Option<&str>, messages: &mut [ChatMessage], budget: usize) -> bool {
    let total = estimate_total_tokens(system, messages);
    if total <= budget {
        return false;
    }

    let mut slots: Vec<(usize, usize, usize)> = Vec::new();
    for (m, msg) in messages.iter().enumerate() {
        for (p, part) in msg.parts.iter().enumerate() {
            if let Some(tokens) = trimmable_tokens(part) {
                slots.push((m, p, tokens));
            }
        }
    }
    if slots.is_empty() {
        warn!(
            "Context is ~{total} tokens against a budget of {budget} and nothing in it \
             can be trimmed (images and stand-ins only); the request may be rejected"
        );
        return false;
    }

    let sizes: Vec<usize> = slots.iter().map(|(_, _, tokens)| *tokens).collect();
    let fixed = total.saturating_sub(sizes.iter().sum());
    let cap = water_fill_cap(&sizes, fixed, budget);
    let keep = cap.saturating_sub(TRIM_MARKER_TOKENS);

    let mut changed = false;
    for &(m, p, tokens) in &slots {
        if tokens <= cap {
            continue;
        }
        match &mut messages[m].parts[p] {
            ContentPart::Text(text) => *text = truncate_to_token_budget(text, keep),
            ContentPart::ToolResult { content, .. } => {
                *content = truncate_to_token_budget(content, keep);
            }
            // Tool input has to stay an object for the provider, so the
            // truncated JSON comes back as the value of one field rather than
            // as a mangled object of its own.
            ContentPart::ToolUse { input, .. } => {
                *input = serde_json::json!({
                    "trimmed": truncate_to_token_budget(&input.to_string(), keep),
                });
            }
            _ => continue,
        }
        changed = true;
    }

    if !changed {
        return false;
    }

    let after = estimate_total_tokens(system, messages);
    info!("Trimmed history to fit the budget: ~{total} → ~{after} tokens (budget {budget})");
    if after > budget {
        warn!(
            "Context is still ~{after} tokens after trimming (budget {budget}); the \
             untrimmable remainder — images, tool names, message framing — is larger \
             than the budget and the request may be rejected"
        );
    }
    true
}

/// Find a safe split point that doesn't break tool-call/result pairs.
///
/// We want to keep at least `preserve_recent` messages at the end,
/// but if the boundary lands between a tool-use assistant message and
/// its corresponding tool-result user message, we move the boundary
/// earlier to keep the pair together.
fn find_safe_split_point(messages: &[ChatMessage], preserve_recent: usize) -> usize {
    if messages.len() <= preserve_recent {
        return 0;
    }

    let mut split = messages.len() - preserve_recent;

    // If the message at `split` is a tool-result (user message with ToolResult parts),
    // move split back to include the preceding assistant tool-use message.
    while split > 0 {
        let msg = &messages[split];
        let is_tool_result = msg.role == Role::User
            && msg
                .parts
                .iter()
                .any(|p| matches!(p, ContentPart::ToolResult { .. }));
        if is_tool_result {
            split -= 1;
        } else {
            break;
        }
    }

    // Also check: if the message just before split is an assistant message
    // with tool_use, include it in the "keep" side to maintain the pair.
    if split > 0 {
        let prev = &messages[split - 1];
        let has_tool_use = prev.role == Role::Assistant
            && prev
                .parts
                .iter()
                .any(|p| matches!(p, ContentPart::ToolUse { .. }));
        if has_tool_use {
            // The message at split should be the tool result — keep the pair together
            // by not moving split further.
        }
    }

    split
}

/// Generate a concise summary of a sequence of messages using the LLM.
///
/// Tool-call and tool-result parts are rendered as plain-text placeholders,
/// so the input need not be tool-paired — safe to call on raw, potentially
/// incomplete history loaded from disk at startup.
pub async fn generate_summary(
    provider: &dyn Provider,
    messages: &[ChatMessage],
) -> anyhow::Result<String> {
    // Build a textual representation of the messages to summarize
    let mut transcript = String::new();
    for msg in messages {
        let role_label = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        for part in &msg.parts {
            match part {
                ContentPart::Text(t) => {
                    transcript.push_str(&format!("{role_label}: {t}\n\n"));
                }
                ContentPart::Image { media_type, .. } => {
                    transcript.push_str(&format!("{role_label}: [image: {media_type}]\n\n"));
                }
                ContentPart::ImageRef { media_type, sha256 } => {
                    transcript.push_str(&format!(
                        "{role_label}: [image: {media_type} sha256={sha256}]\n\n"
                    ));
                }
                // `name` survives the storage boundary precisely so an
                // un-hydrated call still reads as something here.
                ContentPart::ToolUse { name, .. } | ContentPart::ToolUseRef { name, .. } => {
                    transcript.push_str(&format!("{role_label}: [Called tool: {name}]\n\n"));
                }
                ContentPart::ToolResult { content, .. } => {
                    // Truncate long tool results to keep the summary prompt
                    // manageable. `floor_char_boundary` is required: byte
                    // 500 can land inside a multi-byte character (a CJK
                    // tool result routed through here by an ACP session's
                    // `history()` makes this a routine occurrence, not an
                    // edge case), and a raw slice there panics.
                    let truncated = if content.len() > 500 {
                        format!(
                            "{}... (truncated)",
                            &content[..content.floor_char_boundary(500)]
                        )
                    } else {
                        content.clone()
                    };
                    transcript.push_str(&format!("{role_label}: [Tool result: {truncated}]\n\n"));
                }
                ContentPart::ToolResultRef { .. } => {
                    transcript.push_str(&format!("{role_label}: [Tool result: unavailable]\n\n"));
                }
            }
        }
    }

    // Cap the transcript to avoid exceeding context on the summary call itself
    let max_transcript_chars = 50_000;
    if transcript.len() > max_transcript_chars {
        transcript.truncate(max_transcript_chars);
        transcript.push_str("\n... (transcript truncated for summarization)");
    }

    let prompt = format!(
        "Summarize the following conversation concisely. \
         Preserve key information: decisions made, code context, task state, \
         important facts, and any instructions or preferences expressed. \
         Focus on information that would be needed to continue the conversation. \
         Write the summary in the same language(s) used in the conversation.\n\n\
         ---\n\n{transcript}"
    );

    let summary_messages = vec![ChatMessage::user(&prompt)];
    let response = provider.chat(None, &summary_messages, None).await?;

    match response.text {
        Some(text) if !text.is_empty() => Ok(text),
        _ => {
            warn!("Summary generation returned empty response");
            Ok(
                "(Earlier conversation context was compressed but summary generation failed.)"
                    .into(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `Provider` double for `generate_summary`: always returns
    /// the same short text, regardless of what transcript it was handed.
    struct StubProvider;

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }

        async fn chat(
            &self,
            _system: Option<&str>,
            _messages: &[ChatMessage],
            _tools: Option<&[crate::provider::ToolSpec]>,
        ) -> anyhow::Result<crate::provider::ChatResponse> {
            Ok(crate::provider::ChatResponse::text_only(
                "a summary".to_string(),
            ))
        }
    }

    #[test]
    fn test_estimate_tokens_ascii() {
        // "hello world" = 11 chars, ~2-3 tokens
        let tokens = estimate_tokens("hello world");
        assert!(tokens > 0);
        assert!(tokens < 10);
    }

    #[test]
    fn test_estimate_tokens_cjk() {
        // 6 CJK characters, ~4 tokens
        let tokens = estimate_tokens("こんにちは世界");
        assert!(tokens > 0);
    }

    #[test]
    fn test_estimate_tokens_mixed() {
        let tokens = estimate_tokens("Hello こんにちは World");
        assert!(tokens > 0);
    }

    #[test]
    fn test_find_safe_split_point_basic() {
        let messages = vec![
            ChatMessage::user("msg1"),
            ChatMessage::assistant("msg2"),
            ChatMessage::user("msg3"),
            ChatMessage::assistant("msg4"),
            ChatMessage::user("msg5"),
            ChatMessage::assistant("msg6"),
        ];
        let split = find_safe_split_point(&messages, 2);
        assert_eq!(split, 4);
    }

    #[test]
    fn test_find_safe_split_preserves_all_when_few() {
        let messages = vec![ChatMessage::user("msg1"), ChatMessage::assistant("msg2")];
        let split = find_safe_split_point(&messages, 5);
        assert_eq!(split, 0);
    }

    #[test]
    fn test_find_safe_split_avoids_breaking_tool_pair() {
        use serde_json::json;

        let messages = vec![
            ChatMessage::user("start"),
            ChatMessage::assistant("thinking"),
            ChatMessage::user("question"),
            ChatMessage::assistant_with_tools(
                None,
                vec![crate::provider::ToolCall {
                    id: "t1".into(),
                    name: "search".into(),
                    input: json!({}),
                }],
            ),
            ChatMessage::tool_results_with_images(vec![("t1".into(), "result".into())], vec![]),
            ChatMessage::assistant("final answer"),
        ];

        // preserve_recent=2 would normally split at index 4 (tool result),
        // but it should move back to not break the tool pair.
        let split = find_safe_split_point(&messages, 2);
        assert!(
            split <= 3,
            "split should be at or before the tool-use message"
        );
    }

    /// `generate_summary`'s tool-result truncation slices at a raw byte
    /// index. A CJK tool result long enough to cross the 500-byte cutoff
    /// must not land byte 500 inside a multi-byte character — this is
    /// the exact panic the half-hourly ACP digest sweep started hitting
    /// once ACP sessions began persisting tool results. That sweep is
    /// gone, but `generate_summary` still runs on every compaction, and
    /// the cutoff is still a byte index.
    #[tokio::test]
    async fn a_long_cjk_tool_result_does_not_panic_on_the_byte_cutoff() {
        // 3 bytes per character; 200 characters is 600 bytes, comfortably
        // past the 500-byte cutoff and guaranteed to straddle it given the
        // fixed 3-byte width.
        let long_cjk = "日".repeat(200);
        let messages = vec![ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::ToolResult {
                tool_use_id: "c1".to_string(),
                content: long_cjk,
            }],
            input_kind: None,
            user_id: None,
        }];

        // Must not panic, and must produce a summary (the stub's fixed
        // text — what matters here is that generate_summary returned at
        // all rather than unwinding inside the tokio task).
        let summary = generate_summary(&StubProvider, &messages).await.unwrap();
        assert_eq!(summary, "a summary");
    }

    /// A stub that answers to a caller-chosen name, so a test can own its
    /// own calibration key: the table is process-global and the suite runs
    /// in parallel.
    struct NamedStub(&'static str);

    #[async_trait::async_trait]
    impl Provider for NamedStub {
        fn name(&self) -> &str {
            self.0
        }

        async fn chat(
            &self,
            _system: Option<&str>,
            _messages: &[ChatMessage],
            _tools: Option<&[crate::provider::ToolSpec]>,
        ) -> anyhow::Result<crate::provider::ChatResponse> {
            Ok(crate::provider::ChatResponse::text_only("a summary".into()))
        }
    }

    fn usage(provider: &str, tokens: u32) -> PromptUsage {
        PromptUsage {
            provider: provider.to_string(),
            tokens,
        }
    }

    fn big_tool_spec(name: &'static str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "d".repeat(4_000).into(),
            input_schema: serde_json::json!({ "type": "object", "blurb": "s".repeat(4_000) }),
        }
    }

    #[test]
    fn tool_schemas_are_counted_and_scale_with_the_tool_list() {
        assert_eq!(estimate_tools_tokens(None), 0);
        let one = estimate_tools_tokens(Some(&[big_tool_spec("a")]));
        let two = estimate_tools_tokens(Some(&[big_tool_spec("a"), big_tool_spec("b")]));
        assert!(one > 1_000, "a 8k-char schema is not free: {one}");
        assert!(two > one * 3 / 2, "{two} should be about twice {one}");
    }

    #[test]
    fn calibration_folds_in_what_the_provider_reports() {
        let key = "calib-folds";
        assert_eq!(
            calibration_for(&[key]),
            1.0,
            "unknown providers start at 1.0"
        );

        record_prompt_usage(10_000, Some(&usage(key, 30_000)));
        let after_one = calibration_for(&[key]);
        // One observation of 3.0, smoothed from 1.0.
        assert!(
            (after_one - 1.6).abs() < 0.01,
            "expected ~1.6 after one 3.0 observation, got {after_one}"
        );

        // Repeated agreement converges toward the observed ratio.
        for _ in 0..20 {
            record_prompt_usage(10_000, Some(&usage(key, 30_000)));
        }
        let settled = calibration_for(&[key]);
        assert!(
            settled > 2.9 && settled <= 3.0,
            "expected convergence on 3.0, got {settled}"
        );
    }

    #[test]
    fn calibration_ignores_what_it_cannot_learn_from() {
        let key = "calib-ignores";
        record_prompt_usage(10_000, None);
        record_prompt_usage(10, Some(&usage(key, 5_000))); // too small to mean anything
        record_prompt_usage(10_000, Some(&usage(key, 0))); // a server reporting nothing
        assert_eq!(calibration_for(&[key]), 1.0);
    }

    /// A fallback chain shares one budget but not one tokenizer, so the
    /// budget takes the worse of the two.
    #[test]
    fn a_chain_of_keys_takes_the_largest_factor() {
        record_prompt_usage(10_000, Some(&usage("calib-chain-cheap", 10_000)));
        for _ in 0..20 {
            record_prompt_usage(10_000, Some(&usage("calib-chain-dear", 25_000)));
        }
        let dear = calibration_for(&["calib-chain-dear"]);
        assert!(dear > 2.0, "{dear}");
        assert_eq!(
            calibration_for(&["calib-chain-cheap", "calib-chain-dear"]),
            dear
        );
    }

    /// The regression this whole path exists for: an estimate that fits the
    /// threshold on paper while the provider counts far more. Once the
    /// provider has said so once, the same history compacts.
    #[tokio::test]
    async fn a_history_that_only_fits_on_paper_compacts_once_the_provider_says_otherwise() {
        let key = "calib-triggers";
        let config = CompressionConfig {
            enabled: true,
            context_window: 100_000,
            threshold: 0.8, // 80_000
            preserve_recent: 2,
        };
        let mut messages: Vec<ChatMessage> = (0..10)
            .map(|i| ChatMessage::user(format!("turn {i}")))
            .collect();
        messages.push(tool_use_msg("t1"));
        messages.push(tool_result_msg("t1", &"x".repeat(220_000))); // ~55k raw

        let raw = estimate_total_tokens(None, &messages);
        assert!(
            (50_000..80_000).contains(&raw),
            "fixture must fit the threshold on paper: {raw}"
        );
        assert!(
            maybe_compress(&NamedStub(key), None, &messages, None, &config)
                .await
                .unwrap()
                .is_none(),
            "uncalibrated, this history looks like it fits"
        );

        // The provider reports 1.6x what we estimated for a comparable request.
        record_prompt_usage(10_000, Some(&usage(key, 30_000)));
        let factor = calibration_for(&[key]);

        let result = maybe_compress(&NamedStub(key), None, &messages, None, &config)
            .await
            .unwrap()
            .expect("calibrated, the same history is over budget");

        let after = estimate_total_tokens(None, &result.compressed);
        assert!(
            (after as f64 * factor) as usize <= 80_000,
            "~{after} raw x {factor:.2} must land inside the threshold"
        );
    }

    /// Tool schemas are budget too: the same history fits without them and
    /// does not fit with them.
    #[tokio::test]
    async fn tool_schemas_count_against_the_budget() {
        let config = CompressionConfig {
            enabled: true,
            context_window: 100_000,
            threshold: 0.8, // 80_000
            preserve_recent: 2,
        };
        let messages = vec![
            ChatMessage::user("go"),
            tool_use_msg("t1"),
            tool_result_msg("t1", &"x".repeat(300_000)), // ~75k raw
        ];
        assert!(
            maybe_compress(&NamedStub("tools-budget"), None, &messages, None, &config)
                .await
                .unwrap()
                .is_none(),
            "without tool schemas this history fits"
        );

        let tools: Vec<ToolSpec> = (0..4).map(|_| big_tool_spec("t")).collect();
        assert!(
            maybe_compress(
                &NamedStub("tools-budget"),
                None,
                &messages,
                Some(&tools),
                &config
            )
            .await
            .unwrap()
            .is_some(),
            "with them it does not"
        );
    }

    /// A window small enough that a single fat tool result blows past it,
    /// with a `preserve_recent` large enough to protect everything.
    fn tight_config() -> CompressionConfig {
        CompressionConfig {
            enabled: true,
            context_window: 1_000,
            threshold: 0.8,
            preserve_recent: 20,
        }
    }

    fn tool_result_msg(id: &str, content: &str) -> ChatMessage {
        ChatMessage::tool_results_with_images(vec![(id.into(), content.into())], vec![])
    }

    fn tool_use_msg(id: &str) -> ChatMessage {
        ChatMessage::assistant_with_tools(
            None,
            vec![crate::provider::ToolCall {
                id: id.into(),
                name: "search".into(),
                input: serde_json::json!({}),
            }],
        )
    }

    fn tool_result_text(msg: &ChatMessage) -> String {
        msg.parts
            .iter()
            .find_map(|p| match p {
                ContentPart::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("a tool result")
    }

    /// The overflow this whole path exists for: one tool result arrives
    /// mid-turn that is larger than the window, in a session too short to
    /// have anything worth summarizing. Compaction used to bail out here
    /// (`split == 0`) and hand the provider a request it would reject.
    #[tokio::test]
    async fn a_huge_tool_result_is_trimmed_even_when_there_is_nothing_to_summarize() {
        let config = tight_config();
        let messages = vec![
            ChatMessage::user("find it"),
            tool_use_msg("t1"),
            tool_result_msg("t1", &"x".repeat(400_000)),
        ];

        let result = maybe_compress(&StubProvider, None, &messages, None, &config)
            .await
            .unwrap()
            .expect("an oversized history must come back trimmed");

        assert!(
            result.summary.is_none(),
            "nothing was old enough to summarize, so there is no checkpoint to record"
        );
        let budget = 800; // 1000 * 0.8
        let after = estimate_total_tokens(None, &result.compressed);
        assert!(
            after <= budget,
            "still ~{after} tokens against a budget of {budget}"
        );
        assert_eq!(
            result.compressed.len(),
            messages.len(),
            "trimming cuts contents; it must not drop messages and orphan a tool result"
        );
        assert!(tool_result_text(&result.compressed[2]).contains("trimmed"));
    }

    /// The failure that follows a *successful* compaction: the summary
    /// absorbs the old messages, and the preserved window it hands back is
    /// still over the window all by itself.
    #[tokio::test]
    async fn a_preserved_window_that_is_itself_over_budget_is_trimmed_too() {
        let config = tight_config();
        let mut messages: Vec<ChatMessage> = (0..25)
            .map(|i| {
                if i % 2 == 0 {
                    ChatMessage::user(format!("question {i}"))
                } else {
                    ChatMessage::assistant(format!("answer {i}"))
                }
            })
            .collect();
        messages.push(tool_use_msg("t1"));
        messages.push(tool_result_msg("t1", &"x".repeat(400_000)));

        let result = maybe_compress(&StubProvider, None, &messages, None, &config)
            .await
            .unwrap()
            .expect("an oversized history must come back compressed");

        assert_eq!(result.summary.as_deref(), Some("a summary"));
        let budget = 800;
        let after = estimate_total_tokens(None, &result.compressed);
        assert!(
            after <= budget,
            "still ~{after} tokens against a budget of {budget}"
        );
        assert!(
            matches!(
                result.compressed.last().unwrap().parts.first(),
                Some(ContentPart::ToolResult { .. })
            ),
            "the trimmed tool result must still be there to answer its call"
        );
    }

    /// Water-filling, not a flat cut: the trim takes what it needs from the
    /// one part that is over the line and leaves the rest of the window
    /// exactly as it was.
    #[tokio::test]
    async fn trimming_takes_from_the_giant_and_leaves_the_small_results_alone() {
        let config = tight_config();
        let messages = vec![
            ChatMessage::user("go"),
            tool_use_msg("t1"),
            tool_result_msg("t1", "a small result"),
            tool_use_msg("t2"),
            tool_result_msg("t2", &"x".repeat(400_000)),
        ];

        let result = maybe_compress(&StubProvider, None, &messages, None, &config)
            .await
            .unwrap()
            .expect("an oversized history must come back trimmed");

        assert_eq!(
            tool_result_text(&result.compressed[2]),
            "a small result",
            "the small result was never the problem"
        );
        assert!(tool_result_text(&result.compressed[4]).contains("trimmed"));
    }

    /// The estimator prices CJK at two thirds of a token per character, so a
    /// token cap lands on some byte offset it never computed. Cutting there
    /// has to stay on a character boundary — the same panic
    /// `generate_summary` guards against, one cutoff over.
    #[tokio::test]
    async fn a_huge_cjk_tool_result_is_trimmed_without_panicking() {
        let config = tight_config();
        let messages = vec![
            ChatMessage::user("探して"),
            tool_use_msg("t1"),
            tool_result_msg("t1", &"日本語".repeat(50_000)),
        ];

        let result = maybe_compress(&StubProvider, None, &messages, None, &config)
            .await
            .unwrap()
            .expect("an oversized history must come back trimmed");

        let after = estimate_total_tokens(None, &result.compressed);
        assert!(
            after <= 800,
            "still ~{after} tokens against a budget of 800"
        );
        assert!(tool_result_text(&result.compressed[2]).starts_with("日本語"));
    }

    /// A history that fits is left alone — the trim must not fire on every
    /// turn just because it exists.
    #[tokio::test]
    async fn a_history_under_the_threshold_is_left_untouched() {
        let config = tight_config();
        let messages = vec![ChatMessage::user("hi"), ChatMessage::assistant("hello")];
        assert!(
            maybe_compress(&StubProvider, None, &messages, None, &config)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn truncation_reports_what_it_removed_and_stops_under_the_cap() {
        let text = "y".repeat(10_000);
        let cut = truncate_to_token_budget(&text, 100);
        assert!(estimate_tokens(&cut) <= 100 + TRIM_MARKER_TOKENS);
        assert!(cut.contains("chars omitted"));
    }

    /// The stub has one generator so the compaction path and the restore
    /// path cannot drift into producing different shapes for the same
    /// thing.
    #[test]
    fn the_stub_is_a_user_message_carrying_the_summary_and_an_assistant_ack() {
        let stub = compaction_stub("we fixed the parser");
        assert_eq!(stub.len(), 2);
        assert_eq!(stub[0].role, Role::User);
        assert_eq!(stub[1].role, Role::Assistant);
        assert!(
            matches!(&stub[0].parts[0], ContentPart::Text(t) if t.contains("we fixed the parser")),
            "the summary must be in the user message: {:?}",
            stub[0]
        );
    }
}
