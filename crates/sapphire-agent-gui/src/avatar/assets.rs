//! Fetching the avatar's files from the agent and keeping them in a local
//! cache, named by their sha256 and their extension (`<sha256>.<ext>`).
//!
//! Pure of bevy except for the [`AvatarAssets`] `Resource` derive and the
//! [`AssetPath`] [`LoadedAvatar::asset_path`] hands out. The files come
//! from an [`AvatarSource`] so the agent is not the only place an avatar
//! can come from.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use bevy::asset::AssetPath;
use bevy::asset::io::AssetSourceId;
use bevy::prelude::Resource;
use sapphire_agent_core::{AvatarConfig, AvatarFile, AvatarManifest, ChatClient};
use sha2::{Digest as _, Sha256};
use tokio::runtime::Handle;

/// Where an avatar and its files come from. The agent is the source today;
/// this is the one place another (a bundled avatar) plugs in.
#[async_trait::async_trait]
pub trait AvatarSource: Send + Sync {
    /// The avatar's config and the files it has.
    async fn manifest(&self) -> Result<AvatarManifest>;
    /// One file, by its avatar-relative path.
    async fn fetch(&self, path: &str) -> Result<Vec<u8>>;
}

#[async_trait::async_trait]
impl AvatarSource for ChatClient {
    async fn manifest(&self) -> Result<AvatarManifest> {
        self.avatar_manifest().await
    }
    async fn fetch(&self, path: &str) -> Result<Vec<u8>> {
        self.avatar_download(path).await
    }
}

/// Lowercase hex sha256, as the agent's manifest writes it.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `s` is a hash this cache would have written: it names a file
/// inside the cache directory and nothing else.
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The bevy asset source the cache is served under:
/// `companion-avatar://<cache name>`. The host registers it with
/// [`super::register_asset_source`].
pub const ASSET_SOURCE: &str = "companion-avatar";

/// The cache's name for the file at the avatar-relative `path` whose bytes
/// hash to `sha256`: the hash, then the path's extension in lowercase,
/// which bevy picks a loader by (`model.VRM` → `<sha256>.vrm`). An
/// extension other than ASCII letters and digits is left off. `None` when
/// `sha256` is not one, so a name never leaves the cache directory.
pub fn cache_name(sha256: &str, path: &str) -> Option<String> {
    if !is_sha256_hex(sha256) {
        return None;
    }
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_ascii_lowercase);
    Some(match extension {
        Some(extension) => format!("{sha256}.{extension}"),
        None => sha256.to_owned(),
    })
}

/// A directory of files each named by the sha256 of its bytes and the
/// extension of the avatar path they came from ([`cache_name`]).
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `<app cache>/companion-avatar`, or `None` where the platform has no
    /// cache directory.
    pub fn default_dir() -> Option<PathBuf> {
        Some(
            corundum_framework_platform::dirs::AppDirs::new(sapphire_agent_core::APP)?
                .cache()
                .join("companion-avatar"),
        )
    }

    /// Where the file at avatar `path` with these bytes is kept; `None`
    /// when `sha256` is not a sha256.
    pub fn path_of(&self, sha256: &str, path: &str) -> Option<PathBuf> {
        cache_name(sha256, path).map(|name| self.dir.join(name))
    }

    pub fn has(&self, sha256: &str, path: &str) -> bool {
        self.path_of(sha256, path).is_some_and(|p| p.is_file())
    }

    /// Store `bytes`, the file at avatar `path`, under `expected_sha256`;
    /// refuses bytes that hash to something else. Written to a temporary
    /// file and renamed, so a half-written file never has a hash name.
    pub fn put(&self, expected_sha256: &str, path: &str, bytes: &[u8]) -> Result<PathBuf> {
        let Some(name) = cache_name(expected_sha256, path) else {
            bail!("{expected_sha256:?} is not a sha256");
        };
        let actual = sha256_hex(bytes);
        if actual != expected_sha256 {
            bail!("hash is {actual}, expected {expected_sha256}");
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        let tmp = self.dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let written = std::fs::File::create(&tmp).and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        });
        let dest = self.dir.join(name);
        let done = written.and_then(|()| std::fs::rename(&tmp, &dest));
        if let Err(e) = done {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("cannot write {}", dest.display()));
        }
        Ok(dest)
    }
}

