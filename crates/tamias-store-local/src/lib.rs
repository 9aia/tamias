#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-store-local — the local store: unpacked packages under
//! `<root>/[@scope/]<name>@<label>/`, each carrying a
//! [`RECORD_FILE`](tamias_core::RECORD_FILE). `name` aliases the entry so
//! other tools resolve `@scope/name` refs.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tamias_core::{
    copy_tree, dir_digest, is_valid_name, is_valid_version, write_atomic, Error, StoreEntry,
    StoreRecord, RECORD_FILE,
};

/// `verify` output — recorded vs recomputed payload digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    /// `@scope/name@label` of the checked entry.
    pub name: String,
    /// Digest the store record carries.
    pub recorded: String,
    /// Digest recomputed over the store dir.
    pub computed: String,
    /// Whether they match.
    pub ok: bool,
}

/// The local store — one per root.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// A store under `root` (dirs created lazily on first install).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root dir.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every entry — dirs carrying a record, sorted by label. A single
    /// unparsable record file is skipped with a warning (a torn write
    /// or hand-edit shouldn't blind `list`/`info`/`verify`/`remove`);
    /// genuine I/O failures still propagate.
    ///
    /// # Errors
    /// Propagates read failures; parse failures are warnings.
    pub fn entries(&self) -> Result<Vec<StoreEntry>, Error> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        collect(&self.root, &mut out)?;
        out.sort_by_key(StoreEntry::label);
        Ok(out)
    }

    /// Find by exact `@scope/name@label`, else by bare name (erroring when
    /// several installs share it).
    ///
    /// # Errors
    /// Fails when not installed or ambiguous.
    pub fn find(&self, name_or_label: &str) -> Result<StoreEntry, Error> {
        let entries = self.entries()?;
        if let Some(found) = entries.iter().find(|e| e.label() == name_or_label) {
            return Ok(found.clone());
        }
        let mut matches: Vec<StoreEntry> = entries
            .into_iter()
            .filter(|e| e.record.name == name_or_label)
            .collect();
        match matches.len() {
            0 => Err(Error::Failed(format!(
                "package '{name_or_label}' is not installed"
            ))),
            1 => Ok(matches.swap_remove(0)),
            _ => {
                let labels: Vec<String> = matches.iter().map(StoreEntry::label).collect();
                Err(Error::Failed(format!(
                    "package '{name_or_label}' has several installs — pick one: {}",
                    labels.join(", ")
                )))
            }
        }
    }

    /// Other installed entries sharing `entry`'s bare package name —
    /// the "siblings remain" check for version-aware uninstall
    /// (T-029). Drivers key their shared operator state
    /// (`~/.config/<product>/…`) on `--name` alone — scope, kind, and
    /// version never reach the driver — so `uninstall` teardown must
    /// wait until the last same-name entry is gone. Matching is on
    /// `record.name` only: any other entry that would dispatch to a
    /// driver under the same `--name` keeps that shared state alive.
    ///
    /// # Errors
    /// Propagates [`Store::entries`] failures.
    pub fn siblings_of(&self, entry: &StoreEntry) -> Result<Vec<StoreEntry>, Error> {
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| e.dir != entry.dir && e.record.name == entry.record.name)
            .collect())
    }

    /// Unpack `payload` into the entry dir, replacing an existing one
    /// (re-install is the upgrade path). The payload is staged into a
    /// sibling `.tmp-*` dir — copy, digest, and record write all happen
    /// there — then swapped into place: the old entry moves aside to
    /// `.old-*` only after the new one is complete, so a crash never
    /// leaves a partial payload at the entry dir.
    ///
    /// # Errors
    /// Propagates copy/write/rename failures; a failed install leaves
    /// any previous entry untouched and cleans its staging dir.
    pub fn install(&self, payload: &Path, mut record: StoreRecord) -> Result<StoreEntry, Error> {
        let dir = self.entry_dir(&record)?;
        let parent = dir.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(Error::io(format!("create {}", parent.display())))?;
        // Clear litter from crashed installs/removes before staging.
        sweep_staging(parent);
        let stage = staging_dir(&dir);
        if stage.exists() {
            fs::remove_dir_all(&stage).map_err(Error::io(format!("clear {}", stage.display())))?;
        }
        fs::create_dir_all(&stage).map_err(Error::io(format!("create {}", stage.display())))?;
        let staged = (|| -> Result<(), Error> {
            copy_tree(payload, &stage)?;
            record.digest = dir_digest(&stage)?;
            record.installed_at = unix_now();
            let json = serde_json::to_string_pretty(&record)
                .map_err(|e| Error::Failed(format!("serialize record: {e}")))?;
            write_atomic(&stage.join(RECORD_FILE), json.as_bytes())?;
            swap_into_place(&stage, &dir)
        })();
        if staged.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        staged?;
        Ok(StoreEntry { dir, record })
    }

    /// Drop a store entry. The dir is moved aside to `.old-*` before
    /// deletion so a crash mid-delete leaves sweepable litter, never a
    /// half-deleted dir that `collect` would mistake for a scope dir.
    ///
    /// # Errors
    /// Fails when not installed or on I/O error.
    pub fn remove(&self, name_or_label: &str) -> Result<StoreRecord, Error> {
        let entry = self.find(name_or_label)?;
        let old = old_dir(&entry.dir);
        if old.exists() {
            fs::remove_dir_all(&old).map_err(Error::io(format!("clear {}", old.display())))?;
        }
        fs::rename(&entry.dir, &old)
            .map_err(Error::io(format!("move aside {}", entry.dir.display())))?;
        fs::remove_dir_all(&old).map_err(Error::io(format!("remove {}", old.display())))?;
        Ok(entry.record)
    }

    /// Recompute the entry's payload digest against the recorded one.
    ///
    /// # Errors
    /// Fails when not installed or on I/O error.
    pub fn verify(&self, name_or_label: &str) -> Result<VerifyReport, Error> {
        let entry = self.find(name_or_label)?;
        let computed = dir_digest(&entry.dir)?;
        Ok(VerifyReport {
            name: entry.label(),
            recorded: entry.record.digest.clone(),
            computed: computed.clone(),
            ok: computed == entry.record.digest,
        })
    }

    /// Retarget an install's recorded source ref (payload untouched).
    ///
    /// # Errors
    /// Fails when not installed or on I/O error.
    pub fn retarget(&self, name_or_label: &str, new_ref: &str) -> Result<StoreRecord, Error> {
        let entry = self.find(name_or_label)?;
        let mut record = entry.record.clone();
        record.source_ref = new_ref.to_string();
        let record_path = entry.dir.join(RECORD_FILE);
        let json = serde_json::to_string_pretty(&record)
            .map_err(|e| Error::Failed(format!("serialize record: {e}")))?;
        write_atomic(&record_path, json.as_bytes())?;
        Ok(record)
    }

    /// The entry dir — refuses records whose name/scope/version label
    /// aren't single safe path components (records can be hand-edited).
    ///
    /// # Errors
    /// Fails on an unsafe name, scope, or version label.
    fn entry_dir(&self, record: &StoreRecord) -> Result<PathBuf, Error> {
        if !record.name.is_empty() && !is_valid_name(&record.name) {
            return Err(Error::Failed(format!(
                "unsafe package name '{}' in store record",
                record.name
            )));
        }
        if let Some(scope) = &record.scope {
            if !is_valid_name(scope) {
                return Err(Error::Failed(format!(
                    "unsafe scope '{scope}' in store record"
                )));
            }
        }
        let label = version_label(record);
        if !is_valid_version(&label) {
            return Err(Error::Failed(format!(
                "unsafe version label '{label}' in store record"
            )));
        }
        let base = match &record.scope {
            Some(scope) => self.root.join(format!("@{scope}")),
            None => self.root.clone(),
        };
        Ok(base.join(format!("{}@{}", record.name, label)))
    }
}

