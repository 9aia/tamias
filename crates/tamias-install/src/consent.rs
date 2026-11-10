//! Per-package bundled-driver consent memory (proposal P-005 §D.3).
//!
//! `tamias_driver::drivers_allowed` (`--allow-drivers` /
//! `TAMIAS_ALLOW_DRIVERS=1`) is all-or-nothing per invocation. This
//! module persists consent **per package label + payload digest** so
//! re-installing identical bytes doesn't re-prompt, while a changed
//! payload (new `dir_digest`) refuses again.
//!
//! The record file is JSON at `$TAMIAS_CONSENT_FILE` →
//! `$XDG_CONFIG_HOME/tamias/consented.json` →
//! `~/.config/tamias/consented.json` (the same config-dir convention
//! the `allowed_signers` default uses), falling back to
//! `./.tamias/consented.json` when no home is known — beside the
//! project-local `tamias_home()` fallback. Format: a map keyed by the
//! store label (`@scope/name@version`, or the sha-short label when
//! unversioned — the [`crate::include::pending_label`] /
//! [`StoreEntry::label`](tamias_core::StoreEntry::label) convention)
//! to `{ "digest", "drivers", "consented_at" }`. `digest` is the
//! COMPOSED payload `dir_digest` computed in `install_inner`, so
//! consent binds to exact bytes; `drivers` is informational (the
//! digest already implies the file set) — kept for `doctor`/debug.
//!
//! Reads tolerate a missing or corrupt file (corrupt → warn, treated
//! as empty); writes go through [`tamias_core::write_atomic`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tamias_core::{write_atomic, Error};

/// Env override for the consent file — tests/isolation.
const CONSENT_FILE_ENV: &str = "TAMIAS_CONSENT_FILE";

/// The consent record filename under the config dir.
const FILE_NAME: &str = "consented.json";

/// One package's recorded consent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConsentRecord {
    /// The composed payload `dir_digest` consent was granted for —
    /// `sha256:<hex>`.
    pub digest: String,
    /// The bundled driver names consent covered (informational — the
    /// digest already pins the file set).
    pub drivers: Vec<String>,
    /// Unix seconds when consent was recorded.
    pub consented_at: u64,
}

/// label → record. The file's top level is this map verbatim.
type ConsentMap = HashMap<String, ConsentRecord>;

/// The consent file path — `$TAMIAS_CONSENT_FILE` →
/// `$XDG_CONFIG_HOME/tamias/consented.json` →
/// `~/.config/tamias/consented.json` → `./.tamias/consented.json`.
#[must_use]
pub(crate) fn consent_path() -> PathBuf {
    if let Some(p) = std::env::var_os(CONSENT_FILE_ENV) {
        return PathBuf::from(p);
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("tamias").join(FILE_NAME);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    home.map_or_else(
        || PathBuf::from("./.tamias").join(FILE_NAME),
        |h| h.join(".config/tamias").join(FILE_NAME),
    )
}

/// Load the consent map — missing file → empty; unreadable/corrupt →
/// warn + empty (a tampered file must not *grant* consent, and
/// [`record_consent_in`] overwrites it on the next successful install).
fn load(path: &Path) -> ConsentMap {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ConsentMap::new();
    };
    match serde_json::from_str(&text) {
        Ok(map) => map,
        Err(e) => {
            eprintln!(
                "warning: ignoring corrupt consent file {}: {e}",
                path.display()
            );
            ConsentMap::new()
        }
    }
}

/// [`consented_in`] against the resolved [`consent_path`].
#[must_use]
pub(crate) fn consented(label: &str, digest: &str) -> bool {
    consented_in(&consent_path(), label, digest)
}

/// Whether `label` has recorded consent for exactly `digest` in the
/// file at `path`.
///
/// Digest equality already implies the identical file set — including
/// `drivers/` — so no separate driver-set check is needed; the stored
/// `drivers` list is informational.
#[must_use]
fn consented_in(path: &Path, label: &str, digest: &str) -> bool {
    load(path).get(label).is_some_and(|r| r.digest == digest)
}

