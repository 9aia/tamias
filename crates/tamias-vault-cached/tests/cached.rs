//! Behavior tests for [`CachedVault`] — a controllable in-memory
//! remote (`MapVault`) behind a real `AgeVault` cache, so the
//! cache side exercises the actual encrypted-file path.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use age::x25519;
use tamias_core::{Env, Ref, Secret};
use tamias_vault::{SecretRecord, Vault, VaultError};
use tamias_vault_age::AgeVault;
use tamias_vault_cached::{CachePolicy, CachedVault};
use tempfile::TempDir;
use time::OffsetDateTime;

const FAKE: &str = "fake-value-not-a-real-secret";

/// An in-memory `Vault` with the conformance semantics — the test
/// keeps a handle (`Clone`) so "another writer" and "the remote is
/// down" are both expressible. `AgeVault` won't do here: its `get`
/// serves an open-time snapshot, so it can't model a remote that
/// changes underneath us.
#[derive(Clone, Default)]
struct MapVault {
    rows: Arc<Mutex<BTreeMap<(String, String), SecretRecord>>>,
    fail: Arc<AtomicBool>,
}

impl MapVault {
    fn down(&self, yes: bool) {
        self.fail.store(yes, Ordering::SeqCst);
    }

    fn gate<T>(&self, f: impl FnOnce() -> Result<T, VaultError>) -> Result<T, VaultError> {
        if self.fail.load(Ordering::SeqCst) {
            Err(VaultError::Io("remote is down".into()))
        } else {
            f()
        }
    }
}

impl Vault for MapVault {
    fn get(&self, env: &Env, rref: &Ref) -> Result<Option<SecretRecord>, VaultError> {
        self.gate(|| {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(env.as_str().into(), rref.path()))
                .cloned())
        })
    }

    fn put(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        self.gate(|| {
            let mut rows = self.rows.lock().unwrap();
            let key = (env.as_str().into(), rref.path());
            let rec = match rows.get(&key) {
                Some(old) => SecretRecord {
                    value,
                    version: old.version + 1,
                    created_at: old.created_at,
                    rotated_at: old.rotated_at,
                },
                None => SecretRecord {
                    value,
                    version: 1,
                    created_at: OffsetDateTime::now_utc(),
                    rotated_at: None,
                },
            };
            rows.insert(key, rec.clone());
            Ok(rec)
        })
    }

    fn rm(&self, env: &Env, rref: &Ref) -> Result<bool, VaultError> {
        self.gate(|| {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .remove(&(env.as_str().into(), rref.path()))
                .is_some())
        })
    }

    fn list(&self, env: &Env) -> Result<Vec<Ref>, VaultError> {
        self.gate(|| {
            let rows = self.rows.lock().unwrap();
            let mut out: Vec<Ref> = rows
                .keys()
                .filter(|(e, _)| e == env.as_str())
                .map(|(_, r)| Ref::from_path(r).unwrap())
                .collect();
            out.sort();
            Ok(out)
        })
    }

    fn rotate(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        self.gate(|| {
            let mut rows = self.rows.lock().unwrap();
            let key = (env.as_str().into(), rref.path());
            let old = rows.get(&key).ok_or_else(|| VaultError::NotFound {
                rref: rref.path(),
                env: env.as_str().into(),
            })?;
            let rec = SecretRecord {
                value,
                version: old.version + 1,
                created_at: old.created_at,
                rotated_at: Some(OffsetDateTime::now_utc()),
            };
            rows.insert(key, rec.clone());
            Ok(rec)
        })
    }
}

/// A rig: CachedVault over a MapVault remote + a real AgeVault
/// cache file, plus handles for poking the remote/cache directly.
struct Rig {
    _dir: TempDir,
    vault: CachedVault,
    remote: MapVault,
    identity: x25519::Identity,
    cache_path: std::path::PathBuf,
    meta_path: std::path::PathBuf,
}

fn rig(ttl: Duration, envs: Option<Vec<Env>>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let identity = x25519::Identity::generate();
    let cache_path = dir.path().join("cache.age");
    let meta_path = dir.path().join("cache.listed.json");
    let remote = MapVault::default();
    let cache = AgeVault::open(&cache_path, identity.clone(), &[]).unwrap();
    let policy = match envs {
        None => CachePolicy::all(ttl),
        Some(es) => CachePolicy::scoped(es, ttl),
    };
    let vault = CachedVault::new(
        Box::new(remote.clone()),
        Some(Box::new(cache)),
        Some(meta_path.clone()),
        policy,
    );
    Rig {
        _dir: dir,
        vault,
        remote,
        identity,
        cache_path,
        meta_path,
    }
}

fn dev() -> Env {
    Env::dev()
}

fn prod() -> Env {
    Env::new("prod").unwrap()
}

fn rref(path: &str) -> Ref {
    Ref::new(&format!("tamias://{path}")).unwrap()
}

