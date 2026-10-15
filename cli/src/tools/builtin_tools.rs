use crate::image_cache::ImageCache;
use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind, ToolOutput, ToolSet};
use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use sapphire_framework::workspace::WorkspaceState;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

// The one sentence every session-routed shell/file tool's description opens
// with. Which machine a call reaches is a property of the *session*, not of
// the tool: an ACP session reaches the connected editor's machine, every
// other session this agent's own. Naming a fixed machine would be wrong in
// one of the two cases every time, so the sentence lives here once rather
// than in seven drifting copies.
pub(crate) const REACHES_SENTENCE: &str = "Reaches whichever machine this \
    conversation is about: inside an ACP session, the machine the connected \
    editor is running on; otherwise this agent's own machine. ";

fn expand_path(path_str: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path_str).as_ref())
}

// ---------------------------------------------------------------------------
// file_read
// ---------------------------------------------------------------------------

pub struct FileReadTool {
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl FileReadTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "file_read".into(),
                description: format!(
                    "{REACHES_SENTENCE}Read a file with optional line-based pagination. \
                    Accepts absolute paths, ~/... paths, or workspace-relative paths \
                    (resolved against the workspace root). \
                    Returns lines prefixed with their 1-indexed line number in 'N|content' format. \
                    Use offset and limit for large files. \
                    Cannot read binary files or device paths (/dev/, /proc/).",
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path — absolute, ~/..., or relative to the workspace root."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "1-indexed line number to start reading from (default: 1).",
                            "default": 1,
                            "minimum": 1
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to read (default: 500, max: 2000).",
                            "default": 500,
                            "maximum": 2000
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // Which machine this call reaches is decided once, here: a client
        // scoped to this turn means the *editor's* filesystem, and no client
        // means this agent's own — the pre-#270 behaviour, kept for every
        // non-ACP transport. There is no fallback in either direction; see
        // `client_tools`' module doc.
        if let Some(client) = crate::tools::acp_client::current_acp_client() {
            return crate::tools::client_tools::client_read(&client, input).await;
        }

        let path_str = input["path"].as_str().context("missing 'path'")?;
        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().unwrap_or(500).min(2000) as usize;

        let path = expand_path(path_str);
        let path_abs = path.to_string_lossy();

        if path_abs.starts_with("/dev/") || path_abs.starts_with("/proc/") {
            anyhow::bail!("Reading device or proc paths is not allowed.");
        }

        let content = self
            .state
            .read_file(&path)
            .with_context(|| format!("Failed to read '{}'", path.display()))?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let start = offset.saturating_sub(1);

        if start >= total_lines && total_lines > 0 {
            anyhow::bail!(
                "offset {} exceeds file length ({} lines)",
                offset,
                total_lines
            );
        }

        let end = (start + limit).min(total_lines);
        let mut result = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{}|{}", start + i + 1, l))
            .collect::<Vec<_>>()
            .join("\n");

        if end < total_lines {
            result.push_str(&format!(
                "\n[{} more lines — use offset={} to continue]",
                total_lines - end,
                end + 1
            ));
        }

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// file_write
// ---------------------------------------------------------------------------

static SENSITIVE_PREFIXES: &[&str] = &[
    "/etc/",
    "/boot/",
    "/usr/lib/",
    "/usr/bin/",
    "/usr/sbin/",
    "/bin/",
    "/sbin/",
    "/sys/",
    "/proc/",
    "/run/docker.sock",
    "/var/run/docker.sock",
];

/// Files the agent must not be able to rewrite through its own tools.
///
/// This is not merely tidy. `file_write` is `ToolKind::Edit`, which the
/// permission policy allows *without asking* from a chat channel and in
/// ACP's `accept_edits` mode. Unguarded, a Discord message could write
/// `{"profiles":{"zed":{"always_allow":["shell"]}}}` into the
/// permission record; the store is read once at startup, so after the
/// next restart the editor would run `shell` having never been asked.
/// The gate would still be doing its job — it would just be consulting
/// an answer somebody else wrote.
///
/// Named files rather than the whole config directory, deliberately.
/// When `workspace_dir` is unset the workspace root *is* the config
/// file's own directory, so blocking the directory would refuse every
/// write in the workspace on a default install.
static PROTECTED_CONFIG_FILES: &[&str] = &["acp-permissions.json", "config.toml"];

/// The agent's own config directory, if this host has one.
fn agent_config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "sapphire-agent")
        .map(|dirs| dirs.config_dir().to_path_buf())
}

/// Collapse `.` and `..` without touching the filesystem.
///
/// `canonicalize` needs every component to exist, so
/// `nope/../acp-permissions.json` defeats it when `nope` does not; a
/// guard that then falls back to the raw path carries an un-collapsed
/// `..` into the comparison and matches nothing, while the writer's
/// `create_dir_all` makes `nope` and the kernel collapses the `..` at
/// open time.
///
/// Note the deliberate imprecision: collapsing `..` lexically disagrees
/// with the kernel whenever a symlink sits in the path, because the
/// kernel resolves the link first. That direction is accepted — see
/// `protected_leaf`, which does not rely on the whole path matching.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                // An ordinary component: this `..` cancels it.
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // A root or drive prefix: `/..` is `/`. Drop it — it
                // cannot climb, and keeping it would leave an
                // un-collapsed `..` in a path the guard then compares.
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                // Nothing to cancel and no root to stop at: keep it, so
                // a relative path can still be joined onto a root.
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The filename the operating system will actually open, given the
/// filename as written.
///
/// Windows accepts several spellings of one name and normalises them
/// away before the filesystem sees them, so a guard that compares the
/// string as given is comparing something the OS never uses:
///
/// - `name::$DATA` is the file's default data stream — i.e. the file
///   itself. A colon is never legal in an NTFS filename, so everything
///   from the first one onwards is stream syntax and is cut.
/// - Trailing dots and spaces are stripped during path normalisation,
///   so `name.` and `name ` both open `name`.
///
/// Returns `None` when nothing is left, which is not a filename any
/// spelling of a protected file can reduce to.
fn opened_filename(name: &std::ffi::OsStr) -> Option<String> {
    let name = name.to_string_lossy();
    if !cfg!(windows) {
        return Some(name.into_owned());
    }
    let before_stream = name.split(':').next().unwrap_or("");
    let trimmed = before_stream.trim_end_matches(['.', ' ']);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Canonicalise as much of `path` as exists, keeping the rest.
///
/// The same walk-up the framework's `canonicalize_or_parent` does, and
/// for the same reason: a write's target usually does not exist, and
/// often neither do the directories above it — `write_file` calls
/// `create_dir_all`. Returning the raw path when `canonicalize` fails
/// is what let two earlier versions of this guard be talked past, so
/// this never gives up: it finds the deepest existing ancestor and
/// re-attaches everything below it.
fn resolve_as_far_as_possible(path: &Path) -> PathBuf {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();

    loop {
        if let Ok(real) = cursor.canonicalize() {
            let mut out = real;
            for part in suffix.iter().rev() {
                out.push(part);
            }
            return out;
        }
        let Some(name) = cursor.file_name().map(|n| n.to_os_string()) else {
            // Ran out of ancestors without finding one that exists.
            return path.to_path_buf();
        };
        suffix.push(name);
        if !cursor.pop() {
            return path.to_path_buf();
        }
    }
}

/// Two existing paths, same file?
///
/// `fs::metadata` rather than opening the path: opening a FIFO blocks
/// until a writer appears, and the model chooses this path. Metadata
/// follows symlinks, which is what we want — `fs::write` follows them
/// too.
fn same_entry(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        // `same-file` opens a handle, which is why this is gated on the
        // target being an ordinary file first.
        let ordinary = |p: &Path| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false);
        if !(ordinary(a) && ordinary(b)) {
            return false;
        }
        same_file::is_same_file(a, b).unwrap_or(false)
    }
}

/// Whether two directory paths are the same directory.
///
/// Fails **closed**: an answer of "I could not tell" is reported as
/// "yes, treat it as protected". Every previous version of this guard
/// defaulted the other way, and that is what each bypass ultimately
/// exploited — an unresolvable path was read as a safe one.
fn is_protected_dir(candidate: &Path, protected: &Path) -> bool {
    // Resolve BOTH sides the same way. Comparing a canonicalised path
    // against a raw one is how a protected directory that does not
    // exist yet read as "nothing to protect" — the write would have
    // created it, record and all.
    let a = resolve_as_far_as_possible(candidate);
    let b = resolve_as_far_as_possible(protected);
    if a == b {
        return true;
    }
    // Identity settles the rest — a symlinked or otherwise differently
    // spelled directory that exists. `unwrap_or(false)` is safe here
    // only because the lexical comparison above already ran on two
    // paths resolved by the same rules.
    same_file::is_same_file(&a, &b).unwrap_or(false)
}

/// The protected filename this path would write to, if any.
///
/// Compares the *parent* by identity — exact, because a directory a
/// write is headed into either exists or is about to be created at a
/// path we can name — and the *leaf* by the name the OS will open.
/// Neither half depends on the target file existing, which is the
/// property every earlier version lacked.
///
/// Also catches a leaf that merely *points* at a protected file: a
/// symlink (followed once with `read_link`, so a dangling one still
/// resolves) or a hard link (caught by identity, the only thing that
/// can catch it).
fn protected_leaf(
    path: &Path,
    workspace_root: Option<&Path>,
    protected_dir: &Path,
) -> Option<String> {
    use std::path::Component;

    // A drive-relative path (`C:x`, `C:..\x`) resolves against Windows'
    // per-drive current directory — which neither this guard nor the
    // workspace root controls, and which `PathBuf::join` silently
    // discards the root for. No legitimate tool call needs the
    // spelling, so it is refused rather than modelled.
    if !path.is_absolute() && path.components().any(|c| matches!(c, Component::Prefix(_))) {
        return Some("a drive-relative path, which this agent will not write through".to_string());
    }

    let mut candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match workspace_root {
            Some(root) => root.join(path),
            None => path.to_path_buf(),
        }
    };

    // Follow a symlinked leaf, bounded well above any real chain.
    for _ in 0..40 {
        let collapsed = lexically_normalize(&candidate);
        let (Some(parent), Some(leaf)) = (collapsed.parent(), collapsed.file_name()) else {
            return None;
        };

        let resolved_parent = resolve_as_far_as_possible(parent);
        let name = opened_filename(leaf);

        if is_protected_dir(&resolved_parent, protected_dir) {
            // A leaf that normalises to nothing still lands inside the
            // protected directory; refuse rather than reason about it.
            let Some(name) = name.clone() else {
                return Some(
                    "an unnameable entry in this agent's own config directory".to_string(),
                );
            };
            let hit = PROTECTED_CONFIG_FILES.iter().find(|protected| {
                if cfg!(windows) {
                    name.eq_ignore_ascii_case(protected)
                } else {
                    name == **protected
                }
            });
            if let Some(hit) = hit {
                return Some((*hit).to_string());
            }
        }

        // Identity: a hard link is the same file under another name,
        // and no comparison of spellings can see that.
        let target = match &name {
            Some(name) => resolved_parent.join(name),
            None => resolved_parent.clone(),
        };
        for protected in PROTECTED_CONFIG_FILES {
            if same_entry(&target, &protected_dir.join(protected)) {
                return Some((*protected).to_string());
            }
        }

        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(&target) {
                Ok(dest) if dest.is_absolute() => candidate = dest,
                Ok(dest) => candidate = resolved_parent.join(dest),
                Err(_) => return None,
            },
            _ => return None,
        }
    }

    // A chain that long is not a real path; refuse rather than give up.
    Some("a symlink chain too long to follow".to_string())
}

