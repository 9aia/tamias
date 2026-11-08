#![allow(clippy::unwrap_used)]
//! Property tests — invariants that must hold for arbitrary inputs.

use std::collections::BTreeMap;
use std::fs;

use proptest::prelude::*;
use tamias_core::digest::payload_files;
use tamias_core::{
    content_digest, dir_digest, pack_tgz, untar_gz, Error, Lock, LockEntry, Manifest, StoreRecord,
};

/// A minimal package manifest.
fn manifest(name: &str) -> Manifest {
    Manifest {
        name: name.to_string(),
        version: Some("1.0.0".to_string()),
        ..Manifest::default()
    }
}

/// Distinct file names → bytes, deterministic order.
fn write_files(dir: &std::path::Path, files: &[(String, Vec<u8>)]) -> BTreeMap<String, Vec<u8>> {
    let mut seen = BTreeMap::new();
    for (name, bytes) in files {
        seen.insert(name.clone(), bytes.clone());
    }
    for (name, bytes) in &seen {
        fs::write(dir.join(name), bytes).unwrap();
    }
    seen
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// `save` → `load` round-trips any valid manifest.
    #[test]
    fn manifest_round_trips(name in "[A-Za-z0-9][A-Za-z0-9._-]{0,30}") {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest(&name);
        m.save(&dir.path().join("tamias.yaml")).unwrap();
        prop_assert_eq!(Manifest::load(dir.path()).unwrap(), m);
    }

    /// An arbitrary `tamias.yaml` never panics the parser.
    #[test]
    fn manifest_parse_never_panics(text in "\\PC*", depth in 0usize..3) {
        let dir = tempfile::tempdir().unwrap();
        for ext in ["yaml", "toml"] {
            fs::write(dir.path().join(format!("m{depth}.{ext}")), text.as_bytes()).unwrap();
        }
        let _ = Manifest::load_at(&dir.path().join("m0.yaml"));
        let _ = Manifest::load_at(&dir.path().join("m0.toml"));
        prop_assert!(true);
    }

    /// The lockfile round-trips, stays sorted, and upsert keeps it unique
    /// by (scope, name).
    #[test]
    fn lock_round_trips_and_sorts(entries in prop::collection::vec(("[a-z]{1,8}", any::<bool>()), 0..8)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tamias.lock");
        let mut lock = Lock::default();
        for (name, scoped) in &entries {
            lock.upsert(LockEntry::from_record(&StoreRecord {
                name: name.clone(),
                scope: scoped.then(|| "s".to_string()),
                version: None,
                kind: None,
                source_ref: "./x".into(),
                resolved: "r".into(),
                digest: "d".into(),
                installed_at: 0,
            }));
        }
        let keys: Vec<_> = lock
            .packages
            .iter()
            .map(|e| (e.scope.clone(), e.name.clone()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        prop_assert_eq!(&keys, &sorted);
        lock.save(&path).unwrap();
        prop_assert_eq!(Lock::load(&path).unwrap(), lock);
    }

    /// `content_digest` is deterministic, prefixed, and 64 hex chars.
    #[test]
    fn content_digest_is_stable(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let a = content_digest(&bytes);
        prop_assert_eq!(&a, &content_digest(&bytes));
        prop_assert!(a.starts_with("sha256:"));
        prop_assert_eq!(a.len(), "sha256:".len() + 64);
    }

    /// `dir_digest` is independent of file creation order.
    #[test]
    fn dir_digest_is_order_independent(
        files in prop::collection::vec(("[a-z]{1,6}\\.txt", prop::collection::vec(any::<u8>(), 1..64)), 1..5)
    ) {
        let d1 = tempfile::tempdir().unwrap();
        let seen = write_files(d1.path(), &files);
        let d2 = tempfile::tempdir().unwrap();
        for (name, bytes) in seen.iter().rev() {
            fs::write(d2.path().join(name), bytes).unwrap();
        }
        prop_assert_eq!(dir_digest(d1.path()).unwrap(), dir_digest(d2.path()).unwrap());
    }

    /// `pack_tgz` → `untar_gz` preserves the payload exactly.
    #[test]
    fn pack_untar_round_trips(
        files in prop::collection::vec(("[a-z]{1,6}\\.txt", prop::collection::vec(any::<u8>(), 1..64)), 1..4)
    ) {
        let src = tempfile::tempdir().unwrap();
        let seen = write_files(src.path(), &files);
        let out = tempfile::tempdir().unwrap();
        let tgz = out.path().join("x.tgz");
        pack_tgz(src.path(), &tgz).unwrap();
        let dest = tempfile::tempdir().unwrap();
        untar_gz(fs::File::open(&tgz).unwrap(), dest.path()).unwrap();
        prop_assert_eq!(
            payload_files(src.path()).unwrap(),
            payload_files(dest.path()).unwrap()
        );
        for (name, bytes) in &seen {
            prop_assert_eq!(&fs::read(dest.path().join(name)).unwrap(), bytes);
        }
    }

    /// Arbitrary bytes are never a valid gzip tarball — the unpacker errors,
    /// and never panics.
    #[test]
    fn untar_of_garbage_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let dest = tempfile::tempdir().unwrap();
        let r = untar_gz(bytes.as_slice(), dest.path());
        prop_assert!(r.is_err() || bytes.is_empty());
    }
}

/// Sanity: the error type is usable from tests (covers the re-export).
#[test]
fn error_is_constructible() {
    let e: Error = Error::Failed("x".into());
    assert!(e.to_string().contains('x'));
}
