use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sapphire_framework::workspace::{FtsQuery, VectorQuery, WorkspaceState};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

fn lock(state: &Mutex<WorkspaceState>) -> std::sync::MutexGuard<'_, WorkspaceState> {
    state.lock().expect("WorkspaceState mutex poisoned")
}

// ---------------------------------------------------------------------------
// Active memory namespace
// ---------------------------------------------------------------------------
//
// The memory tool needs to know which namespace to write under
// (`memory/<namespace>/<category>/<slug>.md`). The choice is per chat
// turn — Matrix/Discord rooms and pinned API sessions each map to their
// own namespace via Config — but the Tool trait's `execute(input)` API
// has no per-call context. Bridging that with a `tokio::task_local` lets
// the agent and serve loops scope each tool execution without changing
// the trait, and keeps the default ("default") behaviour for any future
// caller that forgets to scope.

tokio::task_local! {
    static MEMORY_NAMESPACE_TL: String;
}

/// Run `fut` with `MEMORY_NAMESPACE_TL` set to `namespace`. Used by the
/// agent / API serve loops to scope per-turn tool executions to the
/// caller's namespace.
pub fn scope_memory_namespace<F: std::future::Future>(
    namespace: String,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    MEMORY_NAMESPACE_TL.scope(namespace, fut)
}

/// The namespace this turn's tool calls should read/write under. Falls
/// back to `"default"` when called outside a `scope_memory_namespace`.
///
/// `pub(crate)` rather than private: `tools::subagent` reads it too, so
/// a subagent's own memory-tool calls land in the same namespace as the
/// conversation that delegated to it, the same way `scope_memory_namespace`
/// already scopes every other tool call in a turn.
pub(crate) fn current_memory_namespace() -> String {
    MEMORY_NAMESPACE_TL
        .try_with(|s| s.clone())
        .unwrap_or_else(|_| crate::config::DEFAULT_NAMESPACE_NAME.to_string())
}

// ---------------------------------------------------------------------------
// memory
// ---------------------------------------------------------------------------

/// Validate a `category` or `slug` segment: non-empty, no path separators,
/// no parent-dir components, no leading dot.
fn validate_segment(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("{kind} must not be empty");
    }
    if value == "." || value == ".." {
        anyhow::bail!("{kind} must not be '.' or '..'");
    }
    if value.starts_with('.') {
        anyhow::bail!("{kind} must not start with '.'");
    }
    if value.contains('/') || value.contains('\\') || value.contains('\0') {
        anyhow::bail!("{kind} must not contain path separators: {value:?}");
    }
    Ok(())
}

const MEMORY_CATEGORY_GUIDE: &str = "Category is mandatory and free-form, but prefer these conventions: \
     'daily' (date-stamped daily logs, slug = YYYY-MM-DD), \
     'dictionary' (short term/definition lookups for names, acronyms, jargon), \
     'knowledge' (longer-form facts, procedures, decisions, learnings — default when unsure). \
     Other categories (e.g. 'recipe', 'project') may be introduced freely.";

fn memory_entry_path(
    state: &Mutex<WorkspaceState>,
    category: &str,
    slug: &str,
) -> Result<(String, std::path::PathBuf)> {
    validate_segment("category", category)?;
    validate_segment("slug", slug)?;
    let namespace = current_memory_namespace();
    let rel = format!("memory/{namespace}/{category}/{slug}.md");
    let abs = lock(state).workspace.root.join(&rel);
    Ok((rel, abs))
}

/// YAML frontmatter tracked on each memory entry file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoryMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_read_at: Option<DateTime<Utc>>,
    #[serde(default)]
    read_count: u64,
}

/// Parse a memory file into `(meta, body)`. Files without frontmatter yield
/// a default `MemoryMeta` and the full raw content as body (enables seamless
/// migration of pre-existing files).
fn parse_memory_file(raw: &str) -> (MemoryMeta, String) {
    match crate::frontmatter::split(raw) {
        Some((fm, body)) => {
            let meta: MemoryMeta = serde_yaml::from_str(fm).unwrap_or_default();
            let body = body.trim_start_matches(['\n', '\r']).to_string();
            (meta, body)
        }
        None => (MemoryMeta::default(), raw.to_string()),
    }
}