/// Refuse writes and deletes to a sensitive system path or to one of
/// the agent's own configuration files.
///
/// `protected_dir` is the directory holding those files: the agent's
/// own config directory in production, a tempdir under test. `verb` is
/// the word used in the error the model sees ("Writing to", "Deleting").
fn refuse_if_sensitive_in(
    path: &Path,
    workspace_root: Option<&Path>,
    protected_dir: Option<&Path>,
    verb: &str,
) -> Result<()> {
    // System prefixes are matched against both the raw path and the
    // collapsed one: the raw catches the plain case, the collapsed
    // catches a `..` detour. macOS canonicalises `/etc` to
    // `/private/etc`, so neither spelling alone is enough.
    let raw_str = path.to_string_lossy().to_string();
    // Separators normalised to `/`: `SENSITIVE_PREFIXES` are Unix-shaped,
    // and `lexically_normalize` rebuilds the path with the platform's
    // separator, so on Windows the collapsed form would never match one
    // and a `..` detour would slip through the check.
    let collapsed_str = lexically_normalize(path)
        .to_string_lossy()
        .replace('\\', "/");
    if SENSITIVE_PREFIXES.iter().any(|prefix| {
        [&raw_str, &collapsed_str]
            .iter()
            .any(|candidate| candidate.starts_with(prefix) || *candidate == prefix)
    }) {
        // Echo the path the model supplied rather than a canonicalised
        // absolute one, which would put the real home directory into a
        // reply that may reach a chat channel.
        anyhow::bail!("{verb} '{raw_str}' is not allowed (sensitive system path).");
    }

    if let Some(dir) = protected_dir
        && let Some(name) = protected_leaf(path, workspace_root, dir)
    {
        anyhow::bail!(
            "{verb} '{raw_str}' is not allowed: that is this agent's own \
             '{name}', which records what you have been given permission to do."
        );
    }

    Ok(())
}

/// Production entry point: protect the agent's real config directory.
fn refuse_if_sensitive(path: &Path, workspace_root: Option<&Path>, verb: &str) -> Result<()> {
    refuse_if_sensitive_in(path, workspace_root, agent_config_dir().as_deref(), verb)
}

pub struct FileWriteTool {
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl FileWriteTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "file_write".into(),
                description: format!("{REACHES_SENTENCE}Write content to a file, completely replacing its existing content. \
                    Accepts absolute paths, ~/... paths, or workspace-relative paths \
                    (resolved against the workspace root). \
                    Creates the file and any missing parent directories automatically. \
                    When the target file is inside the workspace, the search index \
                    is updated automatically. \
                    Refuses writes to sensitive system paths (/etc, /boot, /bin, etc.) \
                    and to this agent's own config.toml and acp-permissions.json.",
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path — absolute, ~/..., or relative to the workspace root."
                        },
                        "content": {
                            "type": "string",
                            "description": "Complete content to write to the file (overwrites entirely)."
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // The editor's machine when a client is scoped, this one otherwise —
        // see `FileReadTool::execute` and `client_tools`' module doc.
        if let Some(client) = crate::tools::acp_client::current_acp_client() {
            return crate::tools::client_tools::client_write(&client, input).await;
        }

        let path_str = input["path"].as_str().context("missing 'path'")?;
        let content = input["content"].as_str().context("missing 'content'")?;

        let path = expand_path(path_str);
        let workspace_root = self.state.workspace.root.clone();
        refuse_if_sensitive(&path, Some(&workspace_root), "Writing to")?;

        self.state
            .write_file(&path, content)
            .with_context(|| format!("Failed to write '{}'", path.display()))?;

        Ok(format!(
            "Written: {} ({} bytes)",
            path.display(),
            content.len()
        ))
    }
}

// ---------------------------------------------------------------------------
// file_delete
// ---------------------------------------------------------------------------

