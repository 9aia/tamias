//! tamias-vault-cached — a read-through, age-encrypted local cache in
//! front of any [`Vault`] backend (P-007).
//!
//! The cache is itself a `Vault` impl — a composite, not a bolt-on:
//! `get`/`list` serve the local cache while it's fresh (TTL),
//! miss → remote fetch → write-through; `put`/`rotate`/`rm` always
//! hit the remote first, then update or invalidate the cached row —
//! writes never serve stale. Rows carry the remote `version`/
//! `rotated_at` **verbatim** — a cached `get` answers with the
//! metadata the remote returned, not the cache's own bookkeeping.
//!
//! Freshness is TTL-bounded ([`CachePolicy::ttl`]): a cached row
//! or listing younger than the TTL answers without a remote
//! round-trip; past it the remote is re-fetched and the cache
//! updated. A remote *failure* falls back to the stale copy — that
//! degraded read is the offline mode the cache exists for.
//!
//! Env scoping is the safety rail: [`CachePolicy::envs`] decides
//! which envs may enter the cache at all — `dev` may cache
//! aggressively while `prod` live-fetches (a stale prod secret is a
//! footgun). `tamias-ops` builds the policy from `[vaults.*] cache`.
//!
//! Layout: rows live in a `Vault` of the caller's choice (ops wires
//! an `AgeVault` at `$XDG_CACHE_HOME/tamias/<vault>.age`, the honest
//! disposable-data placement); the cached value slot holds a JSON
//! row `{v, value, version, created_at, rotated_at, fetched_at}` —
//! ref paths and values stay inside the ciphertext like any vault
//! row, so the cache needs no crypto of its own. Per-env list
//! markers live in a small plaintext sidecar (`<vault>.listed.json`)
//! — env names and timestamps only, never refs or values. Every
//! cache interaction is best-effort: a corrupt row is a miss, a
//! cache that won't open degrades to remote-only pass-through, and
//! the remote is always the source of truth.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tamias_core::{Env, Ref, Secret};
use tamias_vault::{SecretRecord, Vault, VaultError};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use zeroize::Zeroizing;

/// Default freshness window — 300s. Long enough that a burst of
/// `tamias run`s costs one remote round-trip, short enough that a
/// rotation shows up inside a work session.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// On-disk cache-row format version.
const ROW_VERSION: u32 = 1;

/// Which envs the cache may hold + how fresh a row must be to
/// answer without a remote round-trip.
#[derive(Debug, Clone)]
pub struct CachePolicy {
    /// Envs allowed into the cache — `None` caches every env. An
    /// env outside the set passes straight through to the remote:
    /// its rows never land on disk.
    pub envs: Option<BTreeSet<String>>,
    /// Freshness window for rows and list markers.
    pub ttl: Duration,
}

impl CachePolicy {
    /// Cache every env.
    pub fn all(ttl: Duration) -> Self {
        CachePolicy { envs: None, ttl }
    }

    /// Cache only `envs` — every other env live-fetches.
    pub fn scoped(envs: impl IntoIterator<Item = Env>, ttl: Duration) -> Self {
        CachePolicy {
            envs: Some(envs.into_iter().map(|e| e.as_str().to_string()).collect()),
            ttl,
        }
    }

    /// Is `env` inside the cache scope?
    pub fn caches(&self, env: &Env) -> bool {
        match &self.envs {
            None => true,
            Some(set) => set.contains(env.as_str()),
        }
    }
}

/// One cached row — what the cache vault's value slot holds (JSON).
/// `version`/`created_at`/`rotated_at` are the *remote's* values,
/// preserved verbatim so a cached `get` reports the same metadata a
/// live one would; `fetched_at` is the cache's freshness stamp.
#[derive(Serialize, Deserialize)]
struct CacheRow {
    v: u32,
    value: Zeroizing<String>,
    version: u64,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    rotated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    fetched_at: OffsetDateTime,
}

