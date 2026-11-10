#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-cache-local — the content-addressed fetch cache:
//! `<root>/<key>/` payloads (`git-<sha>`, `sha256-<hex>`). Install is
//! fetch→unpack, so a warm cache makes repeat installs and air-gapped
//! nodes free. `remove` never touches it; eviction is by age or full.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tamias_core::{dir_digest, write_atomic, Error};

/// Cache keys are `[A-Za-z0-9._-]+`.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && key != "."
        && key != ".."
}

/// Default `cache clean` eviction window — entries older than 30 days
/// go. Shared by the CLI's `cache clean` default, `tamias serve`'s
/// `DEFAULT_CACHE_MAX_AGE`, and `tamias ui`'s clean job.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 86_400);

/// Integrity metadata written at commit time — one `<root>/.meta/<key>.json`
/// per entry. It lives *outside* the payload dir so it never enters
/// `dir_digest`, `copy_tree`, or `entries()`.
const META_DIR: &str = ".meta";
const META_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EntryMeta {
    /// Schema version — unknown versions read as unverifiable (a miss).
    version: u32,
    /// `sha256:<hex>` of the entry's payload files at commit time —
    /// the same `dir_digest` the store's `verify` recomputes.
    digest: String,
}

/// The content-addressed fetch cache — one per root.
pub struct FetchCache {
    root: PathBuf,
}

impl FetchCache {
    /// A cache under `root` (created lazily on first commit).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root dir.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path for a key — caller checks [`FetchCache::has`].
    #[must_use]
    pub fn entry(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Whether a populated entry exists for `key` — *unverified*: use
    /// [`FetchCache::hit`] for installs.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        valid_key(key) && self.entry(key).is_dir()
    }

    /// Integrity-checked lookup for the install path: `Some(entry)` only
    /// when the entry exists *and* its payload still matches the digest
    /// recorded at commit time. A tampered, unreadable, or pre-metadata
    /// (legacy) entry is evicted — dir and meta — and reported as a miss
    /// so the caller refetches instead of installing unverified bytes.
    ///
    /// # Errors
    /// Propagates failures evicting a rejected entry.
    pub fn hit(&self, key: &str) -> Result<Option<PathBuf>, Error> {
        let entry = self.entry(key);
        if !valid_key(key) || !entry.is_dir() {
            return Ok(None);
        }
        if self.entry_intact(key, &entry) {
            return Ok(Some(entry));
        }
        self.evict(key)?;
        Ok(None)
    }

    /// A scratch dir for an in-flight fetch.
    ///
    /// # Errors
    /// Propagates I/O failures creating the scratch dir.
    pub fn scratch(&self) -> Result<PathBuf, Error> {
        let ctx = format!("create {}", self.root.display());
        fs::create_dir_all(&self.root).map_err(Error::io(ctx))?;
        let scratch = self.root.join(format!(".scratch-{}", std::process::id()));
        if scratch.exists() {
            let clear = format!("clear {}", scratch.display());
            fs::remove_dir_all(&scratch).map_err(Error::io(clear))?;
        }
        fs::create_dir_all(&scratch).map_err(Error::io(format!("create {}", scratch.display())))?;
        Ok(scratch)
    }

    /// Fold a filled scratch dir into `entry(key)` — an existing *verified*
    /// entry wins (the scratch is discarded); a corrupt or legacy one is
    /// evicted and replaced by the fresh fetch. Either way the committed
    /// entry carries integrity metadata, so later [`FetchCache::hit`]s can
    /// prove the payload is what was fetched.
    ///
    /// # Errors
    /// Fails on an invalid key, an unreadable scratch payload, or an I/O
    /// error writing metadata/renaming.
    pub fn commit(&self, scratch: &Path, key: &str) -> Result<PathBuf, Error> {
        if !valid_key(key) {
            return Err(Error::Failed(format!("invalid cache key '{key}'")));
        }
        let dest = self.entry(key);
        if dest.exists() {
            if self.entry_intact(key, &dest) {
                let clear = format!("clear {}", scratch.display());
                fs::remove_dir_all(scratch).map_err(Error::io(clear))?;
                return Ok(dest);
            }
            // Stale/corrupt/legacy entry — never keep it over fresh bytes.
            self.evict(key)?;
        }
        // Digest *before* the rename: a payload that can't be read can't
        // be proven intact later, so the commit fails rather than
        // publishing an unverifiable entry.
        let digest = dir_digest(scratch)?;
        let meta = EntryMeta {
            version: META_VERSION,
            digest,
        };
        let json = serde_json::to_string(&meta).map_err(|e| Error::Failed(format!("meta: {e}")))?;
        let meta_path = self.meta_path(key);
        if let Some(parent) = meta_path.parent() {
            fs::create_dir_all(parent)
                .map_err(Error::io(format!("create {}", parent.display())))?;
        }
        write_atomic(&meta_path, json.as_bytes())?;
        let ctx = format!("commit {} → {}", scratch.display(), dest.display());
        fs::rename(scratch, &dest).map_err(Error::io(ctx))?;
        Ok(dest)
    }