/// `version` when declared, else the resolved sha's short form.
fn version_label(record: &StoreRecord) -> String {
    record.version.clone().unwrap_or_else(|| {
        let sha = record
            .resolved
            .strip_prefix("sha256:")
            .unwrap_or(&record.resolved);
        sha.chars().take(12).collect()
    })
}

/// Sibling staging dir for an install — `.tmp-<label>-<pid>` inside the
/// target's parent (same filesystem → `rename(2)` is atomic). `collect`
/// skips dot-dirs, so a staged payload is invisible until swapped in.
fn staging_dir(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let label = target
        .file_name()
        .map_or_else(|| "pkg".to_string(), |n| n.to_string_lossy().into_owned());
    target.with_file_name(format!(".tmp-{label}-{pid}"))
}

/// Sibling holding dir for a replaced/removed entry — `.old-<label>-<pid>`.
fn old_dir(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let label = target
        .file_name()
        .map_or_else(|| "pkg".to_string(), |n| n.to_string_lossy().into_owned());
    target.with_file_name(format!(".old-{label}-{pid}"))
}

/// Move `stage` onto `target`, keeping the old entry available until
/// the new one is fully in place: target → `.old-*` sibling, stage →
/// target, then drop `.old-*`. POSIX has no atomic rename-dir-over-dir,
/// but every crash window leaves either the old entry, the complete new
/// entry, or sweepable litter — never a partial payload at `target`.
fn swap_into_place(stage: &Path, target: &Path) -> Result<(), Error> {
    if !target.exists() {
        return fs::rename(stage, target).map_err(Error::io(format!(
            "rename {} → {}",
            stage.display(),
            target.display()
        )));
    }
    let old = old_dir(target);
    if old.exists() {
        fs::remove_dir_all(&old).map_err(Error::io(format!("clear {}", old.display())))?;
    }
    fs::rename(target, &old).map_err(Error::io(format!(
        "move aside {} → {}",
        target.display(),
        old.display()
    )))?;
    if let Err(e) = fs::rename(stage, target) {
        // Put the old entry back — the install failed, the previous
        // version must still be there.
        let _ = fs::rename(&old, target);
        return Err(Error::io(format!(
            "install {} → {}",
            stage.display(),
            target.display()
        ))(e));
    }
    // Best-effort: a leftover `.old-*` is hidden from `collect` and
    // swept by the next install/remove in this dir.
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

/// Remove `.tmp-*`/`.old-*` staging dirs left by crashed installs or
/// removes. Best-effort — never fails the caller.
fn sweep_staging(parent: &Path) {
    let Ok(read) = fs::read_dir(parent) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && (n.starts_with(".tmp-") || n.starts_with(".old-")) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Recursively collect dirs that carry a record (scope dirs recurse).
/// Dot-dirs (`.tmp-*`/`.old-*` staging, `.git`-style hidden dirs) are
/// never entries. An unparsable record file is skipped with a warning —
/// same tolerance as `tamias-driver-tamias`'s `scan_store`.
fn collect(dir: &Path, out: &mut Vec<StoreEntry>) -> Result<(), Error> {
    let read = fs::read_dir(dir).map_err(Error::io(format!("read {}", dir.display())))?;
    for entry in read {
        let entry = entry.map_err(Error::io("read store entry"))?;
        let path = entry.path();
        // `file_type` never follows symlinks — a linked dir isn't an
        // entry (and can't recurse the walk).
        let is_dir = entry
            .file_type()
            .map_err(Error::io(format!("stat {}", path.display())))?
            .is_dir();
        if !is_dir {
            continue;
        }
        // Staging/sweep dirs and any hidden dir are never entries.
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let record_path = path.join(RECORD_FILE);
        // no-follow: a linked record file isn't trusted provenance
        let is_file = fs::symlink_metadata(&record_path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        if is_file {
            let text = fs::read_to_string(&record_path)
                .map_err(Error::io(format!("read {}", record_path.display())))?;
            match serde_json::from_str(&text) {
                Ok(record) => out.push(StoreEntry { dir: path, record }),
                Err(e) => eprintln!(
                    "tamias store: skipping unparsable record {}: {e}",
                    record_path.display()
                ),
            }
        } else {
            collect(&path, out)?;
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn record(name: &str, scope: Option<&str>) -> StoreRecord {
        StoreRecord {
            name: name.into(),
            scope: scope.map(str::to_string),
            version: Some("1.0.0".into()),
            kind: None,
            source_ref: "./x".into(),
            resolved: "sha256:abc".into(),
            digest: String::new(),
            signature: None,
            signer: None,
            trust: None,
            installed_at: 0,
        }
    }

    fn payload(dir: &std::path::Path) {
        std::fs::write(dir.join("a.txt"), "a").unwrap();
    }

    #[test]
    fn install_find_verify_remove_and_ambiguity() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        assert!(store.entries().unwrap().is_empty());
        let p = tempfile::tempdir().unwrap();
        payload(p.path());
        store.install(p.path(), record("w", None)).unwrap();
        assert_eq!(store.entries().unwrap().len(), 1);
        assert!(store.verify("w").unwrap().ok);
        let p2 = tempfile::tempdir().unwrap();
        payload(p2.path());
        store.install(p2.path(), record("w", Some("acme"))).unwrap();
        assert!(store.find("w").is_err());
        assert!(store.find("@acme/w@1.0.0").is_ok());
        assert!(store.remove("@acme/w@1.0.0").is_ok());
        assert_eq!(store.entries().unwrap().len(), 1);
    }

    #[test]
    fn siblings_of_matches_bare_name_across_versions_and_scopes() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let p = tempfile::tempdir().unwrap();
        payload(p.path());
        let mut v1 = record("w", None);
        v1.version = Some("1.0.0".into());
        let mut v2 = record("w", None);
        v2.version = Some("2.0.0".into());
        let e1 = store.install(p.path(), v1).unwrap();
        store.install(p.path(), v2).unwrap();
        store.install(p.path(), record("other", None)).unwrap();
        // a scoped install of the same bare name shares the driver's
        // `--name` namespace too
        let mut scoped = record("w", Some("acme"));
        scoped.version = Some("3.0.0".into());
        store.install(p.path(), scoped).unwrap();
        let sibs = store.siblings_of(&e1).unwrap();
        let labels: Vec<String> = sibs.iter().map(StoreEntry::label).collect();
        assert_eq!(labels, ["@acme/w@3.0.0", "w@2.0.0"]);
        // every other same-name entry is a sibling, whichever version
        // is being removed
        let last = store.find("w@2.0.0").unwrap();
        let sibs = store.siblings_of(&last).unwrap();
        assert_eq!(sibs.len(), 2);
        store.remove("w@2.0.0").unwrap();
        store.remove("@acme/w@3.0.0").unwrap();
        assert!(store.siblings_of(&e1).unwrap().is_empty());
    }

    #[test]
    fn verify_mismatch_and_missing() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let p = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("a.txt"), "a").unwrap();
        store.install(p.path(), record("w", None)).unwrap();
        std::fs::write(root.path().join("w@1.0.0/a.txt"), "b").unwrap();
        assert!(!store.verify("w").unwrap().ok);
        assert!(store.verify("ghost").is_err());
        assert!(store.remove("ghost").is_err());
    }

    #[test]
    fn scope_recursion_replace_and_manifest() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let p = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("a.txt"), "a").unwrap();
        let entry = store.install(p.path(), record("w", Some("acme"))).unwrap();
        assert_eq!(entry.label(), "@acme/w@1.0.0");
        assert!(entry.size().is_ok());
        assert!(entry.manifest().is_err());
        let entry2 = store.install(p.path(), record("w", Some("acme"))).unwrap();
        assert_eq!(entry2.label(), "@acme/w@1.0.0");
        assert_eq!(store.entries().unwrap().len(), 1);
        assert!(store.find("@acme/w@1.0.0").is_ok());
    }

    #[test]
    fn root_accessor_and_sha_label() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        assert_eq!(store.root(), root.path());
        let p = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("a.txt"), "a").unwrap();
        let mut rec = record("w", None);
        rec.version = None;
        rec.resolved = "sha256:deadbeefcafe0000".into();
        let entry = store.install(p.path(), rec).unwrap();
        assert_eq!(entry.label(), "w@deadbeefcafe");
    }

    #[test]
    fn install_rejects_unsafe_record_fields() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let p = tempfile::tempdir().unwrap();
        payload(p.path());
        // crafted version label must not escape the store root
        let mut rec = record("w", None);
        rec.version = Some("../../evil".into());
        assert!(store.install(p.path(), rec).is_err());
        // unsafe name / scope are refused too
        let rec = record("../w", None);
        assert!(store.install(p.path(), rec).is_err());
        let rec = record("w", Some("../s"));
        assert!(store.install(p.path(), rec).is_err());
        // nothing was written — not even inside the root
        assert!(fs::read_dir(root.path())
            .map(|mut d| d.next().is_none())
            .unwrap_or(true));
    }

    #[test]
    fn collect_skips_non_dirs() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        std::fs::write(root.path().join("stray.txt"), "x").unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("a.txt"), "a").unwrap();
        store.install(p.path(), record("w", None)).unwrap();
        assert_eq!(store.entries().unwrap().len(), 1);
    }

    #[test]
    fn corrupt_record_is_skipped_not_fatal() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let p = tempfile::tempdir().unwrap();
        payload(p.path());
        store.install(p.path(), record("w", None)).unwrap();
        // a torn/hand-edited record elsewhere must not blind entries()
        let bad = root.path().join("broken@1.0.0");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(RECORD_FILE), "{ not json").unwrap();
        let entries = store.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label(), "w@1.0.0");
        // the rest of the API keeps working too
        assert!(store.verify("w").unwrap().ok);
        assert!(store.remove("w").is_ok());
        assert!(store.entries().unwrap().is_empty());
    }

    #[test]
    fn remove_leaves_no_litter() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let p = tempfile::tempdir().unwrap();
        payload(p.path());
        store.install(p.path(), record("w", None)).unwrap();
        store.remove("w").unwrap();
        let names: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn failed_reinstall_keeps_old_entry_and_cleans_staging() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let good = tempfile::tempdir().unwrap();
        payload(good.path());
        store.install(good.path(), record("w", None)).unwrap();
        // second payload has an unreadable file → copy_tree fails mid-stage
        let bad = tempfile::tempdir().unwrap();
        let locked = bad.path().join("a.txt");
        std::fs::write(&locked, "a").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(store.install(bad.path(), record("w", None)).is_err());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        // old entry intact and still verifies; no staging litter
        assert!(store.verify("w").unwrap().ok);
        let names: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");
        assert!(names.contains(&"w@1.0.0".to_string()), "{names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn install_skips_symlinked_payload_entries() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"loot").unwrap();
        let p = tempfile::tempdir().unwrap();
        payload(p.path());
        std::os::unix::fs::symlink(outside.path(), p.path().join("loot")).unwrap();
        std::os::unix::fs::symlink(".", p.path().join("self")).unwrap();
        let entry = store.install(p.path(), record("w", None)).unwrap();
        // install + digest complete; nothing linked crossed into the store
        assert!(store.verify("w").unwrap().ok);
        assert!(entry.dir.join("a.txt").is_file());
        assert!(!entry.dir.join("loot").exists());
        assert!(!entry.dir.join("self").exists());
        assert!(!entry.dir.join("secret.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn io_error_arms() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::new(&root);
        let p = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("a.txt"), "a").unwrap();
        store.install(p.path(), record("w", None)).unwrap();
        // unreadable root → entries()/find() read fail
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(store.entries().is_err());
        assert!(store.find("w").is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        // read-only root → install / remove fail
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        let p2 = tempfile::tempdir().unwrap();
        std::fs::write(p2.path().join("a.txt"), "a").unwrap();
        assert!(store.install(p2.path(), record("z", None)).is_err());
        assert!(store.remove("w").is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