/// The files the config references whose bytes the cache lacks, once per
/// distinct cache name. Files the config does not reference are not
/// wanted.
pub fn missing<'a>(manifest: &'a AvatarManifest, cache: &Cache) -> Vec<&'a AvatarFile> {
    let wanted = manifest.config.paths();
    let mut seen = std::collections::BTreeSet::new();
    manifest
        .files
        .iter()
        .filter(|f| wanted.contains(&f.path.as_str()))
        .filter(|f| !cache.has(&f.sha256, &f.path))
        .filter(|f| seen.insert(cache_name(&f.sha256, &f.path).unwrap_or_else(|| f.sha256.clone())))
        .collect()
}

/// An avatar whose every referenced file is in the cache.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedAvatar {
    pub config: AvatarConfig,
    /// Each referenced avatar path and the cached file holding its bytes.
    pub files: BTreeMap<String, PathBuf>,
}

impl LoadedAvatar {
    /// Where bevy's `AssetServer` loads the file at the avatar-relative
    /// `path` from: `companion-avatar://<sha256>.<ext>`, the cached copy
    /// under [`ASSET_SOURCE`]. `None` when the config does not reference
    /// `path`. Load through this, not the absolute [`Self::files`] path:
    /// bevy refuses paths outside its approved sources.
    pub fn asset_path(&self, path: &str) -> Option<AssetPath<'static>> {
        let name = self.files.get(path)?.file_name()?.to_str()?;
        Some(
            AssetPath::from_path_buf(PathBuf::from(name))
                .with_source(AssetSourceId::from(ASSET_SOURCE))
                .into_owned(),
        )
    }
}

/// Bring the cache up to date with `source` and describe the avatar it now
/// holds. Fails, caching nothing wrong, if a file cannot be fetched or its
/// bytes do not hash to what the manifest says.
pub async fn sync(source: &dyn AvatarSource, cache: &Cache) -> Result<LoadedAvatar> {
    let manifest = source.manifest().await?;
    for file in missing(&manifest, cache) {
        let bytes = source
            .fetch(&file.path)
            .await
            .with_context(|| format!("cannot fetch {}", file.path))?;
        cache
            .put(&file.sha256, &file.path, &bytes)
            .with_context(|| format!("cannot cache {}", file.path))?;
    }
    let mut files = BTreeMap::new();
    for path in manifest.config.paths() {
        let file = manifest
            .files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| anyhow!("the avatar config names {path}, which the agent lacks"))?;
        let cached = cache
            .path_of(&file.sha256, &file.path)
            .ok_or_else(|| anyhow!("the agent's hash of {path} is not a sha256"))?;
        files.insert(path.to_owned(), cached);
    }
    Ok(LoadedAvatar {
        config: manifest.config,
        files,
    })
}

/// What the renderers read: the last avatar that synced.
#[derive(Resource, Default)]
pub struct AvatarAssets {
    pub loaded: Option<LoadedAvatar>,
    /// Why the latest sync failed; `loaded` is then still the last good one.
    pub error: Option<String>,
    /// Bumps whenever `loaded` changes, so a renderer knows to reload.
    pub generation: u64,
}