pub struct FileDeleteTool {
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl FileDeleteTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "file_delete".into(),
                description: format!("{REACHES_SENTENCE}In an ACP session the delete runs a `rm` on the editor's machine over `terminal/*`. Delete a file from the filesystem. \
                    Accepts absolute paths, ~/... paths, or workspace-relative paths \
                    (resolved against the workspace root). \
                    When the file is inside the workspace, it is also removed from the search index \
                    automatically. \
                    Refuses deletes of sensitive system paths (/etc, /boot, /bin, etc.) \
                    and of this agent's own config.toml and acp-permissions.json. \
                    Cannot delete directories.",
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path — absolute, ~/..., or relative to the workspace root."
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for FileDeleteTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Delete
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let path_str = input["path"].as_str().context("missing 'path'")?;
        if let Some(client) = crate::tools::acp_client::current_acp_client() {
            // ACP has no delete, so the client's side is `rm` over the
            // terminal — see `client_tools::client_delete`.
            return crate::tools::client_tools::client_delete(&client, input).await;
        }

        let path = expand_path(path_str);

        let workspace_root = self.state.workspace.root.clone();
        refuse_if_sensitive(&path, Some(&workspace_root), "Deleting")?;

        self.state
            .delete_file(&path)
            .with_context(|| format!("Failed to delete '{}'", path.display()))?;

        Ok(format!("Deleted: {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// file_append
// ---------------------------------------------------------------------------

pub struct FileAppendTool {
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl FileAppendTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "file_append".into(),
                description: format!("{REACHES_SENTENCE}Append content to the end of a file, creating it if missing. \
                    Accepts absolute paths, ~/... paths, or workspace-relative paths \
                    (resolved against the workspace root). \
                    Creates any missing parent directories automatically. \
                    When the target file is inside the workspace, the search index \
                    is updated automatically. \
                    Refuses writes to sensitive system paths (/etc, /boot, /bin, etc.) \
                    and to this agent's own config.toml and acp-permissions.json.",
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path — absolute, ~/..., or relative to the workspace root."
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to append to the end of the file."
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for FileAppendTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // `client_append` is read -> concatenate -> write on the editor's
        // side, since ACP has no append — see its doc for what that costs
        // (two crossings of the whole file, and no atomicity).
        if let Some(client) = crate::tools::acp_client::current_acp_client() {
            return crate::tools::client_tools::client_append(&client, input).await;
        }

        let path_str = input["path"].as_str().context("missing 'path'")?;
        let content = input["content"].as_str().context("missing 'content'")?;

        let path = expand_path(path_str);
        let workspace_root = self.state.workspace.root.clone();
        refuse_if_sensitive(&path, Some(&workspace_root), "Writing to")?;

        self.state
            .append_file(&path, content)
            .with_context(|| format!("Failed to append to '{}'", path.display()))?;

        Ok(format!(
            "Appended: {} (+{} bytes)",
            path.display(),
            content.len()
        ))
    }
}

// ---------------------------------------------------------------------------
// dir_list
// ---------------------------------------------------------------------------

pub struct DirListTool {
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl DirListTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "dir_list".into(),
                description: format!("{REACHES_SENTENCE}In an ACP session the listing runs a `find` on the editor's machine over `terminal/*`. List the direct children of a directory (non-recursive). \
                    Accepts absolute paths, ~/... paths, or workspace-relative paths. \
                    Entries are sorted alphabetically. Directories are shown with a \
                    trailing slash. For deeper exploration, use dir_walk.",
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path — absolute, ~/..., or relative to the workspace root."
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for DirListTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // ACP has no list/stat, so the editor's listing is one `find` over
        // the terminal -- see `client_tools::client_dir_list`. Outside an
        // ACP session this is the agent's own workspace, below.
        if let Some(client) = crate::tools::acp_client::current_acp_client() {
            return crate::tools::client_tools::client_dir_list(&client, input).await;
        }

        let path_str = input["path"].as_str().context("missing 'path'")?;
        let path = expand_path(path_str);

        let entries = self
            .state
            .list_dir(&path)
            .with_context(|| format!("Failed to list '{}'", path.display()))?;

        if entries.is_empty() {
            return Ok(format!("(empty) {}", path.display()));
        }

        let lines: Vec<String> = entries
            .iter()
            .map(|(p, is_dir)| {
                if *is_dir {
                    format!("{}/", p.display())
                } else {
                    p.display().to_string()
                }
            })
            .collect();
        Ok(lines.join("\n"))
    }
}

/// The one sentence a truncated `dir_walk` reports, shared with the client.
///
/// The wording used to sit, verbatim, in both `DirWalkTool::execute` here and
/// `client_tools::client_dir_walk` -- which is exactly the drift the
/// unification exists to avoid: a model that learned "there was more" on one
/// machine must not meet a differently worded cap on the other. The agent side
/// owns the sentence; the client path calls this.
pub(crate) fn truncation_marker(max_entries: usize) -> String {
    format!("[truncated — more than {max_entries} entries; raise max_entries or narrow path]")
}

// ---------------------------------------------------------------------------
// dir_walk
// ---------------------------------------------------------------------------

pub struct DirWalkTool {
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl DirWalkTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "dir_walk".into(),
                description: format!("{REACHES_SENTENCE}In an ACP session the walk runs a `find` on the editor's machine over `terminal/*`. Recursively list all files and directories under a path. \
                    Accepts absolute paths, ~/... paths, or workspace-relative paths. \
                    Output is a sorted flat list; directories carry a trailing slash. \
                    Bounded by max_depth (default 5) and max_entries (default 500) to \
                    avoid runaway walks into large trees.",
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path — absolute, ~/..., or relative to the workspace root."
                        },
                        "max_depth": {
                            "type": "integer",
                            "description": "Maximum recursion depth (default 5, max 20). 0 = only direct children.",
                            "default": 5,
                            "minimum": 0,
                            "maximum": 20
                        },
                        "max_entries": {
                            "type": "integer",
                            "description": "Maximum number of entries to return before truncating (default 500, max 5000).",
                            "default": 500,
                            "minimum": 1,
                            "maximum": 5000
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }
}

fn walk_recurse(
    state: &WorkspaceState,
    workspace_root: &std::path::Path,
    abs_path: &std::path::Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    results: &mut Vec<(PathBuf, bool)>,
) -> Result<bool> {
    let entries = state.list_dir(abs_path)?;
    for (entry_path, is_dir) in entries {
        if results.len() >= max_entries {
            return Ok(true);
        }
        results.push((entry_path.clone(), is_dir));
        if is_dir && depth < max_depth {
            let abs_next = if entry_path.is_absolute() {
                entry_path
            } else {
                workspace_root.join(&entry_path)
            };
            if walk_recurse(
                state,
                workspace_root,
                &abs_next,
                depth + 1,
                max_depth,
                max_entries,
                results,
            )? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[async_trait]
impl Tool for DirWalkTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // Same routing as `dir_list`: `find` on the editor's side, with the
        // same sorting, trailing slashes and truncation marker produced
        // here (see `client_tools::client_dir_walk`).
        if let Some(client) = crate::tools::acp_client::current_acp_client() {
            return crate::tools::client_tools::client_dir_walk(&client, input).await;
        }

        let path_str = input["path"].as_str().context("missing 'path'")?;
        let max_depth = input["max_depth"].as_u64().unwrap_or(5).min(20) as usize;
        let max_entries = input["max_entries"].as_u64().unwrap_or(500).clamp(1, 5000) as usize;

        let path = expand_path(path_str);

        let state = &self.state;
        let workspace_root = state.workspace.root.clone();
        let mut results: Vec<(PathBuf, bool)> = Vec::new();
        let truncated = walk_recurse(
            state,
            &workspace_root,
            &path,
            0,
            max_depth,
            max_entries,
            &mut results,
        )
        .with_context(|| format!("Failed to walk '{}'", path.display()))?;

        if results.is_empty() {
            return Ok(format!("(empty) {}", path.display()));
        }

        let mut out: Vec<String> = results
            .iter()
            .map(|(p, is_dir)| {
                if *is_dir {
                    format!("{}/", p.display())
                } else {
                    p.display().to_string()
                }
            })
            .collect();
        if truncated {
            out.push(truncation_marker(max_entries));
        }
        Ok(out.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// shell
// ---------------------------------------------------------------------------

/// Default shell used when neither the `shell` parameter nor `$SHELL` is set.
#[cfg(not(windows))]
const FALLBACK_SHELL: &str = "/bin/sh";
/// Windows has no `/bin/sh`, and `$SHELL` is not a Windows convention — when
/// it is set at all it is usually an MSYS path like `/usr/bin/bash` that
/// cannot be spawned natively — so the default there is Windows PowerShell,
/// which every Windows install carries.
#[cfg(windows)]
const FALLBACK_SHELL: &str = "powershell";

/// How the tool describes its default shell to the model.
#[cfg(not(windows))]
const DEFAULT_SHELL_DOC: &str = "the shell named by the `$SHELL` environment variable \
    (falling back to `/bin/sh`)";
#[cfg(windows)]
const DEFAULT_SHELL_DOC: &str = "Windows PowerShell (`powershell`)";

/// The shell to run a command with: the `shell` parameter, else `$SHELL`
/// (not on Windows — see [`FALLBACK_SHELL`]), else the fallback.
fn resolve_shell(requested: Option<&str>) -> String {
    let from_env = if cfg!(windows) {
        None
    } else {
        std::env::var("SHELL").ok()
    };
    requested
        .map(str::to_string)
        .or(from_env)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| FALLBACK_SHELL.to_string())
}

/// Hand `command` to `shell` as one command line. POSIX shells take `-c`;
/// PowerShell and `cmd` spell it their own way, and are matched by file
/// stem so `pwsh.exe` or a full path to `cmd.exe` is recognised too.
fn push_command_line(cmd: &mut tokio::process::Command, shell: &str, command: &str) {
    let stem = Path::new(shell)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(shell)
        .to_ascii_lowercase();
    match stem.as_str() {
        "powershell" | "pwsh" => {
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        }
        "cmd" => {
            // `cmd` does not parse its command line the way the C runtime
            // does, so the escaping `Command::arg` applies would reach it
            // as literal backslashes. Pass the line raw instead, the way
            // Node's `shell: true` does: `/s` strips exactly the outer
            // quotes and leaves the rest of the line alone.
            #[cfg(windows)]
            {
                cmd.args(["/d", "/s", "/c"])
                    .raw_arg(format!("\"{command}\""));
            }
            #[cfg(not(windows))]
            {
                cmd.args(["/c", command]);
            }
        }
        _ => {
            cmd.arg("-c").arg(command);
        }
    }
}

pub struct ShellTool {
    workspace_root: PathBuf,
    spec: ToolSpec,
}

impl ShellTool {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            spec: ToolSpec {
                name: "shell".into(),
                description: format!(
                    "{REACHES_SENTENCE}Execute a shell command and return its output. \
                    Returns stdout, stderr, and exit code. \
                    The default working directory is the workspace root. \
                    By default the command runs under {DEFAULT_SHELL_DOC}; \
                    override per call with the `shell` parameter (e.g. `bash`, \
                    `zsh`, `fish`, `pwsh`, `cmd`, or an absolute path). \
                    Use the timeout parameter for long-running commands (default 60 s, max 600 s). \
                    Not suitable for interactive commands or persistent daemons.",
                )
                .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute (run via `<shell> -c`, or the shell's own equivalent for PowerShell and `cmd`)."
                        },
                        "shell": {
                            "type": "string",
                            "description": format!("Shell executable to run the command with — a name resolved via PATH (e.g. `bash`, `zsh`, `fish`, `pwsh`, `cmd`) or an absolute path. Defaults to {DEFAULT_SHELL_DOC}.")
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Max seconds to wait before killing the process (default: 60, max: 600).",
                            "default": 60,
                            "minimum": 1,
                            "maximum": 600
                        },
                        "workdir": {
                            "type": "string",
                            "description": "Working directory (absolute or ~/... path). Defaults to the workspace root."
                        }
                    },
                    "required": ["command"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // The editor's machine when a client is scoped to this turn, this
        // one otherwise. Same one-shot contract either way: wait up to
        // `timeout`, and on timeout hand back a *running* handle rather than
        // killing the command — see `client_tools::format_timed_out`.
        if let Some(client) = crate::tools::acp_client::current_acp_client() {
            let command = input["command"].as_str().context("missing 'command'")?;
            let timeout = crate::tools::client_tools::clamp_timeout(input["timeout"].as_u64());
            // `workdir` names a directory on the *client's* machine, so it is
            // passed through as written: expanding a `~` here would expand
            // this agent's home instead. `None` lets the client default to
            // the session's own cwd.
            let cwd = input["workdir"].as_str();
            // ACP's `terminal/create` takes a command plus an argv, not a
            // shell string, while this tool's `command` parameter *is* one —
            // so the shell is spelled out here, the same way the agent-side
            // body below runs the line under `$SHELL -c`.
            let args = vec!["-c".to_string(), command.to_string()];

            let run =
                crate::tools::client_exec::run_client_command(&client, "sh", &args, cwd, timeout)
                    .await?;
            return match run.timed_out_handle {
                Some(handle) => Ok(crate::tools::client_tools::format_timed_out(
                    &handle, timeout,
                )),
                None => {
                    let status = run.status.expect(
                        "run_client_command always sets `status` when it does not time out",
                    );
                    let mut out = crate::tools::client_tools::format_finished(&run.output, &status);
                    if let Some(warning) = run.release_warning {
                        out.push('\n');
                        out.push_str(&warning);
                    }
                    Ok(out)
                }
            };
        }

        use std::time::Duration;
        use tokio::process::Command;

        let command = input["command"].as_str().context("missing 'command'")?;
        let timeout_secs = input["timeout"].as_u64().unwrap_or(60).min(600);
        let workdir = input["workdir"]
            .as_str()
            .map(expand_path)
            .unwrap_or_else(|| self.workspace_root.clone());

        let shell = resolve_shell(input["shell"].as_str());

        let mut cmd = Command::new(&shell);
        push_command_line(&mut cmd, &shell, command);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(&workdir)
            // The turn running this tool can be cancelled: the ACP endpoint
            // drops `run_llm_turn`'s future on `session/cancel` and on a
            // vanished client, which drops the `wait_with_output` future and
            // with it the `Child`. Without this, that drop merely *disowns*
            // the process — it keeps running, keeps writing to the
            // workspace, and never reaches the timeout branch below, because
            // the timeout is gone too. `shell` is the only tool that can
            // outlive its future this way; the file tools use blocking
            // `std::fs` and so cannot be interrupted part-way.
            //
            // Nothing else changes shape: `/rpc` and the voice/heartbeat
            // paths run their turn in a detached `tokio::spawn`, so it
            // finishes whether or not anyone is still listening, and this
            // never fires for them. `/a2a` awaits its turn inside the axum
            // handler, whose future hyper can drop when the HTTP client
            // vanishes mid-request — there this quietly stops leaking a
            // process it was already leaking.
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn shell '{shell}'"))?;

        let result =
            tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);

                if stderr.is_empty() {
                    Ok(format!("[exit: {exit_code}]\n{stdout}"))
                } else {
                    Ok(format!(
                        "[exit: {exit_code}]\nstdout:\n{stdout}\nstderr:\n{stderr}"
                    ))
                }
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                // Nothing to kill here any more. `timeout` drops the
                // `wait_with_output` future at the end of the statement
                // above, which drops the `Child`, which `kill_on_drop`
                // turns into a SIGKILL (a `TerminateProcess` on Windows) —
                // so by the time this branch runs the shell is already
                // dead. What used to stand here, `kill -9 <pid>`, did
                // exactly that and no more: a positive pid signals one
                // process, not its group, so neither form reaches
                // grandchildren the shell left behind. It was also a
                // blocking `std::process::Command` on an async thread, and
                // relied on a `kill` binary being on PATH, which is not the
                // case on Windows.
                Ok(format!(
                    "[exit: 124]\nCommand timed out after {timeout_secs}s"
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// weather  (Open-Meteo — no API key required)
// ---------------------------------------------------------------------------

pub struct WeatherTool {
    spec: ToolSpec,
}

impl WeatherTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "weather".into(),
                description: "Fetch a weather forecast via the Open-Meteo API \
                    (no API key required). Resolve a place by passing `location` \
                    (e.g. \"Tokyo\", \"渋谷\", \"Paris, FR\") — it is geocoded \
                    into coordinates — or specify `latitude` and `longitude` \
                    directly to skip geocoding. Returns the current conditions \
                    and a daily forecast (min/max temperature, precipitation, \
                    weather code) for the next `days` days (default 3, max 7). \
                    Temperatures are in Celsius; timezone auto-detected from \
                    the resolved coordinates."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "location": {
                            "type": "string",
                            "description": "Place name to geocode (e.g. \"Tokyo\", \"New York\"). Ignored if latitude and longitude are provided."
                        },
                        "latitude": {
                            "type": "number",
                            "description": "Latitude in decimal degrees (-90..90). If set, longitude is required and location is ignored."
                        },
                        "longitude": {
                            "type": "number",
                            "description": "Longitude in decimal degrees (-180..180). If set, latitude is required and location is ignored."
                        },
                        "days": {
                            "type": "integer",
                            "description": "Number of forecast days to return, starting today (default 3, max 7).",
                            "default": 3,
                            "minimum": 1,
                            "maximum": 7
                        }
                    }
                }),
            },
        }
    }
}

impl Default for WeatherTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Translate a WMO weather interpretation code into a short description.
/// Reference: https://open-meteo.com/en/docs (WMO Weather interpretation codes).
fn wmo_code_description(code: i64) -> &'static str {
    match code {
        0 => "clear sky",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 => "fog",
        48 => "depositing rime fog",
        51 => "light drizzle",
        53 => "moderate drizzle",
        55 => "dense drizzle",
        56 => "light freezing drizzle",
        57 => "dense freezing drizzle",
        61 => "slight rain",
        63 => "moderate rain",
        65 => "heavy rain",
        66 => "light freezing rain",
        67 => "heavy freezing rain",
        71 => "slight snow fall",
        73 => "moderate snow fall",
        75 => "heavy snow fall",
        77 => "snow grains",
        80 => "slight rain showers",
        81 => "moderate rain showers",
        82 => "violent rain showers",
        85 => "slight snow showers",
        86 => "heavy snow showers",
        95 => "thunderstorm",
        96 => "thunderstorm with slight hail",
        99 => "thunderstorm with heavy hail",
        _ => "unknown conditions",
    }
}

#[async_trait]
impl Tool for WeatherTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Fetch
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let client = reqwest::Client::new();
        let days = input["days"].as_u64().unwrap_or(3).clamp(1, 7);

        // Resolve coordinates. Explicit lat/lon wins; otherwise geocode `location`.
        let (latitude, longitude, resolved_name) =
            match (input["latitude"].as_f64(), input["longitude"].as_f64()) {
                (Some(lat), Some(lon)) => {
                    if !(-90.0..=90.0).contains(&lat) {
                        anyhow::bail!("latitude {lat} out of range (-90..90)");
                    }
                    if !(-180.0..=180.0).contains(&lon) {
                        anyhow::bail!("longitude {lon} out of range (-180..180)");
                    }
                    (lat, lon, format!("{lat:.4}, {lon:.4}"))
                }
                (Some(_), None) | (None, Some(_)) => {
                    anyhow::bail!("latitude and longitude must be provided together");
                }
                (None, None) => {
                    let location = input["location"]
                        .as_str()
                        .context("provide either 'location' or both 'latitude' and 'longitude'")?;

                    let geo_resp = client
                        .get("https://geocoding-api.open-meteo.com/v1/search")
                        .query(&[
                            ("name", location),
                            ("count", "1"),
                            ("language", "en"),
                            ("format", "json"),
                        ])
                        .send()
                        .await
                        .context("Open-Meteo geocoding request failed")?;

                    if !geo_resp.status().is_success() {
                        let status = geo_resp.status();
                        let body = geo_resp.text().await.unwrap_or_default();
                        anyhow::bail!("Open-Meteo geocoding error {status}: {body}");
                    }

                    let geo: serde_json::Value = geo_resp
                        .json()
                        .await
                        .context("Failed to parse Open-Meteo geocoding response")?;

                    let result = geo["results"]
                        .as_array()
                        .and_then(|arr| arr.first())
                        .with_context(|| format!("No matches for location '{location}'"))?;

                    let lat = result["latitude"]
                        .as_f64()
                        .context("geocoding result missing 'latitude'")?;
                    let lon = result["longitude"]
                        .as_f64()
                        .context("geocoding result missing 'longitude'")?;
                    let name = result["name"].as_str().unwrap_or(location);
                    let admin = result["admin1"].as_str().unwrap_or("");
                    let country = result["country"].as_str().unwrap_or("");
                    let pretty = [name, admin, country]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ");
                    (lat, lon, pretty)
                }
            };

        let lat_s = latitude.to_string();
        let lon_s = longitude.to_string();
        let days_s = days.to_string();

        let forecast_resp = client
            .get("https://api.open-meteo.com/v1/forecast")
            .query(&[
                ("latitude", lat_s.as_str()),
                ("longitude", lon_s.as_str()),
                ("current", "temperature_2m,relative_humidity_2m,apparent_temperature,precipitation,weather_code,wind_speed_10m"),
                (
                    "daily",
                    "weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,precipitation_probability_max",
                ),
                ("timezone", "auto"),
                ("forecast_days", days_s.as_str()),
            ])
            .send()
            .await
            .context("Open-Meteo forecast request failed")?;

        if !forecast_resp.status().is_success() {
            let status = forecast_resp.status();
            let body = forecast_resp.text().await.unwrap_or_default();
            anyhow::bail!("Open-Meteo forecast error {status}: {body}");
        }

        let data: serde_json::Value = forecast_resp
            .json()
            .await
            .context("Failed to parse Open-Meteo forecast response")?;

        let timezone = data["timezone"].as_str().unwrap_or("UTC");

        let mut out = format!("Weather for {resolved_name} ({timezone})\n");

        if let Some(current) = data.get("current") {
            let temp = current["temperature_2m"].as_f64();
            let feels = current["apparent_temperature"].as_f64();
            let humidity = current["relative_humidity_2m"].as_f64();
            let precip = current["precipitation"].as_f64();
            let wind = current["wind_speed_10m"].as_f64();
            let code = current["weather_code"].as_i64().unwrap_or(-1);
            let time = current["time"].as_str().unwrap_or("");
            out.push_str(&format!(
                "\nCurrent ({time}): {}\n",
                wmo_code_description(code)
            ));
            if let Some(t) = temp {
                out.push_str(&format!("  temp: {t:.1}°C"));
                if let Some(f) = feels {
                    out.push_str(&format!(" (feels {f:.1}°C)"));
                }
                out.push('\n');
            }
            if let Some(h) = humidity {
                out.push_str(&format!("  humidity: {h:.0}%\n"));
            }
            if let Some(p) = precip {
                out.push_str(&format!("  precipitation: {p:.1} mm\n"));
            }
            if let Some(w) = wind {
                out.push_str(&format!("  wind: {w:.1} km/h\n"));
            }
        }

        if let Some(daily) = data.get("daily") {
            let dates = daily["time"].as_array();
            let codes = daily["weather_code"].as_array();
            let tmax = daily["temperature_2m_max"].as_array();
            let tmin = daily["temperature_2m_min"].as_array();
            let psum = daily["precipitation_sum"].as_array();
            let pprob = daily["precipitation_probability_max"].as_array();
            if let Some(dates) = dates {
                out.push_str("\nForecast:\n");
                for (i, date) in dates.iter().enumerate() {
                    let date = date.as_str().unwrap_or("");
                    let code = codes
                        .and_then(|a| a.get(i))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(-1);
                    let hi = tmax.and_then(|a| a.get(i)).and_then(|v| v.as_f64());
                    let lo = tmin.and_then(|a| a.get(i)).and_then(|v| v.as_f64());
                    let pp = psum.and_then(|a| a.get(i)).and_then(|v| v.as_f64());
                    let pr = pprob.and_then(|a| a.get(i)).and_then(|v| v.as_f64());
                    out.push_str(&format!("  {date}: {}", wmo_code_description(code)));
                    if let (Some(hi), Some(lo)) = (hi, lo) {
                        out.push_str(&format!(", {lo:.1}°C / {hi:.1}°C"));
                    }
                    if let Some(pp) = pp {
                        out.push_str(&format!(", precip {pp:.1} mm"));
                    }
                    if let Some(pr) = pr {
                        out.push_str(&format!(" ({pr:.0}%)"));
                    }
                    out.push('\n');
                }
            }
        }

        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// web_search  (Tavily)
// ---------------------------------------------------------------------------

pub struct WebSearchTool {
    api_key: String,
    spec: ToolSpec,
}

impl WebSearchTool {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            spec: ToolSpec {
                name: "web_search".into(),
                description: "Search the web for up-to-date information using Tavily. \
                    Returns titles, URLs, and short content excerpts for the top results."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Number of results to return (default: 5, max: 10).",
                            "default": 5,
                            "maximum": 10
                        }
                    },
                    "required": ["query"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Fetch
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let query = input["query"].as_str().context("missing 'query'")?;
        let limit = input["limit"].as_u64().unwrap_or(5).min(10) as usize;

        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.tavily.com/search")
            .json(&json!({
                "api_key": self.api_key,
                "query": query,
                "max_results": limit,
            }))
            .send()
            .await
            .context("Tavily API request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Tavily API error {status}: {body}");
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Tavily response")?;
        let results = data["results"]
            .as_array()
            .context("Unexpected Tavily response format (missing 'results')")?;

        if results.is_empty() {
            return Ok("No results found.".to_string());
        }

        let lines: Vec<String> = results
            .iter()
            .map(|r| {
                let title = r["title"].as_str().unwrap_or("(no title)");
                let url = r["url"].as_str().unwrap_or("");
                let content = r["content"].as_str().unwrap_or("");
                let snippet = if content.len() > 300 {
                    &content[..content.floor_char_boundary(300)]
                } else {
                    content
                };
                format!("**{title}**\n{url}\n{snippet}")
            })
            .collect();

        Ok(lines.join("\n\n"))
    }
}

// ---------------------------------------------------------------------------
// mcp_reconnect — tear down and re-establish an MCP server connection
// ---------------------------------------------------------------------------

pub struct McpReconnectTool {
    spec: ToolSpec,
    tool_set: Weak<ToolSet>,
}

impl McpReconnectTool {
    pub fn new(tool_set: Weak<ToolSet>) -> Self {
        Self {
            spec: ToolSpec {
                name: "mcp_reconnect".into(),
                description:
                    "Reconnect to a configured MCP server (stdio or HTTP) and refresh its tool list. \
                     Use this when an MCP server has crashed, disconnected, or is being restarted \
                     during testing — tools registered under `mcp__<server>__*` become usable again \
                     without restarting the agent."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Name of the MCP server to reconnect (as configured in tools.mcp_servers)."
                        }
                    },
                    "required": ["server"]
                }),
            },
            tool_set,
        }
    }
}

