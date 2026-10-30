//! MEMORY.md compaction.
//!
//! Reads `<workspace>/memory/<namespace>/MEMORY.md`, asks the provider to
//! merge duplicates and summarise stale entries, and writes the result
//! back. The entry separator `\n\n---\n\n` (used by `MemoryTool`) is
//! preserved by instructing the model. Each namespace is compacted in
//! isolation, so cross-namespace contamination is impossible.

use crate::provider::{ChatMessage, Provider};
use std::path::Path;
use tracing::{info, warn};

const SYSTEM_PROMPT: &str = "You are reorganising an AI agent's persistent memory file (MEMORY.md). \
The file contains independent entries separated by `\\n\\n---\\n\\n` (a Markdown horizontal rule on its own line, blank lines around it). \
\
Your task: \
1. Merge entries that record the same fact (keep the most informative version). \
2. Summarise or compress entries that are clearly stale, verbose, or redundant. \
3. Preserve all unique, useful information. Do NOT invent new facts. \
4. Keep the same `\\n\\n---\\n\\n` separator between entries. \
5. Write in the same language as the original. \
\
Output ONLY the new contents of MEMORY.md, with no extra commentary, no code fences, and no preamble.";

/// Compact `<workspace_dir>/memory/<namespace>/MEMORY.md` in place. No-op
/// if the file doesn't exist or is effectively empty. Errors are logged
/// but never returned.
pub async fn compact_memory(provider: &dyn Provider, workspace_dir: &Path, namespace: &str) {
    let path = workspace_dir
        .join("memory")
        .join(namespace)
        .join("MEMORY.md");
    let original = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            info!("memory_compaction: no MEMORY.md found in namespace '{namespace}', skipping");
            return;
        }
    };

    if original.trim().is_empty() {
        info!("memory_compaction: MEMORY.md in namespace '{namespace}' is empty, skipping");
        return;
    }

    let user_msg = ChatMessage::user(&original);
    let response = match provider.chat(Some(SYSTEM_PROMPT), &[user_msg], None).await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "memory_compaction: provider error for '{namespace}', leaving file unchanged: {e:#}"
            );
            return;
        }
    };

    let new_content = match response.text {
        Some(t) if !t.trim().is_empty() => t,
        _ => {
            warn!("memory_compaction: empty response for '{namespace}', leaving file unchanged");
            return;
        }
    };

    if let Err(e) = std::fs::write(&path, &new_content) {
        warn!("memory_compaction: failed to write MEMORY.md in '{namespace}': {e}");
        return;
    }

    info!(
        "memory_compaction: MEMORY.md in namespace '{}' updated ({} -> {} bytes)",
        namespace,
        original.len(),
        new_content.len()
    );
}
