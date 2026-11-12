use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use sapphire_framework::workspace::WorkspaceState;
use serde_json::json;

use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind, ToolOutput};

pub struct ImageReadTool {
    state: Arc<WorkspaceState>,
    spec: ToolSpec,
}

impl ImageReadTool {
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            spec: ToolSpec {
                name: "image_read".into(),
                description: "Read one JPEG, PNG, GIF or WebP image from this agent's machine \
                    and attach it to the result. Input is limited to 32 MiB; images over \
                    5 MiB are shrunk. Requires host_access; not available over ACP."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string",
                        "description": "Image path on this agent's machine: absolute, ~/..., or workspace-relative." } },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
        }
    }
}

// Controller ruling 2026-11-12: framework 0.28.0 keeps resolve_path and
// canonicalize_or_parent private. Reconstruct file_read's path policy from its
// public workspace root and context, without changing the pinned dependency.
fn resolve_image_path(state: &WorkspaceState, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        state.workspace.root.join(path)
    };
    let mut ancestor = joined.as_path();
    let mut suffix = Vec::new();
    let resolved = loop {
        if let Ok(canonical) = ancestor.canonicalize() {
            break suffix.iter().rev().fold(canonical, |p, part| p.join(part));
        }
        match (ancestor.parent(), ancestor.file_name()) {
            (Some(parent), Some(name)) => {
                suffix.push(name.to_owned());
                ancestor = parent;
            }
            _ => break joined.clone(),
        }
    };
    anyhow::ensure!(
        state.workspace.ctx.allows_external_paths() || resolved.starts_with(&state.workspace.root),
        "Path '{}' escapes the workspace",
        path.display()
    );
    Ok(resolved)
}