#[async_trait]
impl Tool for McpReconnectTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let server = input
            .get("server")
            .and_then(|v| v.as_str())
            .context("Missing required field: server")?;

        let tool_set = self
            .tool_set
            .upgrade()
            .context("ToolSet has been dropped; cannot reconnect")?;

        let known = tool_set.mcp_server_names();
        if !known.iter().any(|n| n == server) {
            anyhow::bail!(
                "unknown MCP server '{server}'. Configured servers: [{}]",
                known.join(", ")
            );
        }

        tool_set.reconnect_mcp_server(server).await
    }
}

// ---------------------------------------------------------------------------
// recall_image
// ---------------------------------------------------------------------------

/// Re-fetch a past image from the workspace-external image cache and
/// attach it to the tool_result so the model can look at it again.
///
/// Older turns in conversation history appear to the model as text
/// markers like `[image: image/png sha256=<hex>]` — the raw bytes
/// aren't re-sent every turn to keep input-token cost down. When the
/// user asks the model to look at a past image, the model is expected
/// to call this tool with the marker's sha256 + media_type; the cache
/// is then queried and the actual bytes get appended to the user
/// message carrying this tool's result.
pub struct RecallImageTool {
    cache: Arc<ImageCache>,
    spec: ToolSpec,
}

impl RecallImageTool {
    pub fn new(cache: Arc<ImageCache>) -> Self {
        let spec = ToolSpec {
            name: "recall_image".into(),
            description: "Re-fetch a past image from the conversation by its sha256 hash. \
                Use this when the user references an image that now appears in history as a \
                `[image: <media_type> sha256=<hex>]` text marker (only images attached in the \
                CURRENT turn are visible inline; older ones are shown as markers to save \
                tokens). Pass the marker's `sha256` and `media_type` verbatim; the actual \
                image bytes will be returned as an attachment for you to view."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sha256": {
                        "type": "string",
                        "description": "Hex sha256 of the image to recall (copy from the `[image: ... sha256=<hex>]` marker)."
                    },
                    "media_type": {
                        "type": "string",
                        "description": "MIME type of the image, e.g. `image/png`, `image/jpeg`, `image/gif`, `image/webp` (copy from the marker)."
                    }
                },
                "required": ["sha256", "media_type"]
            }),
        };
        Self { cache, spec }
    }
}