/// Reopen the cache file — a second read/write handle on the same
/// encrypted store, for asserting what the cache actually holds.
fn open_cache(r: &Rig) -> AgeVault {
    AgeVault::open(&r.cache_path, r.identity.clone(), &[]).unwrap()
}

#[test]
fn miss_fetches_remote_and_writes_through() {
    let r = rig(Duration::from_secs(300), None);
    let rf = rref("a/b");
    // a row the cache never saw (another client's write)
    r.remote.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    let got = r.vault.get(&dev(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), FAKE);
    assert_eq!(got.version, 1);
    // write-through: the cache now holds it — remote down still reads
    r.remote.down(true);
    let got = r.vault.get(&dev(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), FAKE);
    assert_eq!(got.version, 1);
}

#[test]
fn fresh_hit_never_touches_the_remote() {
    let r = rig(Duration::from_secs(300), None);
    let rf = rref("a/b");
    r.vault.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    // change the remote behind the cache's back — a fresh row still
    // answers the cached copy (that's what TTL buys)
    r.remote
        .put(&dev(), &rf, Secret::new("fake-remote-changed"))
        .unwrap();
    r.remote.down(true);
    let got = r.vault.get(&dev(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), FAKE);
}

#[test]
fn stale_row_refetches_and_verbatim_metadata_roundtrips() {
    let r = rig(Duration::ZERO, None);
    let rf = rref("a/b");
    r.vault.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    let rec2 = r
        .remote
        .put(&dev(), &rf, Secret::new("fake-rotated-remote"))
        .unwrap();
    // ttl=0 → every row is stale → remote refetched; the remote's
    // version/created_at come back verbatim, not the cache's
    let got = r.vault.get(&dev(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), "fake-rotated-remote");
    assert_eq!(got.version, rec2.version);
    assert_eq!(got.created_at, rec2.created_at);
}

#[test]
fn remote_failure_serves_the_stale_row() {
    let r = rig(Duration::ZERO, None);
    let rf = rref("a/b");
    r.vault.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    r.remote
        .put(&dev(), &rf, Secret::new("fake-newer-remote"))
        .unwrap();
    r.remote.down(true);
    // stale and the remote is down → the old copy is the best answer
    let got = r.vault.get(&dev(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), FAKE);
}

#[test]
fn get_none_from_remote_invalidates_the_row() {
    // ttl=0 → every row is stale → the remote is consulted on each
    // get, so its `None` answer is authoritative and drops the row
    let r = rig(Duration::ZERO, None);
    let rf = rref("a/b");
    r.vault.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    r.remote.rm(&dev(), &rf).unwrap();
    assert!(r.vault.get(&dev(), &rf).unwrap().is_none());
    // …so no stale fallback remains when the remote goes down
    r.remote.down(true);
    assert!(r.vault.get(&dev(), &rf).is_err());
}

#[test]
fn writes_are_remote_first_then_update_the_cache() {
    let r = rig(Duration::from_secs(300), None);
    let rf = rref("a/b");
    // put → remote first, cache mirror updated
    let rec = r.vault.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    assert_eq!(rec.version, 1);
    r.remote.down(true);
    assert_eq!(
        r.vault.get(&dev(), &rf).unwrap().unwrap().value.expose(),
        FAKE
    );
    // a failed remote write doesn't touch the cached row
    assert!(r
        .vault
        .put(&dev(), &rf, Secret::new("fake-should-not-land"))
        .is_err());
    assert_eq!(
        r.vault.get(&dev(), &rf).unwrap().unwrap().value.expose(),
        FAKE
    );
    // rotate → remote first; version/rotated_at mirror the remote
    r.remote.down(false);
    let rot = r
        .vault
        .rotate(&dev(), &rf, Secret::new("fake-rotated"))
        .unwrap();
    assert_eq!(rot.version, 2);
    assert!(rot.rotated_at.is_some());
    r.remote.down(true);
    let got = r.vault.get(&dev(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), "fake-rotated");
    assert_eq!(got.version, 2);
    assert!(got.rotated_at.is_some());
    // rotate of a missing ref propagates NotFound through the cache
    r.remote.down(false);
    assert!(matches!(
        r.vault.rotate(&dev(), &rref("not/here"), Secret::new(FAKE)),
        Err(VaultError::NotFound { .. })
    ));
}

#[test]
fn rm_is_remote_first_and_invalidates() {
    let r = rig(Duration::from_secs(300), None);
    let rf = rref("a/b");
    r.vault.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    assert!(r.vault.rm(&dev(), &rf).unwrap());
    // row is gone from the cache — no stale answer once the remote
    // is unreachable
    r.remote.down(true);
    assert!(r.vault.get(&dev(), &rf).is_err());
    r.remote.down(false);
    // rm of a missing ref is false, and still invalidates a
    // hypothetical stale row
    assert!(!r.vault.rm(&dev(), &rf).unwrap());
}