/// Serialize `(meta, body)` back into a Markdown file with YAML frontmatter.
fn serialize_memory_file(meta: &MemoryMeta, body: &str) -> Result<String> {
    let fm = serde_yaml::to_string(meta).context("failed to serialize memory frontmatter")?;
    let body_trimmed = body.trim_start_matches(['\n', '\r']);
    Ok(format!("---\n{fm}---\n\n{body_trimmed}"))
}

fn memory_entry_schema(include_content: bool) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert(
        "category".into(),
        json!({
            "type": "string",
            "description": "Category directory under memory/, e.g. \"knowledge\", \"dictionary\", \"daily\"."
        }),
    );
    props.insert(
        "slug".into(),
        json!({
            "type": "string",
            "description": "File stem (without .md), e.g. \"sapphire-agent-memory-design\" or \"2026-04-10\"."
        }),
    );
    let mut required = vec!["category", "slug"];
    if include_content {
        props.insert(
            "content".into(),
            json!({
                "type": "string",
                "description": "Full file content."
            }),
        );
        required.push("content");
    }
    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

// -- memory_add --------------------------------------------------------------

/// Create a new one-file memory entry under `memory/<category>/<slug>.md`.
pub struct MemoryAddTool {
    state: Arc<Mutex<WorkspaceState>>,
    spec: ToolSpec,
}

impl MemoryAddTool {
    pub fn new(state: Arc<Mutex<WorkspaceState>>) -> Self {
        let description = format!(
            "Create a new memory entry at memory/<category>/<slug>.md. \
             Fails if the file already exists (use memory_update to overwrite). \
             Each file holds one self-contained entry retrievable via workspace_search. {MEMORY_CATEGORY_GUIDE}"
        );
        Self {
            state,
            spec: ToolSpec {
                name: "memory_add".into(),
                description: description.into(),
                input_schema: memory_entry_schema(true),
            },
        }
    }
}

#[async_trait]
impl Tool for MemoryAddTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let content = input["content"].as_str().context("missing 'content'")?;
        let (rel, abs) = memory_entry_path(&self.state, category, slug)?;
        if abs.exists() {
            anyhow::bail!("{rel} already exists; use memory_update to overwrite");
        }
        let now = Utc::now();
        let meta = MemoryMeta {
            created_at: Some(now),
            updated_at: Some(now),
            last_read_at: None,
            read_count: 0,
        };
        let serialized = serialize_memory_file(&meta, content)?;
        lock(&self.state)
            .write_file(Path::new(&rel), &serialized)
            .with_context(|| format!("Failed to write {rel}"))?;
        Ok(format!("Created {rel}"))
    }
}

// -- memory_update -----------------------------------------------------------

/// Overwrite an existing memory entry.
pub struct MemoryUpdateTool {
    state: Arc<Mutex<WorkspaceState>>,
    spec: ToolSpec,
}

impl MemoryUpdateTool {
    pub fn new(state: Arc<Mutex<WorkspaceState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "memory_update".into(),
                description: "Overwrite an existing memory entry at \
                    memory/<category>/<slug>.md. Fails if the file does not exist \
                    (use memory_add to create it)."
                    .into(),
                input_schema: memory_entry_schema(true),
            },
        }
    }
}

#[async_trait]
impl Tool for MemoryUpdateTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let content = input["content"].as_str().context("missing 'content'")?;
        let (rel, abs) = memory_entry_path(&self.state, category, slug)?;
        if !abs.exists() {
            anyhow::bail!("{rel} does not exist; use memory_add to create it");
        }
        let raw = std::fs::read_to_string(&abs).with_context(|| format!("Failed to read {rel}"))?;
        let (mut meta, _old_body) = parse_memory_file(&raw);
        let now = Utc::now();
        meta.updated_at = Some(now);
        if meta.created_at.is_none() {
            meta.created_at = Some(now);
        }
        let serialized = serialize_memory_file(&meta, content)?;
        lock(&self.state)
            .write_file(Path::new(&rel), &serialized)
            .with_context(|| format!("Failed to write {rel}"))?;
        Ok(format!("Updated {rel}"))
    }
}