#[async_trait]
impl Tool for RecallImageTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
        // Should never be reached: `execute_full` is overridden below.
        // If something does call this path, surface a clear marker text
        // rather than a panic — the model can recover.
        Ok("recall_image returned no image (text-only path).".to_string())
    }

    async fn execute_full(&self, input: &serde_json::Value) -> Result<ToolOutput> {
        let sha256 = input
            .get("sha256")
            .and_then(|v| v.as_str())
            .context("Missing required field: sha256")?;
        let media_type = input
            .get("media_type")
            .and_then(|v| v.as_str())
            .context("Missing required field: media_type")?;

        if !is_hex_sha256(sha256) {
            anyhow::bail!(
                "sha256 must be a 64-char lowercase hex string; got {} char(s)",
                sha256.len()
            );
        }

        let bytes = self
            .cache
            .get(sha256)
            .with_context(|| format!("image not in cache (sha256={sha256})"))?;

        let data_base64 = BASE64_STANDARD.encode(&bytes);
        let byte_len = bytes.len();
        Ok(ToolOutput {
            text: format!(
                "Recalled image (sha256={sha256}, media_type={media_type}, {byte_len} bytes). \
                 Image attached to this tool result — refer to it directly."
            ),
            images: vec![(media_type.to_string(), data_base64)],
        })
    }
}

fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[cfg(test)]
mod sensitive_path_tests {
    use super::*;
    use tempfile::TempDir;

    /// A stand-in for the agent's config directory.
    ///
    /// `present` decides whether the protected files already exist.
    /// **Both regimes matter, and the absent one matters more**: on a
    /// fresh install nothing has recorded a permission answer yet, so
    /// `acp-permissions.json` is not there — and an earlier version of
    /// this guard leaned on file identity, which can only answer when
    /// the target exists. Its tests created the files every time and so
    /// never exercised the regime production actually starts in. That
    /// is how an NTFS alternate-data-stream bypass shipped.
    fn protected_dir(present: bool) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        if present {
            std::fs::write(path.join("acp-permissions.json"), b"{}").unwrap();
            std::fs::write(path.join("config.toml"), b"").unwrap();
        }
        (dir, path)
    }

    fn refused(path: &str, workspace_root: Option<&Path>, protected: &Path) -> bool {
        refuse_if_sensitive_in(
            Path::new(path),
            workspace_root,
            Some(protected),
            "Writing to",
        )
        .is_err()
    }

    #[test]
    fn system_paths_are_refused() {
        for path in [
            "/etc/passwd",
            "/bin/sh",
            "/proc/self/mem",
            "/var/run/docker.sock",
        ] {
            assert!(
                refuse_if_sensitive_in(Path::new(path), None, None, "Writing to").is_err(),
                "{path} should be refused"
            );
            // A `..` detour to the same place.
            let detour = format!("/tmp/../{}", path.trim_start_matches('/'));
            assert!(
                refuse_if_sensitive_in(Path::new(&detour), None, None, "Writing to").is_err(),
                "{detour} should be refused"
            );
        }
    }

    /// Every spelling that reaches a protected file must be refused —
    /// run twice, once with the files present and once absent, because
    /// the guard must not depend on the target existing.
    #[test]
    fn no_spelling_reaches_a_protected_file() {
        for present in [true, false] {
            let (_guard, dir) = protected_dir(present);
            std::fs::create_dir_all(dir.join("sub")).unwrap();

            let mut spellings: Vec<String> = vec![
                "acp-permissions.json".into(),
                "config.toml".into(),
                // A `..` through a directory that does NOT exist: the
                // writer's create_dir_all makes it and the kernel
                // collapses the `..` at open time.
                "nope/../acp-permissions.json".into(),
                "a/b/c/../../../acp-permissions.json".into(),
                // ...and through one that does.
                "sub/../acp-permissions.json".into(),
                "./acp-permissions.json".into(),
            ];
            // Absolute spellings of the same.
            spellings.push(
                dir.join("acp-permissions.json")
                    .to_string_lossy()
                    .into_owned(),
            );
            spellings.push(
                dir.join("nope/../acp-permissions.json")
                    .to_string_lossy()
                    .into_owned(),
            );

            if cfg!(windows) {
                // `name::$DATA` is the file's own default data stream.
                // A colon is never legal in an NTFS filename, so this
                // is stream syntax and the OS opens the plain file.
                spellings.push("acp-permissions.json::$DATA".into());
                spellings.push("config.toml::$DATA".into());
                // Trailing dots and spaces are stripped when Windows
                // normalises a path.
                spellings.push("acp-permissions.json.".into());
                spellings.push("acp-permissions.json  ".into());
                spellings.push("acp-permissions.json. . ".into());
                spellings.push("ACP-PERMISSIONS.JSON".into());
            }

            for spelling in &spellings {
                assert!(
                    refused(spelling, Some(&dir), &dir),
                    "'{spelling}' must not reach a protected file (present={present})"
                );
            }

            // A climb out of a subdirectory workspace.
            assert!(
                refused("../acp-permissions.json", Some(&dir.join("sub")), &dir),
                "a climb out of the workspace must be refused (present={present})"
            );
        }
    }

    /// A symlink is a different path that opens the same file, and
    /// `fs::write` follows it. The guard follows it too — with
    /// `read_link` rather than `canonicalize`, so a *dangling* link
    /// pointing at a record that does not exist yet still resolves.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_protected_file_is_refused() {
        for present in [true, false] {
            let (_guard, dir) = protected_dir(present);
            let workspace = TempDir::new().unwrap();

            let link = workspace.path().join("notes.json");
            std::os::unix::fs::symlink(dir.join("acp-permissions.json"), &link).unwrap();
            assert!(
                refuse_if_sensitive_in(&link, None, Some(&dir), "Writing to").is_err(),
                "a symlink to the record must be refused (present={present})"
            );

            // A relative link, resolved against its own directory.
            let rel = dir.join("innocent.json");
            std::os::unix::fs::symlink("acp-permissions.json", &rel).unwrap();
            assert!(
                refuse_if_sensitive_in(&rel, None, Some(&dir), "Writing to").is_err(),
                "a relative symlink must be refused (present={present})"
            );
        }
    }

    /// A hard link is the same file under another name, and no
    /// comparison of spellings can see that — only identity can.
    ///
    /// Deliberately NOT `#[cfg(unix)]`. NTFS hard links need no
    /// privilege, and gating this to Unix is how an earlier version
    /// shipped with hard-link protection removed and a red suite on two
    /// platforms, while the Windows host it was verified on reported
    /// everything green because the test never compiled.
    #[test]
    fn a_hard_link_to_a_protected_file_is_refused() {
        let (_guard, dir) = protected_dir(true);
        let workspace = TempDir::new().unwrap();

        for (link, label) in [
            (dir.join("innocent.json"), "beside the record"),
            (workspace.path().join("notes.json"), "in another directory"),
        ] {
            if std::fs::hard_link(dir.join("acp-permissions.json"), &link).is_err() {
                // Cross-volume, or a filesystem without hard links.
                continue;
            }
            assert!(
                refuse_if_sensitive_in(&link, None, Some(&dir), "Writing to").is_err(),
                "a hard link {label} must be refused"
            );
            assert!(
                refuse_if_sensitive_in(&link, None, Some(&dir), "Deleting").is_err(),
                "deleting through a hard link {label} must be refused"
            );
        }
    }

    /// `C:x` means two different things, and the guard has to be right
    /// about both.
    ///
    /// On Windows it is *drive-relative*: it resolves against that
    /// drive's own current directory, which neither this guard nor the
    /// workspace root controls, and `PathBuf::join` silently discards
    /// the root when a path carries a prefix. Enough `..` and it climbs
    /// to the config directory with the guard looking elsewhere. So it
    /// is refused as a spelling — nothing legitimate needs it.
    ///
    /// Everywhere else `C:` is an ordinary run of filename characters,
    /// there is no prefix component, and the path is an unremarkable
    /// relative filename that must keep working. Asserting the refusal
    /// on both platforms is how this first went red on Linux.
    #[test]
    fn a_drive_relative_path_is_refused_on_windows_and_ordinary_elsewhere() {
        for present in [true, false] {
            let (_guard, dir) = protected_dir(present);
            for spelling in [
                r"C:acp-permissions.json",
                r"C:..\..\..\..\acp-permissions.json",
                r"Z:..\config.toml",
            ] {
                let refused_here = refused(spelling, Some(&dir), &dir);
                if cfg!(windows) {
                    assert!(
                        refused_here,
                        "'{spelling}' is drive-relative here and must be refused \
                         (present={present})"
                    );
                } else {
                    assert!(
                        !refused_here,
                        "'{spelling}' is an ordinary filename here and must be allowed \
                         (present={present})"
                    );
                }
            }
        }
    }

    /// A protected directory that does not exist yet must not read as
    /// "nothing to protect": the write would create it, record and all.
    #[test]
    fn a_missing_protected_directory_still_protects() {
        let outer = TempDir::new().unwrap();
        let dir = outer.path().join("not-created-yet");

        for name in PROTECTED_CONFIG_FILES {
            assert!(
                refuse_if_sensitive_in(&dir.join(name), None, Some(&dir), "Writing to").is_err(),
                "'{name}' must be refused even before the directory exists"
            );
        }
        assert!(
            refuse_if_sensitive_in(&dir.join("notes.md"), None, Some(&dir), "Writing to").is_ok(),
            "an ordinary file there is still fine"
        );
    }

    /// The guard must not swallow ordinary paths. In particular a
    /// workspace that IS the protected directory — which is what an
    /// unset `workspace_dir` gives you — stays writable for everything
    /// except the named files.
    #[test]
    fn ordinary_paths_are_allowed() {
        for present in [true, false] {
            let (_guard, dir) = protected_dir(present);

            for path in ["/tmp/notes.md", "/home/someone/todo.txt"] {
                assert!(!refused(path, None, &dir), "{path} should be allowed");
            }
            for name in [
                "notes.md",
                "memory/thing.md",
                "heartbeat/daily.md",
                "acp-permissions.json.backup",
                "not-config.toml",
            ] {
                assert!(
                    !refused(name, Some(&dir), &dir),
                    "{name} must stay writable (present={present})"
                );
            }
        }
    }

    /// The same filename in a lookalike directory is a different file.
    #[test]
    fn a_sibling_directory_is_not_the_protected_one() {
        let (_guard, dir) = protected_dir(true);
        let sibling = TempDir::new().unwrap();
        std::fs::write(sibling.path().join("acp-permissions.json"), b"{}").unwrap();

        assert!(
            !refused(
                &sibling
                    .path()
                    .join("acp-permissions.json")
                    .to_string_lossy(),
                None,
                &dir
            ),
            "a different directory's file of the same name should be allowed"
        );
    }

    /// Deleting is guarded on the same terms as writing. Nothing else
    /// exercises the other verb.
    #[test]
    fn the_delete_verb_is_guarded_too() {
        let (_guard, dir) = protected_dir(true);
        let err = refuse_if_sensitive_in(
            &dir.join("acp-permissions.json"),
            None,
            Some(&dir),
            "Deleting",
        )
        .expect_err("deleting the record must be refused");
        assert!(err.to_string().starts_with("Deleting"), "got {err}");
    }

    /// The production wrapper must actually consult the agent's own
    /// config directory. Nothing else covers that wiring, and a guard
    /// pointed at the wrong directory would pass every other test here.
    #[test]
    fn the_production_wrapper_protects_the_real_config_dir() {
        let Some(config_dir) = agent_config_dir() else {
            panic!("no config dir: PermissionStore falls back to the cwd, which nothing guards");
        };
        assert!(
            refuse_if_sensitive(&config_dir.join("acp-permissions.json"), None, "Writing to")
                .is_err(),
            "the real permission record must be refused"
        );
        assert!(
            refuse_if_sensitive(&config_dir.join("notes.md"), None, "Writing to").is_ok(),
            "an ordinary file in the config dir must stay writable"
        );
    }

    #[test]
    fn lexical_normalisation_collapses_without_touching_the_disk() {
        assert_eq!(
            lexically_normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        // Never pop past the root, and never leave a `..` behind that a
        // later comparison would have to reason about.
        assert_eq!(
            lexically_normalize(Path::new("/../..")),
            PathBuf::from(std::path::MAIN_SEPARATOR_STR)
        );
        // A leading `..` on a relative path has nothing to cancel and
        // is kept, so it can still be joined onto a workspace root.
        assert_eq!(
            lexically_normalize(Path::new("../x")),
            PathBuf::from("../x")
        );
        assert_eq!(lexically_normalize(Path::new("")), PathBuf::from(""));
        assert_eq!(lexically_normalize(Path::new("x")), PathBuf::from("x"));
    }

    #[test]
    fn the_opened_filename_is_what_windows_will_open() {
        let name = |s: &str| opened_filename(std::ffi::OsStr::new(s));

        assert_eq!(
            name("acp-permissions.json").as_deref(),
            Some("acp-permissions.json")
        );
        if cfg!(windows) {
            assert_eq!(name("x.json::$DATA").as_deref(), Some("x.json"));
            assert_eq!(name("x.json:stream").as_deref(), Some("x.json"));
            assert_eq!(name("x.json. . ").as_deref(), Some("x.json"));
            // Nothing left is not a spelling of any real file.
            assert_eq!(name("::$DATA"), None);
            assert_eq!(name(". "), None);
        } else {
            // Elsewhere these are ordinary filename characters.
            assert_eq!(name("x.json::$DATA").as_deref(), Some("x.json::$DATA"));
            assert_eq!(name("x.json.").as_deref(), Some("x.json."));
        }
    }
}