#[test]
fn env_scoping_keeps_unlisted_envs_off_disk() {
    let r = rig(Duration::from_secs(300), Some(vec![dev()]));
    let rf = rref("a/b");
    // prod is out of scope → every verb is a straight remote call
    r.vault.put(&prod(), &rf, Secret::new(FAKE)).unwrap();
    let got = r.vault.get(&prod(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), FAKE);
    r.remote.down(true);
    assert!(r.vault.get(&prod(), &rf).is_err());
    // …and no prod row ever landed in the cache file
    assert!(open_cache(&r).list(&prod()).unwrap().is_empty());
    // dev, in scope, does cache
    r.remote.down(false);
    r.vault.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    assert_eq!(open_cache(&r).list(&dev()).unwrap().len(), 1);
}

#[test]
fn corrupt_cache_row_is_a_miss_not_a_failure() {
    let r = rig(Duration::from_secs(300), None);
    let rf = rref("a/b");
    r.vault.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    // scribble non-JSON into the row's value slot
    let cache = open_cache(&r);
    cache
        .put(&dev(), &rf, Secret::new("{{{{ not json"))
        .unwrap();
    drop(cache);
    // parse failure → treated as a miss → remote fetch → rewrite
    let got = r.vault.get(&dev(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), FAKE);
    r.remote.down(true);
    // and the rewritten row serves again
    let got = r.vault.get(&dev(), &rf).unwrap().unwrap();
    assert_eq!(got.value.expose(), FAKE);
}

#[test]
fn no_cache_is_plain_passthrough() {
    let remote = MapVault::default();
    let v = CachedVault::new(
        Box::new(remote.clone()),
        None,
        None,
        CachePolicy::all(Duration::from_secs(300)),
    );
    let rf = rref("a/b");
    v.put(&dev(), &rf, Secret::new(FAKE)).unwrap();
    assert_eq!(v.get(&dev(), &rf).unwrap().unwrap().value.expose(), FAKE);
    remote.down(true);
    assert!(v.get(&dev(), &rf).is_err());
    assert!(v.list(&dev()).is_err());
}

#[test]
fn list_uses_the_marker_and_stays_offline_capable() {
    let r = rig(Duration::from_secs(300), None);
    r.vault
        .put(&dev(), &rref("a/one"), Secret::new(FAKE))
        .unwrap();
    r.vault
        .put(&dev(), &rref("b/two"), Secret::new(FAKE))
        .unwrap();
    assert_eq!(r.vault.list(&dev()).unwrap().len(), 2);
    // the sidecar holds only env names + timestamps — never refs
    let meta = std::fs::read_to_string(&r.meta_path).unwrap();
    assert!(meta.contains("dev") && !meta.contains("a/one"), "{meta}");
    // remote down → the marked listing serves from cache
    r.remote.down(true);
    let refs = r.vault.list(&dev()).unwrap();
    assert_eq!(refs, vec![rref("a/one"), rref("b/two")]);
}

#[test]
fn list_reconciles_remote_deletes() {
    // ttl=0 → the marker is instantly stale → every list is a real
    // remote fetch followed by a reconcile
    let r = rig(Duration::ZERO, None);
    r.vault
        .put(&dev(), &rref("a/one"), Secret::new(FAKE))
        .unwrap();
    r.vault
        .put(&dev(), &rref("b/two"), Secret::new(FAKE))
        .unwrap();
    r.vault.list(&dev()).unwrap();
    // b/two deleted remotely — the next remote list prunes the mirror
    r.remote.rm(&dev(), &rref("b/two")).unwrap();
    let refs = r.vault.list(&dev()).unwrap();
    assert_eq!(refs, vec![rref("a/one")]);
    r.remote.down(true);
    assert_eq!(r.vault.list(&dev()).unwrap(), vec![rref("a/one")]);
}

#[test]
fn list_without_a_marker_fails_with_the_remote() {
    let r = rig(Duration::from_secs(300), None);
    // rows may exist (gets populate them), but no env listing was
    // ever fetched — there is no complete snapshot to serve
    r.vault
        .put(&dev(), &rref("a/one"), Secret::new(FAKE))
        .unwrap();
    r.remote.down(true);
    assert!(r.vault.list(&dev()).is_err());
}

/// The cache vault never sees a plaintext meta value — the rows it
/// stores are JSON envelopes, but the *file* is ordinary age
/// ciphertext: nothing on disk leaks refs or values.
#[test]
fn cache_file_holds_no_plaintext() {
    let r = rig(Duration::from_secs(300), None);
    r.vault
        .put(&dev(), &rref("a/b"), Secret::new(FAKE))
        .unwrap();
    let bytes = std::fs::read(&r.cache_path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains(FAKE));
    assert!(!text.contains("a/b"));
}

#[test]
fn meta_file_parse_failures_are_no_marker() {
    let r = rig(Duration::from_secs(300), None);
    std::fs::write(&r.meta_path, "not json at all").unwrap();
    r.vault
        .put(&dev(), &rref("a/one"), Secret::new(FAKE))
        .unwrap();
    // unreadable marker + remote down → no cached list to serve
    r.remote.down(true);
    assert!(r.vault.list(&dev()).is_err());
}