// -- memory_append -----------------------------------------------------------

/// Append to a memory entry, creating it if missing.
pub struct MemoryAppendTool {
    state: Arc<Mutex<WorkspaceState>>,
    spec: ToolSpec,
}

impl MemoryAppendTool {
    pub fn new(state: Arc<Mutex<WorkspaceState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "memory_append".into(),
                description: "Append content to the end of a memory entry at \
                    memory/<category>/<slug>.md, creating the file if it does \
                    not exist. Cheaper than memory_read + memory_update when you \
                    just want to tack a new observation onto an existing note. \
                    A blank line is inserted between the existing body and the \
                    new content; add your own Markdown heading if you want a \
                    section break. Frontmatter counters (updated_at) are \
                    maintained automatically."
                    .into(),
                input_schema: memory_entry_schema(true),
            },
        }
    }
}

#[async_trait]
impl Tool for MemoryAppendTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let content = input["content"].as_str().context("missing 'content'")?;
        let (rel, abs) = memory_entry_path(&self.state, category, slug)?;
        let now = Utc::now();

        let (meta, new_body, created) = if abs.exists() {
            let raw =
                std::fs::read_to_string(&abs).with_context(|| format!("Failed to read {rel}"))?;
            let (mut meta, old_body) = parse_memory_file(&raw);
            meta.updated_at = Some(now);
            if meta.created_at.is_none() {
                meta.created_at = Some(now);
            }
            let trimmed = old_body.trim_end_matches(['\n', '\r']);
            let new_body = if trimmed.is_empty() {
                content.to_string()
            } else {
                format!("{trimmed}\n\n{content}")
            };
            (meta, new_body, false)
        } else {
            let meta = MemoryMeta {
                created_at: Some(now),
                updated_at: Some(now),
                last_read_at: None,
                read_count: 0,
            };
            (meta, content.to_string(), true)
        };

        let serialized = serialize_memory_file(&meta, &new_body)?;
        lock(&self.state)
            .write_file(Path::new(&rel), &serialized)
            .with_context(|| format!("Failed to write {rel}"))?;
        Ok(if created {
            format!("Created {rel}")
        } else {
            format!("Appended to {rel}")
        })
    }
}

// -- memory_read -------------------------------------------------------------

/// Read a memory entry and bump its access counters.
pub struct MemoryReadTool {
    state: Arc<Mutex<WorkspaceState>>,
    spec: ToolSpec,
}

impl MemoryReadTool {
    pub fn new(state: Arc<Mutex<WorkspaceState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "memory_read".into(),
                description: "Read a memory entry at memory/<category>/<slug>.md \
                    and return its body. Side effect: updates the file's \
                    frontmatter (last_read_at = now, read_count += 1) so that \
                    recency and frequency can inform future weighting. \
                    Use file_read instead for a non-tracking read."
                    .into(),
                input_schema: memory_entry_schema(false),
            },
        }
    }
}

#[async_trait]
impl Tool for MemoryReadTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let (rel, abs) = memory_entry_path(&self.state, category, slug)?;
        if !abs.exists() {
            anyhow::bail!("{rel} does not exist");
        }
        let raw = std::fs::read_to_string(&abs).with_context(|| format!("Failed to read {rel}"))?;
        let (mut meta, body) = parse_memory_file(&raw);
        meta.last_read_at = Some(Utc::now());
        meta.read_count = meta.read_count.saturating_add(1);
        let serialized = serialize_memory_file(&meta, &body)?;
        if let Err(e) = lock(&self.state).write_file(Path::new(&rel), &serialized) {
            tracing::warn!("memory_read: failed to persist counters for {rel}: {e:#}");
        }
        Ok(body)
    }
}

// -- memory_remove -----------------------------------------------------------

