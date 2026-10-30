//! `sapphire-agent.avatar.*` over IPC: the avatar's files, served from the
//! workspace's `avatar/` folder by [`AvatarStore`]. The method names, wire
//! shapes and client live in `sapphire-agent-core`.

use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use sapphire_agent_core::{
    AVATAR_MANIFEST, AVATAR_READ, AVATAR_REMOVE, AVATAR_SET_MANIFEST, AVATAR_WRITE_BEGIN,
    AVATAR_WRITE_CHUNK, AVATAR_WRITE_COMMIT, AvatarConfig,
};
use sapphire_framework::ipc::{RequestCtx, Router, RpcError, codes};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::avatar_store::{AvatarError, AvatarStore};

type Cell = Arc<OnceLock<Arc<AvatarStore>>>;
type Reply = std::result::Result<Value, RpcError>;

fn rpc_error(code: i32, message: String) -> RpcError {
    RpcError {
        code,
        message,
        data: None,
    }
}

fn store_error(e: AvatarError) -> RpcError {
    match e {
        AvatarError::Invalid(m) => rpc_error(codes::INVALID_PARAMS, m),
        AvatarError::Io(e) => rpc_error(codes::INTERNAL_ERROR, format!("avatar storage: {e}")),
    }
}

/// Decode `ctx`'s params, run `f` on the store off the async threads (it
/// hashes and copies whole files), and map its error to an RPC error.
async fn run<P, T>(
    cell: Cell,
    ctx: RequestCtx,
    method: &'static str,
    f: impl FnOnce(&AvatarStore, P) -> Result<T, AvatarError> + Send + 'static,
) -> Reply
where
    P: for<'de> Deserialize<'de> + Send + 'static,
    T: Into<Value> + Send + 'static,
{
    let store = cell.get().cloned().ok_or_else(|| {
        rpc_error(
            codes::INTERNAL_ERROR,
            "the agent is still starting; try again once `sapphire-agent status` shows it \
             serving"
                .to_owned(),
        )
    })?;
    let params: P = serde_json::from_value(ctx.params).map_err(|e| {
        rpc_error(
            codes::INVALID_PARAMS,
            format!("bad {method} parameters: {e}"),
        )
    })?;
    tokio::task::spawn_blocking(move || f(&store, params))
        .await
        .map_err(|e| rpc_error(codes::INTERNAL_ERROR, format!("{method}: {e}")))?
        .map(Into::into)
        .map_err(store_error)
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
    #[serde(default)]
    offset: u64,
    len: usize,
}

#[derive(Deserialize)]
struct BeginParams {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct ChunkParams {
    upload_id: String,
    offset: u64,
    data: String,
}

#[derive(Deserialize)]
struct CommitParams {
    upload_id: String,
}

#[derive(Deserialize)]
struct RemoveParams {
    path: String,
}

/// Register the seven `avatar.*` methods on `router`; they answer from
/// `cell` once the agent has filled it.
pub fn register(router: Router, cell: Cell) -> Router {
    let (c1, c2, c3, c4, c5, c6, c7) = (
        cell.clone(),
        cell.clone(),
        cell.clone(),
        cell.clone(),
        cell.clone(),
        cell.clone(),
        cell,
    );
    router
        .method(AVATAR_MANIFEST, move |ctx| {
            let cell = c1.clone();
            async move {
                run(cell, ctx, AVATAR_MANIFEST, |s, _: Value| {
                    s.manifest()
                        .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
                })
                .await
            }
        })
        .method(AVATAR_READ, move |ctx| {
            let cell = c2.clone();
            async move {
                run(cell, ctx, AVATAR_READ, |s, p: ReadParams| {
                    let (bytes, eof) = s.read(&p.path, p.offset, p.len)?;
                    Ok(json!({
                        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                        "eof": eof,
                    }))
                })
                .await
            }
        })
        .method(AVATAR_WRITE_BEGIN, move |ctx| {
            let cell = c3.clone();
            async move {
                run(cell, ctx, AVATAR_WRITE_BEGIN, |s, p: BeginParams| {
                    let id = s.write_begin(&p.path, p.size, &p.sha256)?;
                    Ok(json!({ "upload_id": id }))
                })
                .await
            }
        })
        .method(AVATAR_WRITE_CHUNK, move |ctx| {
            let cell = c4.clone();
            async move {
                run(cell, ctx, AVATAR_WRITE_CHUNK, |s, p: ChunkParams| {
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(&p.data)
                        .map_err(|e| AvatarError::Invalid(format!("data is not base64: {e}")))?;
                    s.write_chunk(&p.upload_id, p.offset, &data)?;
                    Ok(json!({}))
                })
                .await
            }
        })
        .method(AVATAR_WRITE_COMMIT, move |ctx| {
            let cell = c5.clone();
            async move {
                run(cell, ctx, AVATAR_WRITE_COMMIT, |s, p: CommitParams| {
                    s.write_commit(&p.upload_id)?;
                    Ok(json!({}))
                })
                .await
            }
        })
        .method(AVATAR_SET_MANIFEST, move |ctx| {
            let cell = c6.clone();
            async move {
                run(cell, ctx, AVATAR_SET_MANIFEST, |s, p: AvatarConfig| {
                    s.set_config(&p)?;
                    Ok(json!({}))
                })
                .await
            }
        })
        .method(AVATAR_REMOVE, move |ctx| {
            let cell = c7.clone();
            async move {
                run(cell, ctx, AVATAR_REMOVE, |s, p: RemoveParams| {
                    s.remove(&p.path)?;
                    Ok(json!({}))
                })
                .await
            }
        })
}

#[cfg(test)]
mod tests {
    use sapphire_agent_core::{AVATAR_CHUNK, AvatarConfig, ChatClient, VrmConfig};