/// [`record_consent_in`] against the resolved [`consent_path`].
pub(crate) fn record_consent(label: &str, digest: &str, drivers: &[String]) -> Result<(), Error> {
    record_consent_in(&consent_path(), label, digest, drivers)
}

/// Record consent for `label` at `digest` covering `drivers` in the
/// file at `path`, merging into the existing records (other packages'
/// entries survive).
///
/// # Errors
/// Propagates serialization/write failures — callers on the install
/// path treat this as advisory (the install already committed).
fn record_consent_in(
    path: &Path,
    label: &str,
    digest: &str,
    drivers: &[String],
) -> Result<(), Error> {
    let mut map = load(path);
    map.insert(
        label.to_string(),
        ConsentRecord {
            digest: digest.to_string(),
            drivers: drivers.to_vec(),
            consented_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        },
    );
    let json = serde_json::to_string_pretty(&map)
        .map_err(|e| Error::Failed(format!("serialize consent file: {e}")))?;
    write_atomic(path, format!("{json}\n").as_bytes())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn consent_round_trip_and_digest_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consented.json");
        assert!(!consented_in(&path, "pkg@1.0.0", "sha256:aaa"));
        record_consent_in(
            &path,
            "pkg@1.0.0",
            "sha256:aaa",
            &["tamias-driver-x".to_string()],
        )
        .unwrap();
        assert!(consented_in(&path, "pkg@1.0.0", "sha256:aaa"));
        // a different digest (changed payload) does NOT match
        assert!(!consented_in(&path, "pkg@1.0.0", "sha256:bbb"));
        // a different label does NOT match
        assert!(!consented_in(&path, "other@1.0.0", "sha256:aaa"));
        // the file round-trips through serde and keeps other records
        record_consent_in(&path, "other@2.0.0", "sha256:ccc", &[]).unwrap();
        assert!(consented_in(&path, "other@2.0.0", "sha256:ccc"));
        assert!(consented_in(&path, "pkg@1.0.0", "sha256:aaa"));
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["pkg@1.0.0"]["drivers"][0], "tamias-driver-x");
        assert!(parsed["pkg@1.0.0"]["consented_at"].is_u64());
    }

    #[test]
    fn missing_and_corrupt_files_mean_no_consent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consented.json");
        // missing → empty
        assert!(!consented_in(&path, "pkg@1.0.0", "sha256:aaa"));
        // corrupt → treated as empty (and warns)
        std::fs::write(&path, "not json{{{").unwrap();
        assert!(!consented_in(&path, "pkg@1.0.0", "sha256:aaa"));
        // recording over a corrupt file recovers it
        record_consent_in(&path, "pkg@1.0.0", "sha256:aaa", &[]).unwrap();
        assert!(consented_in(&path, "pkg@1.0.0", "sha256:aaa"));
    }

    /// `consent_path` reads process env — serialize against the lib.rs
    /// env-mutating tests via the shared [`crate::tests::DRVENV`] lock.
    #[test]
    fn consent_path_precedence() {
        let _g = crate::tests::DRVENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(CONSENT_FILE_ENV, dir.path().join("explicit.json"));
        assert_eq!(
            consent_path(),
            dir.path().join("explicit.json"),
            "TAMIAS_CONSENT_FILE wins"
        );
        std::env::remove_var(CONSENT_FILE_ENV);
        std::env::set_var("XDG_CONFIG_HOME", dir.path().join("xdg"));
        assert_eq!(consent_path(), dir.path().join("xdg/tamias/consented.json"));
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", dir.path().join("home"));
        assert_eq!(
            consent_path(),
            dir.path().join("home/.config/tamias/consented.json")
        );
        std::env::remove_var("HOME");
        std::env::remove_var("USERPROFILE");
        assert_eq!(consent_path(), PathBuf::from("./.tamias/consented.json"));
    }
}