impl CacheRow {
    fn wrap(rec: &SecretRecord) -> Self {
        CacheRow {
            v: ROW_VERSION,
            value: Zeroizing::new(rec.value.expose().to_string()),
            version: rec.version,
            created_at: rec.created_at,
            rotated_at: rec.rotated_at,
            fetched_at: OffsetDateTime::now_utc(),
        }
    }

    /// Within the freshness window?
    fn fresh(&self, ttl: Duration) -> bool {
        fresh(self.fetched_at, ttl)
    }

    /// The record the remote returned — verbatim metadata.
    fn record(&self) -> SecretRecord {
        SecretRecord {
            value: Secret::new(self.value.as_str()),
            version: self.version,
            created_at: self.created_at,
            rotated_at: self.rotated_at,
        }
    }
}

/// Is `t` younger than `ttl`? A `fetched_at` in the future (clock
/// skew) counts as *stale* — a timestamp that can't be trusted to
/// be young is distrusted, erring toward one extra remote fetch.
fn fresh(t: OffsetDateTime, ttl: Duration) -> bool {
    let age = OffsetDateTime::now_utc()
        .unix_timestamp()
        .saturating_sub(t.unix_timestamp());
    age >= 0 && (age as u64) < ttl.as_secs()
}

/// The list-marker sidecar — `{env → rfc3339}` per env whose
/// `list` answer is a complete remote snapshot. Plaintext on
/// purpose: it holds env names and timestamps, never refs or
/// values (those stay inside the cache vault's ciphertext).
#[derive(Default, Serialize, Deserialize)]
struct MetaFile {
    #[serde(default)]
    listed_at: BTreeMap<String, String>,
}

/// A [`Vault`] that reads through a local encrypted cache to a
/// remote source of truth. All cache access is best-effort — a
/// cache that's absent (`None`), corrupt, or fails mid-op degrades
/// to remote-only behavior, never to a wrong answer.
pub struct CachedVault {
    remote: Box<dyn Vault>,
    cache: Option<Box<dyn Vault>>,
    meta_path: Option<PathBuf>,
    policy: CachePolicy,
}

impl CachedVault {
    /// Wrap `remote` with `cache` as the local mirror. `cache` may
    /// be `None` — the vault then behaves as a pass-through (the
    /// degraded mode ops picks when the cache file won't open).
    /// `meta_path` is the list-marker sidecar; `None` means `list`
    /// never serves from the cache (no persisted freshness signal).
    pub fn new(
        remote: Box<dyn Vault>,
        cache: Option<Box<dyn Vault>>,
        meta_path: Option<PathBuf>,
        policy: CachePolicy,
    ) -> Self {
        CachedVault {
            remote,
            cache,
            meta_path,
            policy,
        }
    }

    /// The default cache vault file for a `[vaults]` name —
    /// `$XDG_CACHE_HOME/tamias/<name>.age`, else `~/.cache/tamias/`.
    /// XDG cache dir on purpose: disposable-by-convention data.
    pub fn default_cache_file(vault_name: &str) -> PathBuf {
        cache_dir().join(format!("{}.age", sanitize(vault_name)))
    }

    /// The default list-marker sidecar —
    /// `$XDG_CACHE_HOME/tamias/<name>.listed.json`.
    pub fn default_meta_file(vault_name: &str) -> PathBuf {
        cache_dir().join(format!("{}.listed.json", sanitize(vault_name)))
    }

    /// The cache vault + policy gate, or `None` when `env` isn't
    /// cacheable or no cache is wired.
    fn cache_for(&self, env: &Env) -> Option<&dyn Vault> {
        if !self.policy.caches(env) {
            return None;
        }
        self.cache.as_deref()
    }