    use super::*;
    use crate::serve::chat_ipc::ChatTunnel;

    struct Fx {
        _t: tempfile::TempDir,
        chat: ChatClient,
    }

    async fn fx() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let tunnel = ChatTunnel::default();
        tunnel.attach_avatar_for_test(AvatarStore::new(
            t.path().join("avatar"),
            t.path().join("staging"),
        ));
        let chat = crate::serve::chat_ipc::tests::connected(&tunnel).await;
        Fx { _t: t, chat }
    }

    #[tokio::test]
    async fn upload_configure_and_download_round_trip() {
        let Fx { chat, .. } = fx().await;
        let model = vec![7u8; AVATAR_CHUNK + 10];
        chat.avatar_upload("model.vrm", &model).await.unwrap();
        chat.avatar_set_config(&AvatarConfig {
            vrm: Some(VrmConfig {
                model: "model.vrm".into(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .unwrap();
        let manifest = chat.avatar_manifest().await.unwrap();
        assert_eq!(manifest.config.vrm.unwrap().model, "model.vrm");
        assert_eq!(manifest.files[0].size, model.len() as u64);
        assert_eq!(chat.avatar_download("model.vrm").await.unwrap(), model);
    }

    #[tokio::test]
    async fn an_escaping_path_is_an_invalid_params_error() {
        let Fx { chat, .. } = fx().await;
        let err = chat.avatar_download("../config.toml").await.unwrap_err();
        assert!(err.to_string().contains("invalid") || err.to_string().contains(".."));
    }

    #[tokio::test]
    async fn remove_deletes_an_unused_file_and_refuses_one_in_use() {
        let Fx { chat, .. } = fx().await;
        chat.avatar_upload("a.vrm", b"abc").await.unwrap();
        chat.avatar_set_config(&AvatarConfig {
            vrm: Some(VrmConfig {
                model: "a.vrm".into(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(chat.avatar_remove("a.vrm").await.is_err());
        chat.avatar_set_config(&AvatarConfig::default())
            .await
            .unwrap();
        chat.avatar_remove("a.vrm").await.unwrap();
        assert!(chat.avatar_manifest().await.unwrap().files.is_empty());
    }

    #[tokio::test]
    async fn before_the_agent_is_attached_the_methods_say_it_is_starting() {
        let tunnel = ChatTunnel::default();
        let chat = crate::serve::chat_ipc::tests::connected(&tunnel).await;
        let err = chat.avatar_manifest().await.unwrap_err();
        assert!(err.to_string().contains("still starting"), "{err}");
    }
}