impl AvatarAssets {
    pub fn apply(&mut self, result: Result<LoadedAvatar>) {
        match result {
            Ok(avatar) => {
                self.error = None;
                if self.loaded.as_ref() != Some(&avatar) {
                    self.loaded = Some(avatar);
                    self.generation += 1;
                }
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }
}

/// What [`AvatarOps`] needs of the agent: [`ChatClient`] in the app, a
/// fake in tests.
#[async_trait::async_trait]
pub trait AvatarEditor: Send + Sync {
    /// Stores `bytes` at the avatar-relative `path`.
    async fn upload(&self, path: &str, bytes: &[u8]) -> Result<()>;
    /// The agent's avatar config as it is now.
    async fn config(&self) -> Result<AvatarConfig>;
    /// Replaces the agent's avatar config. The agent refuses one that
    /// references a file it does not have.
    async fn set_config(&self, config: &AvatarConfig) -> Result<()>;
}

#[async_trait::async_trait]
impl AvatarEditor for ChatClient {
    async fn upload(&self, path: &str, bytes: &[u8]) -> Result<()> {
        self.avatar_upload(path, bytes).await
    }
    async fn config(&self) -> Result<AvatarConfig> {
        Ok(self.avatar_manifest().await?.config)
    }
    async fn set_config(&self, config: &AvatarConfig) -> Result<()> {
        self.avatar_set_config(config).await
    }
}

/// Looks up the live agent connection, if there is one.
pub type ClientLookup = Arc<dyn Fn() -> Option<Arc<dyn AvatarEditor>> + Send + Sync>;

/// Names one [`AvatarOps::apply`], to read its outcome with
/// [`AvatarOps::result`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpTicket(u64);

/// Whether ops are running, and how each source's last one failed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpsStatus {
    pub busy: bool,
    /// The error of each source's latest op, if it failed. Kept until
    /// that source starts another op or [`AvatarOps::dismiss`]es it.
    pub errors: BTreeMap<&'static str, String>,
}

/// How many finished tickets are remembered.
const KEPT_RESULTS: usize = 64;

#[derive(Default)]
struct OpsInner {
    running: usize,
    next_ticket: u64,
    errors: BTreeMap<&'static str, String>,
    results: BTreeMap<u64, Result<(), String>>,
}

/// Records an op's end however the task ends: a panicking op still stops
/// being "running" and reports a failure.
struct Finish {
    ops: AvatarOps,
    ticket: OpTicket,
    source: &'static str,
    result: Option<Result<(), String>>,
}

impl Finish {
    /// How the op ended; recorded when `self` drops.
    fn set(&mut self, result: Result<(), String>) {
        self.result = Some(result);
    }
}

impl Drop for Finish {
    fn drop(&mut self) {
        let result = self
            .result
            .take()
            .unwrap_or_else(|| Err("the operation stopped unexpectedly".to_owned()));
        {
            let mut inner = self.ops.lock();
            inner.running = inner.running.saturating_sub(1);
            match &result {
                Ok(()) => self.ops.sync_request.store(true, Ordering::Release),
                Err(e) => {
                    inner.errors.insert(self.source, e.clone());
                }
            }
            inner.results.insert(self.ticket.0, result);
            while inner.results.len() > KEPT_RESULTS {
                inner.results.pop_first();
            }
        }
        self.ops.wake();
    }
}

/// Changes to the agent's avatar made from the GUI. Renderer panels make
/// theirs with [`apply`](Self::apply): it runs on the GUI's runtime
/// through the live client, reports per source, and on success asks for a
/// sync so the manifest is refetched.
#[derive(Clone, Resource)]
pub struct AvatarOps {
    runtime: Handle,
    client: ClientLookup,
    sync_request: Arc<AtomicBool>,
    waker: Option<crate::Waker>,
    inner: Arc<Mutex<OpsInner>>,
    /// Held for a whole op, so ops never interleave.
    serial: Arc<tokio::sync::Mutex<()>>,
}

impl AvatarOps {
    /// Ops on `runtime` using whatever connection `client` finds. On
    /// success `sync_request` is set; the GUI turns that into a
    /// [`crate::chat::SyncRequested`].
    pub fn new(runtime: Handle, client: ClientLookup, sync_request: Arc<AtomicBool>) -> Self {
        Self {
            runtime,
            client,
            sync_request,
            waker: None,
            inner: Arc::default(),
            serial: Arc::default(),
        }
    }

    /// Wakes the GUI when an op finishes, so an idle window repaints its
    /// status.
    pub fn with_waker(mut self, waker: crate::Waker) -> Self {
        self.waker = Some(waker);
        self
    }

    pub fn status(&self) -> OpsStatus {
        let inner = self.lock();
        OpsStatus {
            busy: inner.running > 0,
            errors: inner.errors.clone(),
        }
    }

    /// How the op `ticket` names ended: `None` while it runs (or once
    /// forgotten, long after it ended).
    pub fn result(&self, ticket: &OpTicket) -> Option<Result<(), String>> {
        self.lock().results.get(&ticket.0).cloned()
    }

    /// Forgets `source`'s error.
    pub fn dismiss(&self, source: &'static str) {
        self.lock().errors.remove(source);
    }