    /// The cached row, or `None` — a missing row, a cache error,
    /// and an unparseable (corrupt) row are all just "not cached".
    /// A corrupt row is removed so the next write-through starts
    /// clean.
    fn cache_row(&self, cache: &dyn Vault, env: &Env, rref: &Ref) -> Option<CacheRow> {
        let rec = cache.get(env, rref).ok()??;
        match serde_json::from_str::<CacheRow>(rec.value.expose()) {
            Ok(row) => Some(row),
            Err(_) => {
                let _ = cache.rm(env, rref);
                None
            }
        }
    }

    /// Write-through: `rec` into the cache, best-effort — the
    /// remote already answered; a cache write failure must not
    /// turn a good remote op into an error.
    fn store_row(&self, cache: &dyn Vault, env: &Env, rref: &Ref, rec: &SecretRecord) {
        if let Ok(json) = serde_json::to_string(&CacheRow::wrap(rec)) {
            let _ = cache.put(env, rref, Secret::new(json));
        }
    }

    /// Drop refs the remote no longer has — called after a
    /// successful remote `list` so deletions propagate into the
    /// mirror. Lazy and best-effort: unknown remote refs stay
    /// uncached until their first `get`.
    fn reconcile(&self, cache: &dyn Vault, env: &Env, remote: &[Ref]) {
        if let Ok(cached) = cache.list(env) {
            for r in cached.iter().filter(|r| !remote.contains(r)) {
                let _ = cache.rm(env, r);
            }
        }
    }

    /// The env's list-freshness marker, or `None` — a missing,
    /// unreadable, or unparsable sidecar is "never listed".
    fn listed_at(&self, env: &Env) -> Option<OffsetDateTime> {
        let text = std::fs::read_to_string(self.meta_path.as_ref()?).ok()?;
        let meta: MetaFile = serde_json::from_str(&text).ok()?;
        OffsetDateTime::parse(meta.listed_at.get(env.as_str())?, &Rfc3339).ok()
    }

    /// Stamp `env`'s list marker — best-effort; a failed write just
    /// means the next `list` re-fetches.
    fn mark_listed(&self, env: &Env) {
        let Some(path) = &self.meta_path else {
            return;
        };
        let mut meta: MetaFile = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        meta.listed_at.insert(
            env.as_str().to_string(),
            OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default(),
        );
        if let Ok(json) = serde_json::to_string(&meta) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(path, json).is_ok() {
                let _ = set_private(path);
            }
        }
    }
}

impl Vault for CachedVault {
    fn get(&self, env: &Env, rref: &Ref) -> Result<Option<SecretRecord>, VaultError> {
        let Some(cache) = self.cache_for(env) else {
            return self.remote.get(env, rref);
        };
        let row = self.cache_row(cache, env, rref);
        if let Some(fresh) = row.as_ref().filter(|r| r.fresh(self.policy.ttl)) {
            return Ok(Some(fresh.record()));
        }
        match self.remote.get(env, rref) {
            Ok(Some(rec)) => {
                self.store_row(cache, env, rref, &rec);
                Ok(Some(rec))
            }
            Ok(None) => {
                // remote is authoritative — a stale row for a
                // deleted ref is worse than a miss
                let _ = cache.rm(env, rref);
                Ok(None)
            }
            Err(e) => match row {
                // remote unreachable but a stale copy exists — the
                // degraded read that makes the cache worth having
                Some(row) => Ok(Some(row.record())),
                None => Err(e),
            },
        }
    }

    fn put(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        // remote-first: writes never serve stale. A remote failure
        // leaves the cache untouched — the old row still mirrors
        // the old remote state.
        let rec = self.remote.put(env, rref, value)?;
        if let Some(cache) = self.cache_for(env) {
            self.store_row(cache, env, rref, &rec);
        }
        Ok(rec)
    }

    fn rm(&self, env: &Env, rref: &Ref) -> Result<bool, VaultError> {
        let removed = self.remote.rm(env, rref)?;
        if let Some(cache) = self.cache_for(env) {
            // invalidate even when `removed` is false — the remote
            // not having it means any cached row is stale
            let _ = cache.rm(env, rref);
        }
        Ok(removed)
    }

