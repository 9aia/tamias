//! tamias-testkit — the vault conformance suite.
//!
//! Every [`tamias_vault::Vault`] impl must satisfy the same contract
//! (documented on the trait). [`vault_conformance!`] expands into
//! one `#[test]` per check, each given a fresh vault:
//!
//! ```ignore
//! tamias_testkit::vault_conformance!(|| {
//!     AgeVault::open(&vault_path, x25519::Identity::generate(), &[]).unwrap()
//! });
//! ```
//!
//! Values here are fake by rule — no real secret ever appears in a
//! test fixture.

#![allow(clippy::unwrap_used)]

pub mod httpmock;
pub mod mocks;

use tamias_core::{Env, Ref, Secret};
use tamias_vault::{Vault, VaultError};

/// Fake value used across the suite — obviously not a real secret.
pub const FAKE: &str = "fake-value-not-a-real-secret";

/// Generate the conformance `#[test]` fns. `$make` is a thunk
/// returning a fresh, empty vault.
#[macro_export]
macro_rules! vault_conformance {
    ($make:expr) => {
        #[test]
        fn conform_put_get_roundtrip() {
            let v = ($make)();
            $crate::put_get_roundtrip(&v);
        }

        #[test]
        fn conform_get_missing_is_none() {
            let v = ($make)();
            $crate::get_missing_is_none(&v);
        }

        #[test]
        fn conform_list_refs() {
            let v = ($make)();
            $crate::list_refs(&v);
        }

        #[test]
        fn conform_rm_semantics() {
            let v = ($make)();
            $crate::rm_semantics(&v);
        }

        #[test]
        fn conform_rotate_bumps() {
            let v = ($make)();
            $crate::rotate_bumps(&v);
        }

        #[test]
        fn conform_env_isolation() {
            let v = ($make)();
            $crate::env_isolation(&v);
        }

        #[test]
        fn conform_unicode_and_long_values() {
            let v = ($make)();
            $crate::unicode_and_long_values(&v);
        }

        #[test]
        fn conform_overwrite_semantics() {
            let v = ($make)();
            $crate::overwrite_semantics(&v);
        }
    };
}

fn env(name: &str) -> Env {
    Env::new(name).unwrap()
}

fn rref(path: &str) -> Ref {
    Ref::new(&format!("tamias://{path}")).unwrap()
}

fn paths(refs: &[Ref]) -> Vec<String> {
    refs.iter().map(|r| r.path()).collect()
}

/// put → get returns the value, version 1, timestamps set.
pub fn put_get_roundtrip(v: &dyn Vault) {
    let r = rref("billing/stripe");
    let rec = v.put(&env("dev"), &r, Secret::new(FAKE)).unwrap();
    assert_eq!(rec.value.expose(), FAKE);
    assert_eq!(rec.version, 1);
    assert!(rec.rotated_at.is_none());

    let got = v.get(&env("dev"), &r).unwrap().unwrap();
    assert_eq!(got.value.expose(), FAKE);
    assert_eq!(got.version, 1);
    assert_eq!(got.created_at, rec.created_at);
}

/// get on a missing ref is `Ok(None)`.
pub fn get_missing_is_none(v: &dyn Vault) {
    assert!(v.get(&env("dev"), &rref("nothing/here")).unwrap().is_none());
}

/// list returns the stored refs, sorted; empty env → empty list.
pub fn list_refs(v: &dyn Vault) {
    assert!(v.list(&env("dev")).unwrap().is_empty());
    v.put(&env("dev"), &rref("b/two"), Secret::new(FAKE))
        .unwrap();
    v.put(&env("dev"), &rref("a/one"), Secret::new(FAKE))
        .unwrap();
    v.put(&env("dev"), &rref("lone"), Secret::new(FAKE))
        .unwrap();
    assert_eq!(
        paths(&v.list(&env("dev")).unwrap()),
        vec!["a/one", "b/two", "lone"]
    );
}

/// rm → true once, get → None, rm again → false.
pub fn rm_semantics(v: &dyn Vault) {
    let r = rref("gone/soon");
    v.put(&env("dev"), &r, Secret::new(FAKE)).unwrap();
    assert!(v.rm(&env("dev"), &r).unwrap());
    assert!(v.get(&env("dev"), &r).unwrap().is_none());
    assert!(!v.rm(&env("dev"), &r).unwrap());
}

/// rotate on an existing ref bumps version, sets rotated_at, swaps
/// the value; on a missing ref it's `Err(NotFound)`.
pub fn rotate_bumps(v: &dyn Vault) {
    let r = rref("spin/this");
    let first = v.put(&env("dev"), &r, Secret::new(FAKE)).unwrap();
    let rotated = v
        .rotate(&env("dev"), &r, Secret::new("fake-rotated-value"))
        .unwrap();
    assert_eq!(rotated.value.expose(), "fake-rotated-value");
    assert_eq!(rotated.version, first.version + 1);
    assert!(rotated.rotated_at.is_some());
    assert_eq!(rotated.created_at, first.created_at);

    let err = v
        .rotate(&env("dev"), &rref("not/here"), Secret::new(FAKE))
        .expect_err("rotate of missing ref must fail");
    assert!(
        matches!(err, VaultError::NotFound { .. }),
        "rotate of missing ref must be NotFound, got {err:?}"
    );
}

/// Envs are isolated keyspaces — same path in dev vs prod are
/// independent records.
pub fn env_isolation(v: &dyn Vault) {
    let r = rref("shared/name");
    v.put(&env("dev"), &r, Secret::new("fake-dev-value"))
        .unwrap();

    assert!(v.get(&env("prod"), &r).unwrap().is_none());
    assert!(v.list(&env("prod")).unwrap().is_empty());
    assert!(!v.rm(&env("prod"), &r).unwrap());

    v.put(&env("prod"), &r, Secret::new("fake-prod-value"))
        .unwrap();
    assert_eq!(
        v.get(&env("dev"), &r).unwrap().unwrap().value.expose(),
        "fake-dev-value"
    );
    assert_eq!(
        v.get(&env("prod"), &r).unwrap().unwrap().value.expose(),
        "fake-prod-value"
    );

    assert!(v.rm(&env("prod"), &r).unwrap());
    assert!(v.get(&env("dev"), &r).unwrap().is_some());
}

/// Non-ASCII and large values round-trip intact.
pub fn unicode_and_long_values(v: &dyn Vault) {
    let uni = "🔐 fáke ünïcodé  — not real";
    let long = "fake-".repeat(4096); // ~20 KiB
    let r1 = rref("unicode/value");
    let r2 = rref("long/value");
    v.put(&env("dev"), &r1, Secret::new(uni)).unwrap();
    v.put(&env("dev"), &r2, Secret::new(long.clone())).unwrap();
    assert_eq!(
        v.get(&env("dev"), &r1).unwrap().unwrap().value.expose(),
        uni
    );
    assert_eq!(
        v.get(&env("dev"), &r2).unwrap().unwrap().value.expose(),
        long
    );
}

/// Re-put of an existing ref overwrites the value, bumps version,
/// preserves created_at, and does *not* mark a rotation.
pub fn overwrite_semantics(v: &dyn Vault) {
    let r = rref("over/written");
    let first = v.put(&env("dev"), &r, Secret::new(FAKE)).unwrap();
    let second = v
        .put(&env("dev"), &r, Secret::new("fake-overwritten"))
        .unwrap();
    assert_eq!(second.value.expose(), "fake-overwritten");
    assert_eq!(second.version, first.version + 1);
    assert_eq!(second.created_at, first.created_at);
    assert!(second.rotated_at.is_none());
}
