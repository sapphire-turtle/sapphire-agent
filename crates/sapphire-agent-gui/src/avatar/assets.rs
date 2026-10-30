//! Fetching the avatar's files from the agent and keeping them in a local
//! cache, named by their sha256.
//!
//! Pure of bevy except for the [`AvatarAssets`] `Resource` derive. The
//! files come from an [`AvatarSource`] so the agent is not the only place
//! an avatar can come from.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use bevy::prelude::Resource;
use sapphire_agent_core::{AvatarConfig, AvatarFile, AvatarManifest, ChatClient};
use sha2::{Digest as _, Sha256};

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

/// A directory of files each named by the sha256 of its bytes.
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

    pub fn path_of(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256)
    }

    pub fn has(&self, sha256: &str) -> bool {
        is_sha256_hex(sha256) && self.path_of(sha256).is_file()
    }

    /// Store `bytes` under `expected_sha256`; refuses bytes that hash to
    /// something else. Written to a temporary file and renamed, so a
    /// half-written file never has a hash name.
    pub fn put(&self, expected_sha256: &str, bytes: &[u8]) -> Result<PathBuf> {
        if !is_sha256_hex(expected_sha256) {
            bail!("{expected_sha256:?} is not a sha256");
        }
        let actual = sha256_hex(bytes);
        if actual != expected_sha256 {
            bail!("hash is {actual}, expected {expected_sha256}");
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        let tmp = self
            .dir
            .join(format!(".{expected_sha256}.{}.tmp", std::process::id()));
        let written = std::fs::File::create(&tmp).and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        });
        let dest = self.path_of(expected_sha256);
        let done = written.and_then(|()| std::fs::rename(&tmp, &dest));
        if let Err(e) = done {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("cannot write {}", dest.display()));
        }
        Ok(dest)
    }
}

/// The files the config references whose bytes the cache lacks, once per
/// distinct hash. Files the config does not reference are not wanted.
pub fn missing<'a>(manifest: &'a AvatarManifest, cache: &Cache) -> Vec<&'a AvatarFile> {
    let wanted = manifest.config.paths();
    let mut seen = std::collections::BTreeSet::new();
    manifest
        .files
        .iter()
        .filter(|f| wanted.contains(&f.path.as_str()))
        .filter(|f| !cache.has(&f.sha256))
        .filter(|f| seen.insert(f.sha256.as_str()))
        .collect()
}

/// An avatar whose every referenced file is in the cache.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedAvatar {
    pub config: AvatarConfig,
    /// Each referenced avatar path and the cached file holding its bytes.
    pub files: BTreeMap<String, PathBuf>,
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
            .put(&file.sha256, &bytes)
            .with_context(|| format!("cannot cache {}", file.path))?;
    }
    let mut files = BTreeMap::new();
    for path in manifest.config.paths() {
        let file = manifest
            .files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| anyhow!("the avatar config names {path}, which the agent lacks"))?;
        files.insert(path.to_owned(), cache.path_of(&file.sha256));
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

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
        cache.put(&sha(b"B"), b"B").unwrap();
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
        assert!(!cache.has(&sha(b"A")));
        assert!(!cache.has(&sha(b"tampered")));
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