    fn list(&self, env: &Env) -> Result<Vec<Ref>, VaultError> {
        let Some(cache) = self.cache_for(env) else {
            return self.remote.list(env);
        };
        let marker = self.listed_at(env);
        if marker.is_some_and(|t| fresh(t, self.policy.ttl)) {
            if let Ok(refs) = cache.list(env) {
                return Ok(refs);
            }
        }
        match self.remote.list(env) {
            Ok(refs) => {
                self.reconcile(cache, env, &refs);
                self.mark_listed(env);
                Ok(refs)
            }
            Err(e) => match marker {
                // ever listed + remote down → the stale snapshot is
                // still the best answer
                Some(_) => cache.list(env),
                None => Err(e),
            },
        }
    }

    fn rotate(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        let rec = self.remote.rotate(env, rref, value)?;
        if let Some(cache) = self.cache_for(env) {
            self.store_row(cache, env, rref, &rec);
        }
        Ok(rec)
    }
}

/// `$XDG_CACHE_HOME/tamias` or `~/.cache/tamias`.
fn cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache"))
        .join("tamias")
}

/// `~` for path defaults.
fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// A `[vaults]` name is a TOML key — anything goes, including
/// `..` and `/`. Map it onto a single safe filename component.
fn sanitize(name: &str) -> String {
    let out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        "_".into()
    } else {
        out
    }
}

/// `chmod 0600` — the sidecar is non-sensitive but
/// private-by-default is still right.
#[cfg(unix)]
fn set_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn policy_scopes_envs() {
        let all = CachePolicy::all(Duration::from_secs(60));
        assert!(all.caches(&Env::dev()));
        assert!(all.caches(&Env::new("prod").unwrap()));
        let scoped = CachePolicy::scoped([Env::dev()], Duration::from_secs(60));
        assert!(scoped.caches(&Env::dev()));
        assert!(!scoped.caches(&Env::new("prod").unwrap()));
    }

    #[test]
    fn default_paths_follow_xdg() {
        temp_env::with_vars(
            [
                ("XDG_CACHE_HOME", Some("/tmp/xdg-c")),
                ("HOME", Some("/tmp/fakehome")),
            ],
            || {
                assert_eq!(
                    CachedVault::default_cache_file("cloud"),
                    PathBuf::from("/tmp/xdg-c/tamias/cloud.age")
                );
                assert_eq!(
                    CachedVault::default_meta_file("cloud"),
                    PathBuf::from("/tmp/xdg-c/tamias/cloud.listed.json")
                );
            },
        );
        temp_env::with_vars(
            [
                ("XDG_CACHE_HOME", None::<&str>),
                ("HOME", Some("/tmp/fakehome")),
            ],
            || {
                assert_eq!(
                    CachedVault::default_cache_file("cloud"),
                    PathBuf::from("/tmp/fakehome/.cache/tamias/cloud.age")
                );
            },
        );
    }

    #[test]
    fn sanitize_blocks_traversal() {
        assert_eq!(sanitize("cloud"), "cloud");
        assert_eq!(sanitize("../etc"), ".._etc");
        assert_eq!(sanitize("a/b"), "a_b");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize("weird name"), "weird_name");
    }

    #[test]
    fn freshness_window() {
        let ttl = Duration::from_secs(60);
        assert!(fresh(OffsetDateTime::now_utc(), ttl));
        // a row older than the TTL is stale; zero TTL is never fresh
        assert!(!fresh(
            OffsetDateTime::now_utc() - time::Duration::hours(1),
            ttl
        ));
        assert!(!fresh(OffsetDateTime::now_utc(), Duration::ZERO));
        // fetched_at in the future (clock skew) is distrusted → stale
        assert!(!fresh(
            OffsetDateTime::now_utc() + time::Duration::hours(1),
            ttl
        ));
    }
}