    /// Whether an op asked for a sync since this was last called.
    pub fn take_sync_request(&self) -> bool {
        self.sync_request.swap(false, Ordering::AcqRel)
    }

    /// The way for a panel to change the avatar. In one task, one op at a
    /// time (a second `apply` waits for the first), it:
    ///
    /// 1. uploads each of `uploads` (avatar path, bytes), in order;
    /// 2. fetches the agent's config as it is now, so a panel's stale view
    ///    of it cannot undo another panel's edit;
    /// 3. runs `edit` on that config;
    /// 4. checks the result and sets it on the agent.
    ///
    /// It stops at the first error. The uploads come first because the
    /// agent refuses a config that references a file it lacks. `edit`
    /// refuses with `Err(why)` when the change no longer fits the config it
    /// is given (another device renamed or removed what it edits): nothing
    /// is set, and `why` is the op's error. `source` names the caller (a
    /// panel's title) for [`OpsStatus::errors`]; its previous error is
    /// cleared now. A sync follows success.
    pub fn apply(
        &self,
        source: &'static str,
        uploads: Vec<(String, Vec<u8>)>,
        edit: impl FnOnce(&mut AvatarConfig) -> Result<(), String> + Send + 'static,
    ) -> OpTicket {
        let ticket = {
            let mut inner = self.lock();
            let ticket = OpTicket(inner.next_ticket);
            inner.next_ticket += 1;
            inner.running += 1;
            inner.errors.remove(source);
            ticket
        };
        let mut finish = Finish {
            ops: self.clone(),
            ticket,
            source,
            result: None,
        };
        let Some(client) = (self.client)() else {
            finish.set(Err("not connected to the agent".to_owned()));
            return ticket;
        };
        let serial = Arc::clone(&self.serial);
        self.runtime.spawn(async move {
            let _one_at_a_time = serial.lock().await;
            let run = async {
                for (path, bytes) in &uploads {
                    client
                        .upload(path, bytes)
                        .await
                        .with_context(|| format!("cannot upload {path}"))?;
                }
                let mut config = client.config().await?;
                edit(&mut config).map_err(|e| anyhow!(e))?;
                config.check().map_err(|e| anyhow!(e))?;
                client.set_config(&config).await
            };
            finish.set(run.await.map_err(|e| format!("{e:#}")));
        });
        ticket
    }

    fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, OpsInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use sapphire_agent_core::{AvatarConfig, AvatarFile, AvatarManifest, SpriteConfig, SpritePose};

    use super::*;

    struct FakeSource {
        manifest: AvatarManifest,
        files: HashMap<String, Vec<u8>>,
        fetches: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl AvatarSource for FakeSource {
        async fn manifest(&self) -> anyhow::Result<AvatarManifest> {
            Ok(self.manifest.clone())
        }
        async fn fetch(&self, path: &str) -> anyhow::Result<Vec<u8>> {
            self.fetches.lock().unwrap().push(path.to_owned());
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such file {path}"))
        }
    }

    fn sha(bytes: &[u8]) -> String {
        sha256_hex(bytes)
    }

    fn config_of(paths: &[&str]) -> AvatarConfig {
        let mut pose = SpritePose {
            base: paths[0].to_owned(),
            ..Default::default()
        };
        for (i, p) in paths.iter().enumerate().skip(1) {
            pose.expressions.insert(format!("e{i}"), (*p).to_owned());
        }
        let mut sprite = SpriteConfig {
            default_pose: "p".to_owned(),
            ..Default::default()
        };
        sprite.poses.insert("p".to_owned(), pose);
        AvatarConfig {
            vrm: None,
            sprite: Some(sprite),
        }
    }

    fn file(path: &str, bytes: &[u8]) -> AvatarFile {
        AvatarFile {
            path: path.to_owned(),
            size: bytes.len() as u64,
            sha256: sha(bytes),
        }
    }

