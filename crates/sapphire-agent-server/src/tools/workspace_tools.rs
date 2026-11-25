use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind};
use anyhow::{Context, Result};
use async_trait::async_trait;
use jiff::Timestamp;
use sapphire_framework::workspace::{FtsQuery, VectorQuery, WorkspaceState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

fn lock(state: &WorkspaceState) -> &WorkspaceState {
    state
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
    static MEMORY_NAMESPACE_TL: Vec<String>;
}

/// Run `fut` with `MEMORY_NAMESPACE_TL` set to `chain`: the caller's
/// namespace first, then every namespace it reaches through `include`
/// (`Config::resolve_namespace_chain`). Used by the agent / API serve
/// loops to scope per-turn tool executions. The memory tools default to
/// the first entry and may be pointed at any other.
pub fn scope_memory_namespace<F: std::future::Future>(
    chain: Vec<String>,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    MEMORY_NAMESPACE_TL.scope(chain, fut)
}

/// The namespaces this turn's memory tools may use, own first. Falls back
/// to `["default"]` outside a `scope_memory_namespace` (or for an empty
/// scope).
pub(crate) fn current_memory_chain() -> Vec<String> {
    MEMORY_NAMESPACE_TL
        .try_with(|chain| chain.clone())
        .ok()
        .filter(|chain| !chain.is_empty())
        .unwrap_or_else(|| vec![crate::config::DEFAULT_NAMESPACE_NAME.to_string()])
}

/// The namespace this turn's tool calls should read/write under. Falls
/// back to `"default"` when called outside a `scope_memory_namespace`.
///
/// `pub(crate)` rather than private: `tools::subagent` reads it too, so
/// a subagent's own memory-tool calls land in the same namespace as the
/// conversation that delegated to it, the same way `scope_memory_namespace`
/// already scopes every other tool call in a turn.
pub(crate) fn current_memory_namespace() -> String {
    current_memory_chain().swap_remove(0)
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

/// `memory/<namespace>/<category>/<slug>.md`, relative and absolute.
/// `namespace` absent means this turn's own; present, it must be one of
/// `current_memory_chain()`.
/// The optional `namespace` argument: absent or `null` means this
/// conversation's own namespace; anything but a string is an error rather
/// than a silent fall-back to the own namespace.
fn namespace_arg(input: &Value) -> Result<Option<&str>> {
    match input.get("namespace") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(ns)) => Ok(Some(ns)),
        Some(other) => anyhow::bail!("namespace must be a string, got {other}"),
    }
}

fn memory_entry_path(
    state: &WorkspaceState,
    namespace: Option<&str>,
    category: &str,
    slug: &str,
) -> Result<(String, std::path::PathBuf)> {
    validate_segment("category", category)?;
    validate_segment("slug", slug)?;
    let chain = current_memory_chain();
    let namespace = match namespace {
        None => chain[0].clone(),
        Some(ns) => {
            validate_segment("namespace", ns)?;
            if !chain.iter().any(|c| c == ns) {
                anyhow::bail!(
                    "namespace '{ns}' is not available here; choose one of: {}",
                    chain.join(", ")
                );
            }
            ns.to_string()
        }
    };
    let rel = format!("memory/{namespace}/{category}/{slug}.md");
    let abs = lock(state).workspace.root.join(&rel);
    Ok((rel, abs))
}

/// YAML frontmatter tracked on each memory entry file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoryMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_read_at: Option<Timestamp>,
    #[serde(default)]
    read_count: u64,
}

/// Parse a memory file into `(meta, body)`. Files without frontmatter yield
/// a default `MemoryMeta` and the full raw content as body (enables seamless
/// migration of pre-existing files).
fn parse_memory_file(raw: &str) -> (MemoryMeta, String) {
    match crate::frontmatter::split(raw) {
        Some((fm, body)) => {
            let meta: MemoryMeta = serde_norway::from_str(fm).unwrap_or_default();
            let body = body.trim_start_matches(['\n', '\r']).to_string();
            (meta, body)
        }
        None => (MemoryMeta::default(), raw.to_string()),
    }
}