    /// Abandon a scratch dir after a failed fetch.
    pub fn discard(&self, scratch: &Path) {
        if scratch.exists() {
            let _ = fs::remove_dir_all(scratch);
        }
    }

    /// `<root>/.meta/<key>.json` — the entry's integrity record.
    fn meta_path(&self, key: &str) -> PathBuf {
        self.root.join(META_DIR).join(format!("{key}.json"))
    }

    /// Whether `entry`'s payload still matches the digest its meta
    /// recorded. Any doubt — missing/unparseable/foreign-version meta,
    /// unreadable payload, mismatch — reads as *not* intact.
    fn entry_intact(&self, key: &str, entry: &Path) -> bool {
        let Ok(text) = fs::read_to_string(self.meta_path(key)) else {
            return false;
        };
        let Ok(meta) = serde_json::from_str::<EntryMeta>(&text) else {
            return false;
        };
        meta.version == META_VERSION
            && meta.digest.starts_with("sha256:")
            && dir_digest(entry).is_ok_and(|d| d == meta.digest)
    }

    /// Remove an entry dir (a symlink is unlinked, never traversed) and
    /// its meta — the eviction half of [`FetchCache::hit`]/[`FetchCache::commit`].
    fn evict(&self, key: &str) -> Result<(), Error> {
        let entry = self.entry(key);
        if let Ok(meta_fs) = fs::symlink_metadata(&entry) {
            let result = if meta_fs.file_type().is_symlink() || meta_fs.is_file() {
                fs::remove_file(&entry)
            } else {
                fs::remove_dir_all(&entry)
            };
            result.map_err(Error::io(format!("evict {}", entry.display())))?;
        }
        let meta = self.meta_path(key);
        if meta.exists() {
            fs::remove_file(&meta).map_err(Error::io(format!("evict {}", meta.display())))?;
        }
        Ok(())
    }