    #[tokio::test]
    async fn sync_fetches_only_referenced_files_missing_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        cache.put(&sha(b"B"), "b.png", b"B").unwrap();
        let source = FakeSource {
            manifest: AvatarManifest {
                config: config_of(&["a.png", "b.png"]),
                files: vec![
                    file("a.png", b"A"),
                    file("b.png", b"B"),
                    file("unused.png", b"U"),
                ],
            },
            files: HashMap::from([
                ("a.png".to_owned(), b"A".to_vec()),
                ("b.png".to_owned(), b"B".to_vec()),
                ("unused.png".to_owned(), b"U".to_vec()),
            ]),
            fetches: Mutex::new(Vec::new()),
        };
        let loaded = sync(&source, &cache).await.unwrap();
        assert_eq!(*source.fetches.lock().unwrap(), vec!["a.png".to_owned()]);
        assert_eq!(loaded.files.len(), 2);
        for path in loaded.files.values() {
            assert!(path.exists());
        }
        assert_eq!(std::fs::read(&loaded.files["a.png"]).unwrap(), b"A");
    }

    #[test]
    fn a_cache_name_is_the_hash_and_the_lowercase_extension() {
        let h = sha(b"x");
        assert_eq!(cache_name(&h, "body/model.VRM"), Some(format!("{h}.vrm")));
        assert_eq!(
            cache_name(&h, "pose.dir/base.png"),
            Some(format!("{h}.png"))
        );
        // No extension, or one bevy could not pick a loader by: the hash alone.
        assert_eq!(cache_name(&h, "pose.dir/base"), Some(h.clone()));
        assert_eq!(cache_name(&h, "base."), Some(h.clone()));
        assert_eq!(cache_name(&h, "base.p g"), Some(h.clone()));
        // Never a name outside the cache.
        assert_eq!(cache_name("../../etc/passwd", "a.png"), None);
        assert_eq!(cache_name(&h.to_uppercase(), "a.png"), None);
    }

    #[tokio::test]
    async fn cached_files_keep_their_extension_and_load_from_the_asset_source() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let source = FakeSource {
            manifest: AvatarManifest {
                config: config_of(&["poses/Base.PNG", "happy"]),
                files: vec![file("poses/Base.PNG", b"A"), file("happy", b"H")],
            },
            files: HashMap::from([
                ("poses/Base.PNG".to_owned(), b"A".to_vec()),
                ("happy".to_owned(), b"H".to_vec()),
            ]),
            fetches: Mutex::new(Vec::new()),
        };
        let loaded = sync(&source, &cache).await.unwrap();
        let base = format!("{}.png", sha(b"A"));
        assert_eq!(loaded.files["poses/Base.PNG"], dir.path().join(&base));
        assert!(cache.has(&sha(b"A"), "poses/Base.PNG"));

        let asset = loaded.asset_path("poses/Base.PNG").unwrap();
        assert_eq!(asset.to_string(), format!("companion-avatar://{base}"));
        assert_eq!(asset.get_full_extension(), Some("png"));
        assert_eq!(
            loaded.asset_path("happy").unwrap().to_string(),
            format!("companion-avatar://{}", sha(b"H"))
        );
        assert_eq!(loaded.asset_path("unreferenced.png"), None);
    }