#[cfg(test)]
mod recall_image_tests {
    use super::*;
    use tempfile::TempDir;

    fn open_cache() -> (TempDir, Arc<ImageCache>) {
        let tmp = TempDir::new().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf()).unwrap();
        (tmp, cache)
    }

    #[tokio::test]
    async fn returns_image_on_cache_hit() {
        let (_tmp, cache) = open_cache();
        let bytes = b"\xff\xd8\xfffake-jpeg".to_vec();
        let sha = crate::image_cache::sha256_hex(&bytes);
        cache.put(&sha, &bytes).unwrap();

        let tool = RecallImageTool::new(cache);
        let out = tool
            .execute_full(&json!({"sha256": sha, "media_type": "image/jpeg"}))
            .await
            .unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].0, "image/jpeg");
        assert_eq!(BASE64_STANDARD.decode(&out.images[0].1).unwrap(), bytes);
        assert!(out.text.contains(&sha));
    }

    #[tokio::test]
    async fn errors_on_cache_miss() {
        let (_tmp, cache) = open_cache();
        let tool = RecallImageTool::new(cache);
        let sha = "0".repeat(64);
        let err = tool
            .execute_full(&json!({"sha256": sha, "media_type": "image/png"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not in cache"));
    }

    #[tokio::test]
    async fn rejects_malformed_sha() {
        let (_tmp, cache) = open_cache();
        let tool = RecallImageTool::new(cache);
        let err = tool
            .execute_full(&json!({"sha256": "abc", "media_type": "image/png"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("64-char"));
    }
}

// ---------------------------------------------------------------------------
// refresh_system_prompt
// ---------------------------------------------------------------------------

/// Tool the agent calls to explicitly re-read workspace files into the
/// system prompt. System-prompt file reads are pinned (see
/// `Workspace::read_file`); this drops the pinned cache AND every channel
/// snapshot so the next turn rebuilds from the current files — one of the
/// two moments the prompt legitimately changes (the other is the
/// day-boundary snapshot rebuild). Delegates to `Agent::invalidate_system_prompts`.
pub struct RefreshSystemPromptTool {
    agent: std::sync::Weak<crate::agent::Agent>,
    spec: ToolSpec,
}

impl RefreshSystemPromptTool {
    pub fn new(agent: std::sync::Weak<crate::agent::Agent>) -> Self {
        Self {
            agent,
            spec: ToolSpec {
                name: "refresh_system_prompt".to_string().into(),
                description: "システムプロンプトの読み込みキャッシュをクリアし、\
ワークスペースファイルを再読み込みさせます。メモリ等を編集して即時反映させたい時に呼んでください。"
                    .to_string()
                    .into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
        }
    }
}

#[async_trait]
impl Tool for RefreshSystemPromptTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
        let agent = self
            .agent
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("agent unavailable"))?;
        agent.invalidate_system_prompts().await;
        Ok("システムプロンプトの読み込みキャッシュをクリアしました".to_string())
    }
}

#[cfg(test)]
mod refresh_system_prompt_tests {
    use super::*;
    use crate::channel::Channels;
    use crate::provider::registry::ProviderRegistry;
    use crate::session::SessionStore;
    use crate::workspace::Workspace;

    /// A real `Agent`, built the way `agent.rs`'s `build_test_agent`
    /// builds one. The tool only ever holds the `Weak` reference, but
    /// constructing a real `Agent` keeps the wiring under test identical
    /// to the one `main.rs` wires at runtime: `Weak<Agent>` pointing at
    /// a live agent, registered into a `ToolSet` via `register_tool`.
    /// The `TempDir` is leaked on purpose — same test-binary reasoning
    /// as every other fixture in this crate.
    fn agent_weak() -> (Arc<crate::agent::Agent>, Weak<crate::agent::Agent>) {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let agent = Arc::new(crate::agent::Agent::new(
            crate::config::Config::parse_for_test("[anthropic]\napi_key = \"test\"\n"),
            Arc::new(Channels::new(Vec::new(), std::collections::HashMap::new())),
            Arc::new(ProviderRegistry::for_test(
                &["anthropic", "stub"],
                Arc::new(crate::serve::StubProvider::new(Vec::new())),
            )),
            Arc::new(Workspace::new(
                dir.path().to_path_buf(),
                crate::config::DigestConfig::default(),
            )),
            None,
            Arc::new(SessionStore::new(dir.path().join("sessions"), "test", None)),
            None,
        ));
        let weak = Arc::downgrade(&agent);
        (agent, weak)
    }

    /// (1) a registered `refresh_system_prompt` is visible through
    /// `ToolSet::specs_filtered`, and (2) `execute` on a live agent
    /// returns the success text. The tool is also the case that
    /// deliberately relies on the default `Other` kind, like
    /// `mcp_reconnect` — asserted here because this tool is registered
    /// outside `default_tool_set`, so `every_tool_declares_its_kind`
    /// never sees it.
    #[tokio::test]
    async fn registered_tool_is_visible_and_execute_returns_success_text() {
        let (agent, weak) = agent_weak();
        let set = ToolSet::new(
            vec![Box::new(RefreshSystemPromptTool::new(weak)) as Box<dyn Tool>],
            Vec::new(),
        );

        let names: Vec<String> = set
            .specs_filtered(|_| true)
            .await
            .into_iter()
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(names, vec!["refresh_system_prompt".to_string()]);

        let tool = RefreshSystemPromptTool::new(Arc::downgrade(&agent));
        assert_eq!(tool.kind(), ToolKind::Other);
        assert_eq!(tool.spec().name, "refresh_system_prompt");
        assert!(
            tool.spec().input_schema["properties"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            tool.execute(&json!({})).await.unwrap(),
            "システムプロンプトの読み込みキャッシュをクリアしました"
        );
    }

    /// The tool body is a thin wrapper: upgrade the `Weak<Agent>` and
    /// call `Agent::invalidate_system_prompts` — nothing else. With the
    /// agent gone there is nothing else to do either, and the upgrade
    /// failure surfaces as this error text.
    #[tokio::test]
    async fn execute_errors_when_the_agent_is_gone() {
        let tool = RefreshSystemPromptTool::new(Weak::new());
        let err = tool.execute(&json!({})).await.unwrap_err();
        assert_eq!(format!("{err:#}"), "agent unavailable");
    }
}

#[cfg(test)]
mod session_routing_tests {
    use super::*;
    use crate::tools::acp_client::{AcpClient, scope_acp_client, tests::FakeClient};
    use sapphire_framework::workspace::{AppContext, Workspace};

    static TEST_CTX: AppContext = AppContext::new("sapphire-agent").allow_external_paths();

    /// A fresh workspace in a temp dir. Same shape as `tools::tests`'
    /// helper of the same name, duplicated rather than shared because the
    /// intervening `mod tests` is private and this module is not its
    /// descendant.
    fn test_workspace() -> Arc<WorkspaceState> {
        // `AppContext` panics on any `cache_dir()` access until
        // `set_cache_dir` has been called once, and `Workspace::from_root`
        // reads it to compute the workspace's cache path. First writer
        // wins and later calls are ignored, so calling it here every time
        // is safe.
        TEST_CTX
            .set_cache_dir(std::env::temp_dir().join("sapphire-agent-builtin-tools-test-cache"));
        // Leaked on purpose: this is a test binary and the OS reclaims the
        // directory when it exits.
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        std::fs::create_dir_all(dir.path().join(".sapphire-agent")).unwrap();
        let ws = Workspace::from_root(&TEST_CTX, dir.path()).unwrap();
        Arc::new(WorkspaceState::open(ws).unwrap())
    }

    fn file_read_tool_for_test() -> (Arc<WorkspaceState>, FileReadTool) {
        let state = test_workspace();
        (Arc::clone(&state), FileReadTool::new(state))
    }

    fn file_write_tool_for_test() -> (Arc<WorkspaceState>, FileWriteTool) {
        let state = test_workspace();
        (Arc::clone(&state), FileWriteTool::new(state))
    }

    fn file_append_tool_for_test() -> (Arc<WorkspaceState>, FileAppendTool) {
        let state = test_workspace();
        (Arc::clone(&state), FileAppendTool::new(state))
    }

    fn file_delete_tool_for_test() -> (Arc<WorkspaceState>, FileDeleteTool) {
        let state = test_workspace();
        (Arc::clone(&state), FileDeleteTool::new(state))
    }

    fn dir_list_tool_for_test() -> (Arc<WorkspaceState>, DirListTool) {
        let state = test_workspace();
        (Arc::clone(&state), DirListTool::new(state))
    }

    fn dir_walk_tool_for_test() -> (Arc<WorkspaceState>, DirWalkTool) {
        let state = test_workspace();
        (Arc::clone(&state), DirWalkTool::new(state))
    }

    /// `ShellTool` takes the workspace root rather than the state: its own
    /// default working directory *is* that root.
    fn shell_tool_for_test() -> (Arc<WorkspaceState>, ShellTool) {
        let state = test_workspace();
        let root = state.workspace.root.clone();
        (Arc::clone(&state), ShellTool::new(root))
    }

    /// Put `content` in the *agent's* copy of `name` — the machine the
    /// routing tests below must not touch.
    fn write_workspace_file(state: &Arc<WorkspaceState>, name: &str, content: &str) {
        state.write_file(Path::new(name), content).unwrap();
    }

    fn read_workspace_file(state: &Arc<WorkspaceState>, name: &str) -> Option<String> {
        state.read_file(Path::new(name)).ok()
    }

    /// The point of the unification: with an ACP client scoped, `file_read`
    /// reads the *editor's* machine. The workspace and the fake client are
    /// given deliberately different contents so the assertion cannot pass by
    /// accident.
    #[tokio::test]
    async fn file_read_reads_the_clients_machine_inside_an_acp_session() {
        let (state, tool) = file_read_tool_for_test();
        // The agent's own file, which must NOT be what comes back.
        write_workspace_file(&state, "note.txt", "agent side\n");

        let client = Arc::new(FakeClient::default());
        client.queue_read_result("client side\n");
        let out = scope_acp_client(
            Arc::clone(&client) as Arc<dyn AcpClient>,
            tool.execute(&json!({"path": "note.txt"})),
        )
        .await
        .unwrap();

        assert_eq!(out, "client side\n");
        assert!(
            client.read_paths().contains(&"note.txt".to_string()),
            "the read must have gone to the client, got {:?}",
            client.read_paths()
        );
    }

    /// Without a client scoped, the same tool reads the agent's own machine —
    /// unchanged behaviour for every non-ACP transport.
    #[tokio::test]
    async fn file_read_reads_the_agents_machine_outside_an_acp_session() {
        let (state, tool) = file_read_tool_for_test();
        write_workspace_file(&state, "note.txt", "agent side\n");
        let out = tool.execute(&json!({"path": "note.txt"})).await.unwrap();
        assert!(out.contains("agent side"), "got: {out}");
    }

    /// The write direction of the same routing: an ACP turn's `file_write`
    /// lands on the editor's machine and the agent's own copy of that path is
    /// left exactly as it was.
    #[tokio::test]
    async fn file_write_writes_the_clients_machine_inside_an_acp_session() {
        let (state, tool) = file_write_tool_for_test();
        write_workspace_file(&state, "note.txt", "agent side\n");

        let client = Arc::new(FakeClient::default());
        scope_acp_client(
            Arc::clone(&client) as Arc<dyn AcpClient>,
            tool.execute(&json!({"path": "note.txt", "content": "client side\n"})),
        )
        .await
        .unwrap();

        assert_eq!(
            client.writes.lock().unwrap().as_slice(),
            &[("note.txt".to_string(), "client side\n".to_string())]
        );
        assert_eq!(
            read_workspace_file(&state, "note.txt").as_deref(),
            Some("agent side\n"),
            "the agent's own file must be untouched: the write went to the client"
        );
    }

    #[tokio::test]
    async fn file_write_writes_the_agents_machine_outside_an_acp_session() {
        let (state, tool) = file_write_tool_for_test();
        tool.execute(&json!({"path": "note.txt", "content": "agent side\n"}))
            .await
            .unwrap();
        assert_eq!(
            read_workspace_file(&state, "note.txt").as_deref(),
            Some("agent side\n")
        );
    }

    /// `file_append` is not an ACP request at all: the client's file is read
    /// whole, the addition concatenated, and the result written back. The
    /// agent's own copy is the "before" in that pair and must stay it.
    #[tokio::test]
    async fn file_append_concatenates_on_the_clients_machine_inside_an_acp_session() {
        let (state, tool) = file_append_tool_for_test();
        write_workspace_file(&state, "note.txt", "agent side\n");

        let client = Arc::new(FakeClient::default());
        client.queue_read_result("client side\n");
        let out = scope_acp_client(
            Arc::clone(&client) as Arc<dyn AcpClient>,
            tool.execute(&json!({"path": "note.txt", "content": "more\n"})),
        )
        .await
        .unwrap();

        assert_eq!(out, "Appended: note.txt (+5 bytes)");
        assert_eq!(
            client.writes.lock().unwrap().as_slice(),
            &[("note.txt".to_string(), "client side\nmore\n".to_string())],
            "the client's existing content must be the prefix, not the agent's"
        );
        assert_eq!(
            read_workspace_file(&state, "note.txt").as_deref(),
            Some("agent side\n"),
            "the agent's own file must be untouched"
        );
    }

    /// `file_delete` on the client is `rm` over the terminal, since ACP has
    /// no delete. The argv assertion is what pins the shape of that call —
    /// the path travels as a positional argument, never interpolated into
    /// the script — and the agent's own file is what proves the call went to
    /// the other machine.
    #[tokio::test]
    async fn file_delete_deletes_on_the_clients_machine_inside_an_acp_session() {
        let (state, tool) = file_delete_tool_for_test();
        write_workspace_file(&state, "note.txt", "agent side\n");

        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout("");
        let out = scope_acp_client(
            Arc::clone(&client) as Arc<dyn AcpClient>,
            tool.execute(&json!({"path": "note.txt"})),
        )
        .await
        .unwrap();

        assert_eq!(out, "Deleted: note.txt");
        assert_eq!(
            read_workspace_file(&state, "note.txt").as_deref(),
            Some("agent side\n"),
            "the agent's own file must be untouched: the delete ran on the client"
        );
        // Positional-argument placement is the whole contract here, and
        // `$0` is the trap: `bash -c <script> <argv0> [args...]` puts the
        // first word *after* the script in `$0`, so a path passed as
        // `argv0` would land where no script reads it and every delete
        // would fail. Assert every slot, not just "somewhere in the argv".
        let creates = client.creates.lock().unwrap();
        assert_eq!(creates[0].0, "bash");
        let argv = &creates[0].1;
        assert_eq!(argv[0], "-c");
        assert_eq!(
            argv[1],
            crate::tools::client_tools::delete_script(),
            "the script is the `-c` operand"
        );
        assert_eq!(
            argv[2], "client_delete",
            "argv0 is a placeholder; the path must not sit in `$0`, which no script reads"
        );
        assert_eq!(
            argv[3], "note.txt",
            "the path is `$1` — the first real argument — not interpolated into the script"
        );
        assert_eq!(argv.len(), 4, "no extra arguments: {argv:?}");
    }

    /// ACP's `terminal/create` takes a command plus an argv, not a shell
    /// string, while this tool's own `command` parameter *is* a shell string.
    /// So the ACP path has to spell `sh -c` out itself, and `workdir` maps
    /// onto the client call's `cwd`.
    #[tokio::test]
    async fn shell_runs_the_command_line_through_sh_c_on_the_client() {
        let (_state, tool) = shell_tool_for_test();
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout("hi\n");

        let out = scope_acp_client(
            Arc::clone(&client) as Arc<dyn AcpClient>,
            tool.execute(&json!({"command": "echo hi", "workdir": "/z"})),
        )
        .await
        .unwrap();

        assert!(out.contains("hi"), "got: {out}");
        let creates = client.creates.lock().unwrap();
        assert_eq!(creates[0].0, "sh");
        assert_eq!(creates[0].1, vec!["-c".to_string(), "echo hi".to_string()]);
        assert_eq!(
            creates[0].2.as_deref(),
            Some("/z"),
            "`workdir` is the client call's `cwd`"
        );
    }

    /// Outside an ACP session the same tool still runs on this machine — the
    /// agent-side body, untouched.
    #[tokio::test]
    async fn shell_runs_on_the_agents_machine_outside_an_acp_session() {
        let (_state, tool) = shell_tool_for_test();
        let out = tool
            // Quoted, so it reads as one argument to PowerShell's `echo`
            // (the Windows default) as well as to `sh`'s.
            .execute(&json!({"command": "echo \"agent side\""}))
            .await
            .unwrap();
        assert!(out.contains("agent side"), "got: {out}");
    }

    /// `dir_list` on an ACP turn runs on the editor's machine. The path is
    /// outside this workspace on purpose (`/z`): the agent-side body would
    /// refuse it, so a listing can only have come from the client.
    ///
    /// The queued stdout is what the script actually emits, in the script's own
    /// order (agent-side pre-order DFS, `E` announcement first), because the
    /// client passes that order through: a fixture in some other order would
    /// only be testing the fixture.
    #[tokio::test]
    async fn dir_list_lists_the_clients_machine_inside_an_acp_session() {
        let (_state, tool) = dir_list_tool_for_test();
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout("E\t/z\nF\t/z/a.txt\nF\t/z/b.txt\nD\t/z/sub\n");

        let out = scope_acp_client(
            Arc::clone(&client) as Arc<dyn AcpClient>,
            tool.execute(&json!({"path": "/z"})),
        )
        .await
        .unwrap();

        assert_eq!(out, "/z/a.txt\n/z/b.txt\n/z/sub/");
        let creates = client.creates.lock().unwrap();
        assert_eq!(creates.len(), 1, "exactly one terminal, on the client");
        assert_eq!(
            creates[0].0, "bash",
            "the listing is a command on the client"
        );
    }

    /// The same routing for `dir_walk`, whose two bounds are positional
    /// arguments: `max_depth + 1` for `find` (it counts the starting point
    /// as depth 0) and `max_entries + 1` so truncation is decidable without
    /// a second round trip.
    #[tokio::test]
    async fn dir_walk_walks_the_clients_machine_inside_an_acp_session() {
        let (_state, tool) = dir_walk_tool_for_test();
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout("F\t/z/f0\nF\t/z/f1\nF\t/z/f2\n");

        let out = scope_acp_client(
            Arc::clone(&client) as Arc<dyn AcpClient>,
            tool.execute(&json!({"path": "/z", "max_depth": 2, "max_entries": 2})),
        )
        .await
        .unwrap();

        assert_eq!(
            out,
            "/z/f0\n/z/f1\n[truncated \u{2014} more than 2 entries; \
             raise max_entries or narrow path]"
        );
        let creates = client.creates.lock().unwrap();
        let argv = &creates[0].1;
        assert_eq!(argv[3], "3", "max_depth + 1, got: {argv:?}");
        assert_eq!(argv[4], "3", "max_entries + 1, got: {argv:?}");
    }

    /// Outside an ACP session both tools are the agent's own bodies, untouched:
    /// sorted, directories with a trailing slash.
    ///
    /// The order is a **pre-order DFS**, which is what the client's script has
    /// to reproduce: `sub` comes before the sibling file `sub.txt`, and
    /// everything under `sub` is emitted right after `sub`. A flat sort of the
    /// whole listing would put `sub.txt` first, because `/` sorts above `.`.
    #[tokio::test]
    async fn dir_list_still_lists_the_agents_machine_outside_an_acp_session() {
        let (state, tool) = dir_list_tool_for_test();
        let root = state.workspace.root.clone();
        write_workspace_file(&state, "a.txt", "a\n");
        write_workspace_file(&state, "b.txt", "b\n");
        write_workspace_file(&state, "sub/inner.txt", "inner\n");
        write_workspace_file(&state, "sub.txt", "sub\n");

        let out = tool
            .execute(&json!({"path": root.to_str().unwrap()}))
            .await
            .unwrap();

        // Sliced rather than compared whole: the workspace root carries the
        // framework's own `.sapphire-agent` directory, which sorts before
        // `a.txt` and is not what this test is about.
        let lines: Vec<&str> = out.lines().collect();
        let a = lines.iter().position(|l| *l == "a.txt").expect("a.txt");
        assert_eq!(
            &lines[a..a + 4],
            ["a.txt", "b.txt", "sub/", "sub.txt"],
            "one level, sorted, directories with a slash: {out}"
        );
    }

    /// The truncation sentence is the same one `client_tools` adds, so a model
    /// that learned it on this machine reads the client's answer the same way
    /// -- neither machine's walk reports "there was more" in its own words.
    /// Read from the shared function, so a reworded marker is reworded on both
    /// sides or on neither.
    #[tokio::test]
    async fn dir_walk_truncates_on_the_agents_machine_with_the_same_marker() {
        let (state, tool) = dir_walk_tool_for_test();
        let dir = state.workspace.root.join("d");
        std::fs::create_dir_all(&dir).unwrap();
        write_workspace_file(&state, "d/a.txt", "a\n");
        write_workspace_file(&state, "d/b.txt", "b\n");

        let out = tool
            .execute(&json!({"path": dir.to_str().unwrap(), "max_entries": 1}))
            .await
            .unwrap();

        assert!(out.contains("a.txt"), "got: {out}");
        assert!(out.contains(&truncation_marker(1)), "got: {out}");
    }

    /// A shell to run the client's own scripts with, or `None` where neither is
    /// on `PATH` -- the same guard `skills`' script tests use. The scripts are
    /// shell running on someone else's machine, so a Rust-side assertion that
    /// never evaluates them would prove nothing about the order the two
    /// machines produce.
    ///
    /// `None` on Windows as well: there `bash` on `PATH` is usually WSL's
    /// `System32\bash.exe`, which runs (and fails) without a distribution, and
    /// even Git's `sh` would see the agent's `C:\...` paths rather than the
    /// POSIX ones these scripts are written for. The scripts target a POSIX
    /// client, so a POSIX host is where they can be run against the agent.
    fn shell_for_scripts() -> Option<&'static str> {
        if !cfg!(unix) {
            return None;
        }
        ["bash", "sh"].into_iter().find(|c| {
            std::process::Command::new(c)
                .arg("-c")
                .arg("exit 0")
                .status()
                .is_ok()
        })
    }

    /// Run one of the client's scripts the way `run_client_bash` does --
    /// `<shell> -c <script> <argv0> <args...>` -- against a real directory.
    fn run_client_script(
        shell: &str,
        script: &str,
        argv0: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> std::process::Output {
        let mut cmd = std::process::Command::new(shell);
        cmd.arg("-c").arg(script).arg(argv0).args(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().expect("the shell runs")
    }

    /// The review's finding, pinned against the real thing: the client's `find`
    /// script must produce the **agent's own pre-order DFS** -- `sub`, the
    /// entries under `sub`, then the sibling file `sub.txt` -- and not a flat
    /// sort of the whole listing, which a byte comparison puts the other way
    /// round (`/` is 0x2F, above `.`).
    ///
    /// Both machines are run against one real tree, so this is the comparison
    /// the two fake-driven tests can only approximate: the agent's own tools on
    /// this side, and the client's script through a real shell on the other.
    #[tokio::test]
    async fn the_client_scripts_keep_the_agents_own_dfs_order() {
        let Some(shell) = shell_for_scripts() else {
            return;
        };
        let (state, walk) = dir_walk_tool_for_test();
        let list = DirListTool::new(Arc::clone(&state));
        let root = state.workspace.root.clone();
        write_workspace_file(&state, "a.txt", "a\n");
        write_workspace_file(&state, "b.txt", "b\n");
        write_workspace_file(&state, "sub/inner.txt", "inner\n");
        write_workspace_file(&state, "sub.txt", "sub\n");
        let root = root.to_str().unwrap().to_string();

        // The agent's own tools, on this machine.
        let agent_list = list.execute(&json!({"path": root})).await.unwrap();
        let agent_walk = walk.execute(&json!({"path": root})).await.unwrap();

        // The client's scripts, through a real shell, over the same tree.
        let client_list = run_client_script(
            shell,
            &crate::tools::client_tools::list_script(),
            "client_dir_list",
            &[&root],
            &[],
        );
        let client_walk = run_client_script(
            shell,
            &crate::tools::client_tools::walk_script(),
            "client_dir_walk",
            &[&root, "6", "1001"],
            &[],
        );
        assert!(client_list.status.success(), "list script: {client_list:?}");
        assert!(client_walk.status.success(), "walk script: {client_walk:?}");

        // The agent's listing is relative to the workspace root (both tools
        // treat it as internal), the script's is absolute: strip the shared
        // prefix so the two are compared as one list of names, in one order.
        let prefix = format!("{}/", root.trim_end_matches('/'));
        let shape = |out: &std::process::Output| {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let (entries, _) = crate::tools::client_tools::shape_entries(&stdout, &root, 1000);
            entries
                .into_iter()
                .map(|entry| {
                    entry
                        .strip_prefix(&prefix)
                        .expect("every entry is under the root")
                        .to_string()
                })
                .collect::<Vec<String>>()
        };

        assert_eq!(shape(&client_list), agent_list.lines().collect::<Vec<_>>());
        assert_eq!(shape(&client_walk), agent_walk.lines().collect::<Vec<_>>());

        // The fixture's own sanity: the pair this task is about, in the order
        // the agent emits it -- `sub` and its contents before the sibling file
        // `sub.txt`.
        let lines: Vec<&str> = agent_walk.lines().collect();
        let sub = lines.iter().position(|l| *l == "sub/").expect("sub/");
        assert_eq!(
            &lines[sub..sub + 3],
            ["sub/", "sub/inner.txt", "sub.txt"],
            "the agent's DFS, which the client's script has to match: {agent_walk}"
        );
    }

    /// The review's finding 2, pinned against the real thing: `find` prints
    /// nothing on stdout when it cannot read the directory, so a script that
    /// only reported what came back would let `(empty) <path>` stand in for a
    /// path that is not there. The script refuses instead, before `find` runs,
    /// and `run_client_bash` turns a non-zero exit into an error.
    #[test]
    fn the_client_script_refuses_a_missing_directory() {
        let Some(shell) = shell_for_scripts() else {
            return;
        };
        let missing = "/nonexistent-sapphire-agent-test-dir";
        assert!(!std::path::Path::new(missing).exists());

        for script in [
            crate::tools::client_tools::list_script(),
            crate::tools::client_tools::walk_script(),
        ] {
            let out =
                run_client_script(shell, &script, "client_dir_list", &[missing, "3", "3"], &[]);
            assert!(
                !out.status.success(),
                "a missing directory must not exit 0: {out:?}"
            );
            assert!(
                out.stdout.is_empty(),
                "nothing may reach stdout that could be shaped into a listing: {out:?}"
            );
        }
    }

    /// The review's other half of finding 2: `~/...` is quoted on its way into
    /// `find`, and quoting is exactly what stops a tilde from expanding -- so
    /// the path that every one of these tools advertises failed on the client.
    /// The scripts now expand it against the **client's** `$HOME`, which is the
    /// only `$HOME` that can answer for the client's machine; `file_delete`
    /// carries the same resolution, since its `rm` was reachable the same way.
    #[test]
    fn the_client_scripts_resolve_a_tilde_against_the_clients_home() {
        let Some(shell) = shell_for_scripts() else {
            return;
        };
        let home =
            std::env::temp_dir().join(format!("sapphire-agent-tilde-{}", std::process::id()));
        std::fs::create_dir_all(home.join("x")).unwrap();
        std::fs::write(home.join("x/f.txt"), "f\n").unwrap();
        let home = home.to_str().unwrap().to_string();
        let env = [("HOME", home.as_str())];

        let list = run_client_script(
            shell,
            &crate::tools::client_tools::list_script(),
            "client_dir_list",
            &["~/x"],
            &env,
        );
        assert!(list.status.success(), "{list:?}");
        let stdout = String::from_utf8_lossy(&list.stdout).into_owned();
        assert!(
            stdout.contains(&format!("E\t{home}/x\n")),
            "the announcement names the client's resolved path: {stdout}"
        );
        assert!(
            stdout.contains(&format!("F\t{home}/x/f.txt\n")),
            "`~` resolved against the client's own $HOME: {stdout}"
        );

        let delete = run_client_script(
            shell,
            &crate::tools::client_tools::delete_script(),
            "client_delete",
            &["~/x/f.txt"],
            &env,
        );
        assert!(delete.status.success(), "{delete:?}");
        assert!(
            !std::path::Path::new(&home).join("x/f.txt").exists(),
            "the tilde path must have deleted the client's file, not failed on it"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