#[async_trait]
impl Tool for ImageReadTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        Ok(self.execute_full(input).await?.text)
    }

    async fn execute_full(&self, input: &serde_json::Value) -> Result<ToolOutput> {
        anyhow::ensure!(
            crate::tools::acp_client::current_acp_client().is_none(),
            "image_read is unavailable in ACP sessions; no host fallback is permitted"
        );
        let path = crate::tools::builtin_tools::expand_path(
            input["path"].as_str().context("missing 'path'")?,
        );
        let state = Arc::clone(&self.state);
        let (path, original_len, prepared) = tokio::task::spawn_blocking(move || -> Result<_> {
            let path = resolve_image_path(&state, &path)?;
            anyhow::ensure!(!path.starts_with("/dev") && !path.starts_with("/proc"),
                "Reading device or proc paths is not allowed");
            let metadata = std::fs::metadata(&path)
                .with_context(|| format!("Failed to inspect '{}'", path.display()))?;
            anyhow::ensure!(metadata.is_file(), "image path must be a regular file");
            anyhow::ensure!(metadata.len() <= crate::channel::MAX_DOWNLOAD_BYTES as u64,
                "image exceeds the 32 MiB input limit");
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            // If the file is replaced by a FIFO after metadata, don't block
            // opening it before the handle's metadata can be rechecked.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let file = options.open(&path)
                .with_context(|| format!("Failed to open '{}'", path.display()))?;
            let metadata = file.metadata()?;
            anyhow::ensure!(metadata.is_file(), "image path must be a regular file");
            anyhow::ensure!(metadata.len() <= crate::channel::MAX_DOWNLOAD_BYTES as u64,
                "image exceeds the 32 MiB input limit");
            let mut bytes = Vec::new();
            file.take(crate::channel::MAX_DOWNLOAD_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= crate::channel::MAX_DOWNLOAD_BYTES,
                "image exceeds the 32 MiB input limit");
            let original_len = bytes.len();
            let prepared = crate::channel::prepare_image(bytes)
                .map_err(anyhow::Error::msg)
                .context("Only JPEG, PNG, GIF and WebP are supported; SVG can be read as text with file_read")?;
            Ok((path, original_len, prepared))
        }).await.context("image read task failed")??;
        let shrunk = if original_len > crate::channel::MAX_ATTACHMENT_BYTES {
            "shrunk"
        } else {
            "not shrunk"
        };
        Ok(ToolOutput {
            text: format!(
                "Read image {} ({original_len} original bytes, {}, {shrunk}). Image attached to this tool result.",
                crate::workspace::slash_path(&path),
                prepared.media_type
            ),
            images: vec![(prepared.media_type, BASE64_STANDARD.encode(&prepared.data))],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::acp_client::{scope_acp_client, tests::FakeClient};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sapphire_framework::workspace::{AppContext, Workspace};
    use serde_json::json;
    use std::io::Cursor;

    static RESTRICTED: AppContext = AppContext::new("sapphire-agent");
    static EXTERNAL: AppContext = AppContext::new("sapphire-agent").allow_external_paths();

    fn test_workspace(external: bool) -> Arc<WorkspaceState> {
        let ctx = if external { &EXTERNAL } else { &RESTRICTED };
        ctx.set_cache_dir(std::env::temp_dir().join("sapphire-image-read-test-cache"));
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".sapphire-agent")).unwrap();
        Arc::new(WorkspaceState::open(Workspace::from_root(ctx, &root).unwrap()).unwrap())
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut seed = 0x12345678_u32;
        let img = image::RgbImage::from_fn(width, height, |_, _| {
            let mut channel = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            };
            image::Rgb([channel(), channel(), channel()])
        });
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[tokio::test]
    async fn image_read_sniffs_not_extension() {
        let state = test_workspace(false);
        let bytes = png(1, 1);
        std::fs::write(state.workspace.root.join("image.jpg"), &bytes).unwrap();
        let tool = ImageReadTool::new(state);
        let out = tool
            .execute_full(&json!({"path": "image.jpg"}))
            .await
            .unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].0, "image/png");
        assert_eq!(STANDARD.decode(&out.images[0].1).unwrap(), bytes);
        assert!(
            out.text.contains("image.jpg")
                && out.text.contains("image/png")
                && out.text.contains(&bytes.len().to_string())
        );
        assert!(out.text.contains("not shrunk"));
        assert_eq!(
            tool.execute(&json!({"path": "image.jpg"})).await.unwrap(),
            out.text
        );
        assert_eq!(
            tool.spec().input_schema,
            json!({"type":"object", "properties":{"path":{"type":"string", "description":"Image path on this agent's machine: absolute, ~/..., or workspace-relative."}}, "required":["path"], "additionalProperties":false})
        );
    }

    #[tokio::test]
    async fn image_read_shrinks_without_mutating_original() {
        let state = test_workspace(false);
        let bytes = png(2000, 1200);
        assert!(
            bytes.len() > crate::channel::MAX_ATTACHMENT_BYTES
                && bytes.len() <= crate::channel::MAX_DOWNLOAD_BYTES,
            "{}",
            bytes.len()
        );
        let path = state.workspace.root.join("large.png");
        std::fs::write(&path, &bytes).unwrap();
        let out = ImageReadTool::new(state)
            .execute_full(&json!({"path": "large.png"}))
            .await
            .unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].0, "image/jpeg");
        let result = STANDARD.decode(&out.images[0].1).unwrap();
        assert!(result.len() <= crate::channel::MAX_ATTACHMENT_BYTES);
        assert!(image::load_from_memory(&result).is_ok());
        assert!(out.text.contains("shrunk") && out.text.contains(&bytes.len().to_string()));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn image_read_rejects_oversize_and_nonimage() {
        let state = test_workspace(false);
        let path = state.workspace.root.join("huge");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(crate::channel::MAX_DOWNLOAD_BYTES as u64 + 1)
            .unwrap();
        std::fs::write(state.workspace.root.join("a.svg"), b"<svg/>").unwrap();
        let tool = ImageReadTool::new(state);
        let err = tool
            .execute_full(&json!({"path":"huge"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("32 MiB"), "{err:#}");
        let err = tool
            .execute_full(&json!({"path":"a.svg"}))
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("JPEG") && text.contains("SVG") && text.contains("file_read"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn image_read_rejects_directory() {
        let state = test_workspace(false);
        let err = ImageReadTool::new(state)
            .execute_full(&json!({"path":"."}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("regular file"), "{err:#}");
    }

    #[tokio::test]
    async fn image_read_refuses_acp_without_host_fallback() {
        let state = test_workspace(false);
        std::fs::write(state.workspace.root.join("host.png"), png(1, 1)).unwrap();
        let fake = Arc::new(FakeClient::default());
        let tool = ImageReadTool::new(state);
        let err = scope_acp_client(fake.clone(), tool.execute_full(&json!({"path":"host.png"})))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("ACP"), "{err:#}");
        assert!(fake.reads.lock().unwrap().is_empty());
        assert!(fake.creates.lock().unwrap().is_empty());
    }

    // 2026-11-12 controller ruling: reproduce file_read's resolution using
    // the framework 0.28.0 public surface; resolve_path itself is private.
    #[tokio::test]
    async fn image_read_respects_file_read_path_policy() {
        let state = test_workspace(false);
        std::fs::write(state.workspace.root.join("relative.png"), png(1, 1)).unwrap();
        let external = tempfile::tempdir().unwrap();
        let outside = external.path().join("outside.png");
        std::fs::write(&outside, png(1, 1)).unwrap();
        let tool = ImageReadTool::new(state.clone());
        assert!(
            tool.execute_full(&json!({"path":"relative.png"}))
                .await
                .is_ok()
        );
        assert!(tool.execute_full(&json!({"path": outside})).await.is_err());
        let text = external.path().join("outside.txt");
        std::fs::write(&text, "outside").unwrap();
        assert!(
            crate::tools::builtin_tools::FileReadTool::new(state)
                .execute(&json!({"path":text}))
                .await
                .is_err()
        );
        assert!(
            ImageReadTool::new(test_workspace(true))
                .execute_full(&json!({"path":outside}))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn image_read_expands_tilde_like_file_read() {
        let home = std::env::var_os("HOME").expect("HOME for tilde expansion");
        let dir = tempfile::Builder::new()
            .prefix("sapphire-image-test-")
            .tempdir_in(&home)
            .unwrap();
        std::fs::write(dir.path().join("image.png"), png(1, 1)).unwrap();
        std::fs::write(dir.path().join("text.txt"), "hello").unwrap();
        let rel = dir.path().strip_prefix(&home).unwrap().to_str().unwrap();
        let state = test_workspace(true);
        let tool = ImageReadTool::new(state.clone());
        assert!(
            tool.execute_full(&json!({"path":format!("~/{rel}/image.png")}))
                .await
                .is_ok()
        );
        assert!(
            crate::tools::builtin_tools::FileReadTool::new(state)
                .execute(&json!({"path":format!("~/{rel}/text.txt")}))
                .await
                .is_ok()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn image_read_rejects_fifo_and_resolved_device_paths() {
        use std::os::unix::fs::symlink;
        let state = test_workspace(true);
        let fifo = state.workspace.root.join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        symlink("/dev/null", state.workspace.root.join("device")).unwrap();
        let tool = ImageReadTool::new(state);
        for path in ["fifo", "device", "/proc/self/status"] {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                tool.execute_full(&json!({"path":path})),
            )
            .await
            .expect("must not block");
            assert!(result.is_err(), "{path}");
        }
    }
}