    #[tokio::test]
    async fn a_download_whose_hash_differs_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let source = FakeSource {
            manifest: AvatarManifest {
                config: config_of(&["a.png"]),
                files: vec![file("a.png", b"A")],
            },
            files: HashMap::from([("a.png".to_owned(), b"tampered".to_vec())]),
            fetches: Mutex::new(Vec::new()),
        };
        let err = sync(&source, &cache).await.unwrap_err();
        assert!(format!("{err:#}").contains("a.png"), "{err:#}");
        assert!(!cache.has(&sha(b"A"), "a.png"));
        assert!(!cache.has(&sha(b"tampered"), "a.png"));
    }

    #[tokio::test]
    async fn an_empty_manifest_loads_an_empty_avatar() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let source = FakeSource {
            manifest: AvatarManifest::default(),
            files: HashMap::new(),
            fetches: Mutex::new(Vec::new()),
        };
        let loaded = sync(&source, &cache).await.unwrap();
        assert_eq!(
            loaded,
            LoadedAvatar {
                config: AvatarConfig::default(),
                files: Default::default()
            }
        );
        assert!(source.fetches.lock().unwrap().is_empty());
    }

    /// An agent whose avatar is a config and a set of file names, which
    /// refuses a config naming a file it lacks, like the real one.
    struct FakeEditor {
        state: Mutex<(AvatarConfig, Vec<String>)>,
        log: Mutex<Vec<String>>,
        fail_upload: Option<&'static str>,
    }

    impl FakeEditor {
        fn new(config: AvatarConfig) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new({
                    let files = config.paths().into_iter().map(str::to_owned).collect();
                    (config, files)
                }),
                log: Mutex::default(),
                fail_upload: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl AvatarEditor for FakeEditor {
        async fn upload(&self, path: &str, _: &[u8]) -> anyhow::Result<()> {
            self.log.lock().unwrap().push(format!("upload {path}"));
            if self.fail_upload == Some(path) {
                anyhow::bail!("disk full");
            }
            self.state.lock().unwrap().1.push(path.to_owned());
            Ok(())
        }
        async fn config(&self) -> anyhow::Result<AvatarConfig> {
            self.log.lock().unwrap().push("config".to_owned());
            Ok(self.state.lock().unwrap().0.clone())
        }
        async fn set_config(&self, config: &AvatarConfig) -> anyhow::Result<()> {
            self.log.lock().unwrap().push("set_config".to_owned());
            let mut state = self.state.lock().unwrap();
            if let Some(missing) = config
                .paths()
                .into_iter()
                .find(|p| !state.1.iter().any(|f| f == p))
            {
                anyhow::bail!("the agent lacks {missing}");
            }
            state.0 = config.clone();
            Ok(())
        }
    }

    fn ops_over(runtime: &tokio::runtime::Runtime, editor: Arc<FakeEditor>) -> AvatarOps {
        AvatarOps::new(
            runtime.handle().clone(),
            Arc::new(move || Some(Arc::clone(&editor) as Arc<dyn AvatarEditor>)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn wait_for(ops: &AvatarOps, ticket: &OpTicket) -> Result<(), String> {
        for _ in 0..500 {
            if let Some(result) = ops.result(ticket) {
                return result;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the op did not finish");
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap()
    }

    #[test]
    fn apply_uploads_before_set_config_and_edits_the_freshest_config() {
        let runtime = runtime();
        // The agent's config is "other panel's" by the time apply fetches.
        let editor = FakeEditor::new(config_of(&["old.png"]));
        let ops = ops_over(&runtime, Arc::clone(&editor));

        let ticket = ops.apply(
            "sprite",
            vec![("new.png".to_owned(), b"N".to_vec())],
            |config| {
                let sprite = config.sprite.as_mut().unwrap();
                sprite
                    .poses
                    .get_mut("p")
                    .unwrap()
                    .expressions
                    .insert("happy".into(), "new.png".into());
                Ok(())
            },
        );
        assert_eq!(wait_for(&ops, &ticket), Ok(()));

        assert_eq!(
            *editor.log.lock().unwrap(),
            ["upload new.png", "config", "set_config"]
        );
        let state = editor.state.lock().unwrap();
        let pose = &state.0.sprite.as_ref().unwrap().poses["p"];
        assert_eq!(
            pose.base, "old.png",
            "the fetched config was edited, not rebuilt"
        );
        assert_eq!(pose.expressions["happy"], "new.png");
        assert!(ops.take_sync_request());
        assert!(!ops.status().busy);
    }

    #[test]
    fn a_structural_error_is_reported_without_set_config() {
        let runtime = runtime();
        let editor = FakeEditor::new(config_of(&["a.png"]));
        let ops = ops_over(&runtime, Arc::clone(&editor));

        let ticket = ops.apply("sprite", vec![], |config| {
            config.sprite.as_mut().unwrap().default_pose = "missing-pose".to_owned();
            Ok(())
        });
        let error = wait_for(&ops, &ticket).unwrap_err();
        assert!(error.contains("missing-pose"), "{error}");
        assert!(
            !editor
                .log
                .lock()
                .unwrap()
                .contains(&"set_config".to_owned())
        );
        assert!(ops.status().errors["sprite"].contains("missing-pose"));
        assert!(!ops.take_sync_request());
    }

    #[test]
    fn a_refused_edit_is_reported_without_set_config() {
        let runtime = runtime();
        let editor = FakeEditor::new(config_of(&["a.png"]));
        let ops = ops_over(&runtime, Arc::clone(&editor));

        // The pose the panel saw is gone from the agent's newer config.
        let ticket = ops.apply("sprite", vec![], |_| {
            Err("pose gone was removed elsewhere".to_owned())
        });
        let error = wait_for(&ops, &ticket).unwrap_err();
        assert!(error.contains("pose gone"), "{error}");
        assert_eq!(*editor.log.lock().unwrap(), ["config"]);
        assert!(ops.status().errors["sprite"].contains("pose gone"));
        assert!(!ops.take_sync_request());
    }

    #[test]
    fn an_upload_error_stops_the_op_before_the_config() {
        let runtime = runtime();
        let mut editor = FakeEditor::new(config_of(&["a.png"]));
        Arc::get_mut(&mut editor).unwrap().fail_upload = Some("b.png");
        let ops = ops_over(&runtime, Arc::clone(&editor));
        let ticket = ops.apply(
            "sprite",
            vec![("b.png".to_owned(), vec![]), ("c.png".to_owned(), vec![])],
            |_| Ok(()),
        );
        let error = wait_for(&ops, &ticket).unwrap_err();
        assert!(
            error.contains("b.png") && error.contains("disk full"),
            "{error}"
        );
        assert_eq!(*editor.log.lock().unwrap(), ["upload b.png"]);
    }

    #[test]
    fn one_sources_error_survives_another_sources_op() {
        let runtime = runtime();
        let editor = FakeEditor::new(config_of(&["a.png"]));
        let ops = ops_over(&runtime, editor);

        let bad = ops.apply("vrm", vec![("x.vrm".to_owned(), vec![])], |config| {
            config.vrm = Some(Default::default()); // empty model: check() fails
            Ok(())
        });
        assert!(wait_for(&ops, &bad).is_err());
        let good = ops.apply("sprite", vec![], |_| Ok(()));
        assert_eq!(wait_for(&ops, &good), Ok(()));
        assert!(ops.status().errors.contains_key("vrm"), "kept");
        assert!(!ops.status().errors.contains_key("sprite"));

        // Its own next op clears it; so does dismissing.
        let again = ops.apply("vrm", vec![], |_| Ok(()));
        let _ = wait_for(&ops, &again);
        assert!(!ops.status().errors.contains_key("vrm"));
        let bad = ops.apply("vrm", vec![], |c| {
            c.vrm = Some(Default::default());
            Ok(())
        });
        assert!(wait_for(&ops, &bad).is_err());
        ops.dismiss("vrm");
        assert!(ops.status().errors.is_empty());
    }

    #[test]
    fn a_panicking_edit_does_not_leave_the_status_busy() {
        let runtime = runtime();
        let editor = FakeEditor::new(config_of(&["a.png"]));
        let ops = ops_over(&runtime, editor);
        let ticket = ops.apply("sprite", vec![], |_| -> Result<(), String> {
            panic!("boom")
        });
        assert!(wait_for(&ops, &ticket).is_err());
        assert!(!ops.status().busy);
        assert!(ops.status().errors.contains_key("sprite"));
    }

    #[test]
    fn without_a_connection_the_op_fails_at_once() {
        let runtime = runtime();
        let ops = AvatarOps::new(
            runtime.handle().clone(),
            Arc::new(|| None),
            Arc::new(AtomicBool::new(false)),
        );
        let ticket = ops.apply("sprite", vec![], |_| Ok(()));
        assert_eq!(
            ops.result(&ticket),
            Some(Err("not connected to the agent".into()))
        );
    }

    #[test]
    fn assets_keep_the_last_good_avatar_when_a_sync_fails() {
        let good = LoadedAvatar {
            config: AvatarConfig::default(),
            files: Default::default(),
        };
        let mut assets = AvatarAssets::default();
        assets.apply(Ok(good.clone()));
        assert_eq!(assets.generation, 1);
        assets.apply(Err(anyhow::anyhow!("offline")));
        assert_eq!(assets.loaded, Some(good.clone()));
        assert!(assets.error.is_some());
        assert_eq!(assets.generation, 1);
        // The same avatar again changes nothing but clears the error.
        assets.apply(Ok(good));
        assert!(assets.error.is_none());
        assert_eq!(assets.generation, 1);
    }
}
