//! [`CachedVault`] runs the same vault conformance suite every
//! impl passes — the composite is transparent: remote records come
//! back verbatim, writes are remote-first, and the cache never
//! changes the port's semantics.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use age::x25519;
use tamias_core::{Env, Ref, Secret};
use tamias_vault::{SecretRecord, Vault, VaultError};
use tamias_vault_age::AgeVault;
use tamias_vault_cached::{CachePolicy, CachedVault};
use tempfile::TempDir;

/// The fixture the macro's thunk returns — keeps the `TempDir`
/// alive for the test's span.
struct Fix {
    _dir: TempDir,
    inner: CachedVault,
}

impl Vault for Fix {
    fn get(&self, env: &Env, rref: &Ref) -> Result<Option<SecretRecord>, VaultError> {
        self.inner.get(env, rref)
    }
    fn put(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        self.inner.put(env, rref, value)
    }
    fn rm(&self, env: &Env, rref: &Ref) -> Result<bool, VaultError> {
        self.inner.rm(env, rref)
    }
    fn list(&self, env: &Env) -> Result<Vec<Ref>, VaultError> {
        self.inner.list(env)
    }
    fn rotate(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        self.inner.rotate(env, rref, value)
    }
}

tamias_testkit::vault_conformance!(|| {
    // remote: a real AgeVault (conformance never fails it); cache:
    // a second AgeVault; all envs cached, generous TTL.
    let dir = TempDir::new().unwrap();
    let id = x25519::Identity::generate();
    let remote = AgeVault::open(&dir.path().join("remote.age"), id.clone(), &[]).unwrap();
    let cache = AgeVault::open(&dir.path().join("cache.age"), id, &[]).unwrap();
    let meta = dir.path().join("cache.listed.json");
    Fix {
        _dir: dir,
        inner: CachedVault::new(
            Box::new(remote),
            Some(Box::new(cache)),
            Some(meta),
            CachePolicy::all(Duration::from_secs(3600)),
        ),
    }
});