    /// Entry dirs (dotfiles excluded — scratches aren't entries).
    ///
    /// # Errors
    /// Propagates read failures.
    pub fn entries(&self) -> Result<Vec<PathBuf>, Error> {
        let mut out = Vec::new();
        if self.root.is_dir() {
            let ctx = format!("read {}", self.root.display());
            let read = fs::read_dir(&self.root).map_err(Error::io(ctx))?;
            for entry in read {
                let path = entry.map_err(Error::io("read cache entry"))?.path();
                let hidden = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'));
                if path.is_dir() && !hidden {
                    out.push(path);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Evict entries whose mtime is older than `max_age`.
    ///
    /// # Errors
    /// Propagates read/remove failures.
    pub fn clean(&self, max_age: Duration) -> Result<Vec<PathBuf>, Error> {
        let now = SystemTime::now();
        let mut evicted = Vec::new();
        for path in self.entries()? {
            let stale = fs::metadata(&path)
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| now.duration_since(mtime).is_ok_and(|age| age >= max_age));
            if stale {
                let ctx = format!("remove {}", path.display());
                fs::remove_dir_all(&path).map_err(Error::io(ctx))?;
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let meta = self.meta_path(name);
                    if meta.exists() {
                        fs::remove_file(&meta)
                            .map_err(Error::io(format!("remove {}", meta.display())))?;
                    }
                }
                evicted.push(path);
            }
        }
        Ok(evicted)
    }

    /// Full eviction — `cache clean --all`.
    ///
    /// # Errors
    /// Propagates read/remove failures.
    pub fn clean_all(&self) -> Result<Vec<PathBuf>, Error> {
        self.clean(Duration::ZERO)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn scratch_commit_has_clean() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path());
        assert!(!cache.has("sha256-x"));
        let scratch = cache.scratch().unwrap();
        std::fs::write(scratch.join("f"), "x").unwrap();
        cache.commit(&scratch, "sha256-x").unwrap();
        assert!(cache.has("sha256-x"));
        assert_eq!(cache.entries().unwrap().len(), 1);
        assert_eq!(cache.clean_all().unwrap().len(), 1);
        assert!(!cache.has("sha256-x"));
    }

    #[test]
    fn invalid_key_and_discard() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path().join("fetch"));
        assert!(cache
            .commit(std::path::Path::new("/nope"), "bad/key")
            .is_err());
        let scratch = cache.scratch().unwrap();
        cache.discard(&scratch);
        assert!(!scratch.exists());
        cache.discard(std::path::Path::new("/nonexistent"));
    }

    #[test]
    fn clean_by_age_and_hidden_excluded() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path());
        let scratch = cache.scratch().unwrap();
        std::fs::write(scratch.join("f"), "x").unwrap();
        cache.commit(&scratch, "k1").unwrap();
        std::fs::create_dir_all(root.path().join(".hidden")).unwrap();
        assert_eq!(cache.entries().unwrap().len(), 1);
        assert_eq!(cache.clean(Duration::from_secs(0)).unwrap().len(), 1);
    }

    #[test]
    fn key_validation_and_existing_entry() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path());
        assert!(!cache.has("bad/key"));
        assert!(!cache.has(".."));
        let s1 = cache.scratch().unwrap();
        std::fs::write(s1.join("f"), "x").unwrap();
        cache.commit(&s1, "k").unwrap();
        let s2 = cache.scratch().unwrap();
        std::fs::write(s2.join("f"), "y").unwrap();
        assert!(cache.commit(&s2, "k").unwrap().is_dir());
        assert!(cache.root().is_dir());
        assert!(cache.entry("k").is_dir());
    }

    #[test]
    fn scratch_reuse_clears_stale() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path());
        let s1 = cache.scratch().unwrap();
        std::fs::write(s1.join("f"), "x").unwrap();
        let s2 = cache.scratch().unwrap();
        assert!(s2.is_dir());
        assert!(!s1.join("f").exists());
    }

    #[test]
    fn commit_missing_scratch_errors() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path().join("fetch"));
        let missing = root.path().join("nope");
        assert!(cache.commit(&missing, "sha256-abc").is_err());
    }

    #[test]
    fn hit_verifies_and_evicts_corruption() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path());
        let scratch = cache.scratch().unwrap();
        std::fs::write(scratch.join("f"), "good").unwrap();
        cache.commit(&scratch, "k").unwrap();
        // intact entry → verified hit
        assert_eq!(cache.hit("k").unwrap(), Some(cache.entry("k")));
        // tamper → miss + eviction (dir and meta both gone)
        std::fs::write(cache.entry("k").join("f"), "evil").unwrap();
        assert_eq!(cache.hit("k").unwrap(), None);
        assert!(!cache.has("k"));
        assert!(!cache.meta_path("k").exists());
        // a fresh commit under the same key works and re-verifies
        let s2 = cache.scratch().unwrap();
        std::fs::write(s2.join("f"), "good").unwrap();
        cache.commit(&s2, "k").unwrap();
        assert!(cache.hit("k").unwrap().is_some());
    }

    #[test]
    fn hit_treats_legacy_and_missing_meta_as_miss() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path());
        // pre-change entry: populated dir, no .meta record
        let legacy = cache.entry("old");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("f"), "x").unwrap();
        assert_eq!(cache.hit("old").unwrap(), None);
        assert!(!legacy.exists());
        // meta deleted after a real commit → same treatment
        let scratch = cache.scratch().unwrap();
        std::fs::write(scratch.join("f"), "x").unwrap();
        cache.commit(&scratch, "k").unwrap();
        std::fs::remove_file(cache.meta_path("k")).unwrap();
        assert_eq!(cache.hit("k").unwrap(), None);
        assert!(!cache.entry("k").exists());
        // garbage meta → miss too
        let s2 = cache.scratch().unwrap();
        std::fs::write(s2.join("f"), "x").unwrap();
        cache.commit(&s2, "k2").unwrap();
        std::fs::write(cache.meta_path("k2"), "not json").unwrap();
        assert_eq!(cache.hit("k2").unwrap(), None);
    }

    #[test]
    fn commit_replaces_corrupt_existing_entry() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path());
        let s1 = cache.scratch().unwrap();
        std::fs::write(s1.join("f"), "v1").unwrap();
        cache.commit(&s1, "k").unwrap();
        // poison the committed entry, then commit fresh bytes under
        // the same key (the no-prefetch-identity path, e.g. fs sources)
        std::fs::write(cache.entry("k").join("f"), "poison").unwrap();
        let s2 = cache.scratch().unwrap();
        std::fs::write(s2.join("f"), "v2").unwrap();
        let dest = cache.commit(&s2, "k").unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("f")).unwrap(), "v2");
        assert!(cache.hit("k").unwrap().is_some());
    }

    #[test]
    fn clean_evicts_meta_with_entry() {
        let root = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(root.path());
        let scratch = cache.scratch().unwrap();
        std::fs::write(scratch.join("f"), "x").unwrap();
        cache.commit(&scratch, "k1").unwrap();
        assert!(cache.meta_path("k1").exists());
        assert_eq!(cache.clean(Duration::from_secs(0)).unwrap().len(), 1);
        assert!(!cache.meta_path("k1").exists());
    }

    #[cfg(unix)]
    #[test]
    fn io_error_arms() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        // root's parent read-only → scratch() create fails
        let ro = base.path().join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let cache = FetchCache::new(ro.join("cache"));
        assert!(cache.scratch().is_err());
        // unreadable root → entries()/clean() read fail; commit rename fails
        let r2 = base.path().join("r2");
        std::fs::create_dir_all(r2.join("k1")).unwrap();
        let c2 = FetchCache::new(&r2);
        assert_eq!(c2.entries().unwrap().len(), 1);
        std::fs::set_permissions(&r2, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(c2.entries().is_err());
        assert!(c2.clean(Duration::from_secs(0)).is_err());
        assert!(c2.commit(&c2.entry("nope"), "k2").is_err());
        std::fs::set_permissions(&r2, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