/// Serialize `(meta, body)` back into a Markdown file with YAML frontmatter.
fn serialize_memory_file(meta: &MemoryMeta, body: &str) -> Result<String> {
    let fm = serde_norway::to_string(meta).context("failed to serialize memory frontmatter")?;
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
    props.insert(
        "namespace".into(),
        json!({
            "type": "string",
            "description": "Memory namespace to use, from the list under # Memory. Omit for this conversation's own namespace."
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
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl MemoryAddTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        let description = format!(
            "Create a new memory entry at memory/<category>/<slug>.md. \
             Fails if the file already exists (use memory_update to overwrite). \
             Each file holds one self-contained entry retrievable via workspace_search (when file tools are available). {MEMORY_CATEGORY_GUIDE}"
        );
        Self {
            state,
            spec: ToolSpec {
                kind: ToolKind::Edit,
                group: None,
                name: "memory_add".into(),
                description: description.into(),
                input_schema: memory_entry_schema(true),
            },
        }
    }
}

#[async_trait]
impl Tool for MemoryAddTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let content = input["content"].as_str().context("missing 'content'")?;
        let (rel, abs) = memory_entry_path(&self.state, namespace_arg(input)?, category, slug)?;
        if abs.exists() {
            anyhow::bail!("{rel} already exists; use memory_update to overwrite");
        }
        let now = Timestamp::now();
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
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl MemoryUpdateTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                kind: ToolKind::Edit,
                group: None,
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
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let content = input["content"].as_str().context("missing 'content'")?;
        let (rel, abs) = memory_entry_path(&self.state, namespace_arg(input)?, category, slug)?;
        if !abs.exists() {
            anyhow::bail!("{rel} does not exist; use memory_add to create it");
        }
        let raw = std::fs::read_to_string(&abs).with_context(|| format!("Failed to read {rel}"))?;
        let (mut meta, _old_body) = parse_memory_file(&raw);
        let now = Timestamp::now();
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
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl MemoryAppendTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                kind: ToolKind::Edit,
                group: None,
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
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let content = input["content"].as_str().context("missing 'content'")?;
        let (rel, abs) = memory_entry_path(&self.state, namespace_arg(input)?, category, slug)?;
        let now = Timestamp::now();

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
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl MemoryReadTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                kind: ToolKind::Read,
                group: None,
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
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let (rel, abs) = memory_entry_path(&self.state, namespace_arg(input)?, category, slug)?;
        if !abs.exists() {
            anyhow::bail!("{rel} does not exist");
        }
        let raw = std::fs::read_to_string(&abs).with_context(|| format!("Failed to read {rel}"))?;
        let (mut meta, body) = parse_memory_file(&raw);
        meta.last_read_at = Some(Timestamp::now());
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
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl MemoryRemoveTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                kind: ToolKind::Delete,
                group: None,
                name: "memory_remove".into(),
                description: "Delete a memory entry at memory/<category>/<slug>.md.".into(),
                input_schema: memory_entry_schema(false),
            },
        }
    }
}