/// Delete a memory entry.
pub struct MemoryRemoveTool {
    state: Arc<Mutex<WorkspaceState>>,
    spec: ToolSpec,
}

impl MemoryRemoveTool {
    pub fn new(state: Arc<Mutex<WorkspaceState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "memory_remove".into(),
                description: "Delete a memory entry at memory/<category>/<slug>.md.".into(),
                input_schema: memory_entry_schema(false),
            },
        }
    }
}

#[async_trait]
impl Tool for MemoryRemoveTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Delete
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let (rel, abs) = memory_entry_path(&self.state, category, slug)?;
        if !abs.exists() {
            anyhow::bail!("{rel} does not exist");
        }
        std::fs::remove_file(&abs).with_context(|| format!("Failed to remove {rel}"))?;
        Ok(format!("Removed {rel}"))
    }
}

// ---------------------------------------------------------------------------
// workspace_search
// ---------------------------------------------------------------------------

/// Full-text and semantic search across workspace files.
pub struct WorkspaceSearchTool {
    state: Arc<Mutex<WorkspaceState>>,
    spec: ToolSpec,
}

impl WorkspaceSearchTool {
    pub fn new(state: Arc<Mutex<WorkspaceState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "workspace_search".into(),
                description: "Search across all indexed files in the workspace. \
                    Two modes are available: \
                    'fts' (full-text / BM25, always available) and \
                    'semantic' (vector similarity, requires an embedder to be configured — falls back to fts if unavailable). \
                    Returns matching file titles and paths.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query."
                        },
                        "mode": {
                            "type": "string",
                            "description": "Search mode: 'fts' (full-text, default) or 'semantic' (vector similarity).",
                            "enum": ["fts", "semantic"],
                            "default": "fts"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of results (default 10).",
                            "default": 10
                        }
                    },
                    "required": ["query"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for WorkspaceSearchTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let query = input["query"].as_str().context("missing 'query'")?;
        let limit = input["limit"].as_u64().unwrap_or(10) as usize;
        let mode = input["mode"].as_str().unwrap_or("fts");

        let state = lock(&self.state);

        if mode == "semantic"
            && let Some(embedder) = state.embedder()
        {
            let vq = VectorQuery::new(query, &*embedder).limit(limit);
            let results = state
                .retrieve_db()
                .search_similar(&vq)
                .context("Vector similarity search failed")?;

            if results.is_empty() {
                return Ok("No results found.".to_string());
            }
            let lines: Vec<String> = results
                .iter()
                .map(|r| format!("- {} [score: {:.4}]", r.path, r.score))
                .collect();
            return Ok(format!("[semantic]\n{}", lines.join("\n")));
        }

        // FTS (default or fallback)
        let fq = FtsQuery::new(query).limit(limit);
        let results = state
            .retrieve_db()
            .search_fts(&fq)
            .context("FTS search failed")?;

        if results.is_empty() {
            return Ok("No results found.".to_string());
        }

        let header = if mode == "semantic" {
            "[fts — semantic fallback: no embedder configured]\n"
        } else {
            "[fts]\n"
        };
        let lines: Vec<String> = results.iter().map(|r| format!("- {}", r.path)).collect();
        Ok(format!("{}{}", header, lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// workspace_sync
// ---------------------------------------------------------------------------

/// Re-index the workspace so files edited outside the agent become
/// searchable. There is no git leg — the framework removed auto-sync.
pub struct WorkspaceSyncTool {
    state: Arc<Mutex<WorkspaceState>>,
    spec: ToolSpec,
}

impl WorkspaceSyncTool {
    pub fn new(state: Arc<Mutex<WorkspaceState>>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "workspace_sync".into(),
                description: "Re-index the workspace so files changed outside \
                    the agent become searchable. Does not touch git."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for WorkspaceSyncTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
        let state = lock(&self.state);

        let (upserted, removed) = state
            .sync_retrieve()
            .context("Failed to re-index workspace")?;
        Ok(format!(
            "Synced: {upserted} files indexed, {removed} removed."
        ))
    }
}