#[async_trait]
impl Tool for MemoryRemoveTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let category = input["category"].as_str().context("missing 'category'")?;
        let slug = input["slug"].as_str().context("missing 'slug'")?;
        let (rel, abs) = memory_entry_path(&self.state, namespace_arg(input)?, category, slug)?;
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
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl WorkspaceSearchTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                kind: ToolKind::Search,
                group: Some(crate::config::ToolGroup::Fs),
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
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl WorkspaceSyncTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                kind: ToolKind::Other,
                group: Some(crate::config::ToolGroup::Fs),
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

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_framework::workspace::{AppContext, Workspace};

    static TEST_CTX: AppContext = AppContext::new("sapphire-agent").allow_external_paths();

    fn test_state() -> Arc<WorkspaceState> {
        TEST_CTX.set_cache_dir(std::env::temp_dir().join("sapphire-agent-tools-test-cache"));
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        std::fs::create_dir_all(dir.path().join(".sapphire-agent")).unwrap();
        let ws = Workspace::from_root(&TEST_CTX, dir.path()).unwrap();
        Arc::new(WorkspaceState::open(ws).unwrap())
    }

    fn chain(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn non_string_namespace_is_refused() {
        let state = test_state();
        let tool = MemoryAddTool::new(Arc::clone(&state));
        let input = json!({"namespace": 7, "category": "knowledge", "slug": "x", "content": "hi"});
        let err = scope_memory_namespace(chain(&["mom", "family"]), tool.execute(&input))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("namespace"), "{err}");
        assert!(!root(&state).join("memory/mom/knowledge/x.md").exists());
    }

    #[test]
    fn null_namespace_means_own() {
        assert_eq!(namespace_arg(&json!({"namespace": null})).unwrap(), None);
        assert_eq!(namespace_arg(&json!({})).unwrap(), None);
        assert_eq!(
            namespace_arg(&json!({"namespace": "family"})).unwrap(),
            Some("family")
        );
    }

    fn root(state: &WorkspaceState) -> std::path::PathBuf {
        lock(state).workspace.root.clone()
    }

    #[tokio::test]
    async fn memory_add_defaults_to_own_namespace() {
        let state = test_state();
        let tool = MemoryAddTool::new(Arc::clone(&state));
        let input = json!({"category": "knowledge", "slug": "x", "content": "hi"});
        scope_memory_namespace(chain(&["mom", "family"]), tool.execute(&input))
            .await
            .unwrap();
        assert!(root(&state).join("memory/mom/knowledge/x.md").is_file());
    }

    #[tokio::test]
    async fn memory_add_writes_to_included_namespace() {
        let state = test_state();
        let tool = MemoryAddTool::new(Arc::clone(&state));
        let input =
            json!({"namespace": "family", "category": "knowledge", "slug": "x", "content": "hi"});
        let out = scope_memory_namespace(chain(&["mom", "family"]), tool.execute(&input))
            .await
            .unwrap();
        assert!(out.contains("memory/family/"), "{out}");
        assert!(root(&state).join("memory/family/knowledge/x.md").is_file());
        assert!(!root(&state).join("memory/mom/knowledge/x.md").exists());
    }

    #[tokio::test]
    async fn memory_read_reads_included_namespace() {
        let state = test_state();
        let path = root(&state).join("memory/family/knowledge/x.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "shared fact").unwrap();
        let tool = MemoryReadTool::new(Arc::clone(&state));
        let input = json!({"namespace": "family", "category": "knowledge", "slug": "x"});
        let out = scope_memory_namespace(chain(&["mom", "family"]), tool.execute(&input))
            .await
            .unwrap();
        assert!(out.contains("shared fact"), "{out}");
    }

    #[tokio::test]
    async fn out_of_chain_namespace_is_refused() {
        let state = test_state();
        let tool = MemoryAddTool::new(Arc::clone(&state));
        let input =
            json!({"namespace": "admin", "category": "knowledge", "slug": "x", "content": "hi"});
        let err = scope_memory_namespace(chain(&["mom", "family"]), tool.execute(&input))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("mom, family"), "{err}");
        assert!(!root(&state).join("memory/admin").exists());
    }

    #[tokio::test]
    async fn invalid_namespace_segment_is_refused() {
        let state = test_state();
        let tool = MemoryAddTool::new(Arc::clone(&state));
        for bad in ["", "../default", "a/b"] {
            let input =
                json!({"namespace": bad, "category": "knowledge", "slug": "x", "content": "hi"});
            let res = scope_memory_namespace(chain(&["mom", "family"]), tool.execute(&input)).await;
            assert!(res.is_err(), "{bad:?}");
        }
        assert!(!root(&state).join("memory").exists());
    }

    #[tokio::test]
    async fn transitive_include_is_reachable() {
        let cfg = crate::config::Config::parse_for_test(
            "[memory_namespace.mom]\ninclude = [\"family\"]\n\
             [memory_namespace.family]\ninclude = [\"household\"]\n\
             [memory_namespace.household]\n",
        );
        let state = test_state();
        let tool = MemoryAddTool::new(Arc::clone(&state));
        let input = json!({"namespace": "household", "category": "knowledge", "slug": "x", "content": "hi"});
        scope_memory_namespace(cfg.resolve_namespace_chain("mom"), tool.execute(&input))
            .await
            .unwrap();
        assert!(
            root(&state)
                .join("memory/household/knowledge/x.md")
                .is_file()
        );
    }

    #[tokio::test]
    async fn current_memory_chain_defaults_outside_scope() {
        assert_eq!(current_memory_chain(), chain(&["default"]));
        let inner = scope_memory_namespace(chain(&["mom", "family"]), async {
            (current_memory_namespace(), current_memory_chain())
        })
        .await;
        assert_eq!(inner, ("mom".to_string(), chain(&["mom", "family"])));
    }
}
