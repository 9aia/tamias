//! `sync` — reconcile a project manifest's `deps:` against the install
//! store + `tamias.lock` (PL-0006 / P-003): the missing "install a
//! fresh clone's dep set" verb. For each `deps:` ref the shared
//! [`install_with_progress`] pipeline runs only when the store entry is
//! missing or its record drifted from the lock entry
//! (`source_ref`/`resolved`/`digest`); a matching pair is reported
//! `current` without re-resolving. Every installed/reinstalled dep
//! refreshes its `tamias.lock` entry via [`Lock::upsert_record`] — the
//! same tail `add` uses.
//!
//! The store is global, not project-scoped: sync **never removes**
//! entries — extraneous packages stay `tamias remove`'s job.
//!
//! `--locked` (`[`SyncOptions::locked`]`) is the strict mode: a dep is
//! `current` only when its lock entry exists *and* the store record
//! matches it; anything else is a per-dep `failed` outcome and nothing
//! is installed or re-resolved — the lockfile must already describe
//! the world.

use std::path::Path;

use tamias_core::{Error, Lock, LockEntry, Manifest, StoreEntry, StoreRecord};
use tamias_store_local::Store;

use crate::{install_with_progress, Env, Progress, Surface};

/// How one `deps:` ref came out of [`sync_project`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepStatus {
    /// Store entry exists and its record matches the lock entry —
    /// nothing was fetched.
    Current,
    /// No prior store entry — installed fresh.
    Installed,
    /// A store entry existed but drifted from the lock (or the lock
    /// had no entry) — re-installed and the lock refreshed.
    Reinstalled,
    /// The dep failed (or `--locked` refused the drift). Sync
    /// continues with the remaining deps; the report carries the
    /// error text in `detail`.
    Failed,
}

impl DepStatus {
    /// The wire/JSON word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Installed => "installed",
            Self::Reinstalled => "reinstalled",
            Self::Failed => "failed",
        }
    }
}

/// One dep's outcome — `dep` is the manifest ref verbatim, `detail`
/// the installed label (`@scope/name@version`) or the error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepOutcome {
    /// The `deps:` ref.
    pub dep: String,
    /// What happened to it.
    pub status: DepStatus,
    /// Label on success, error text on failure.
    pub detail: String,
}

/// The whole sync's per-dep results, in manifest order.
#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    /// One outcome per `deps:` entry.
    pub outcomes: Vec<DepOutcome>,
}

impl SyncReport {
    /// How many deps failed.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| o.status == DepStatus::Failed)
            .count()
    }

    /// Whether every dep is `current`/`installed`/`reinstalled`.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failed() == 0
    }
}

/// Sync knobs — the `--locked`/`--allow-drivers` pair every surface
/// exposes (the TUI has no `--locked` UI; it always passes `false`).
#[derive(Clone, Copy, Debug, Default)]
pub struct SyncOptions {
    /// Consent to register + run payload-bundled `drivers/` —
    /// forwarded to [`install_with_progress`] per dep; a refusal
    /// aborts the whole sync as an error (the surfaces' consent
    /// modals rely on it).
    pub allow_drivers: bool,
    /// Strict mode — deps must already be `current` per `tamias.lock`
    /// and the store; drift or a missing lock entry fails the dep
    /// instead of installing/moving it.
    pub locked: bool,
}

/// The lock entry pinned for `dep` — matched on `source_ref`, the
/// field `add`/`update`/`pin` keep equal to the manifest ref.
fn lock_entry_for<'a>(lock: &'a Lock, dep: &str) -> Option<&'a LockEntry> {
    lock.packages.iter().find(|e| e.source_ref == dep)
}

/// The store entry for a lock entry — by (scope, name); the store
/// keys entries by label so a bare-name lookup would be ambiguous.
fn store_match<'a>(entries: &'a [StoreEntry], lock: &LockEntry) -> Option<&'a StoreEntry> {
    entries.iter().find(|e| {
        e.record.name == lock.name
            && e.record.scope == lock.scope
            && e.record.version == lock.version
    })
}

/// Whether `record` agrees with the lock entry on everything the
/// lock pins — name/scope plus the provenance triple.
fn record_matches(record: &StoreRecord, lock: &LockEntry) -> bool {
    record.name == lock.name
        && record.scope == lock.scope
        && record.source_ref == lock.source_ref
        && record.resolved == lock.resolved
        && record.digest == lock.digest
}

/// Sync the project manifest in `dir` (`tamias.yaml`/`.yml`/`.toml`
/// via [`Manifest::path_in`]) against `dir/tamias.lock` and the store
/// under `env.store_root`. Progress streams through `report` exactly
/// like [`install_with_progress`]; `surface` tunes consent hints.
///
/// Per-dep install failures are collected into the report (sync keeps
/// going); a bundled-driver consent refusal, a manifest/lock I/O
/// failure, or a lock-write failure aborts with `Err`.
///
/// # Errors
/// Fails when no project manifest exists in `dir`, the manifest or
/// lock can't be read, a dep's install hits a consent refusal, or a
/// lock write fails.
pub fn sync_project(
    env: &Env,
    dir: &Path,
    surface: Surface,
    options: SyncOptions,
    report: &mut dyn FnMut(Progress),
) -> Result<SyncReport, Error> {
    let Some(manifest_path) = Manifest::path_in(dir) else {
        return Err(Error::Failed(format!(
            "no tamias.yaml/.yml/.toml in {} — sync needs a project manifest",
            dir.display()
        )));
    };
    let project = Manifest::load_project_at(&manifest_path)?;
    let lock_path = dir.join("tamias.lock");
    let mut lock = Lock::load(&lock_path)?;
    let store = Store::new(&env.store_root);
    let entries = store.entries()?;

    let mut outcomes = Vec::with_capacity(project.deps.len());
    for dep in &project.deps {
        let locked_entry = lock_entry_for(&lock, dep);
        let store_entry = locked_entry.and_then(|l| store_match(&entries, l));
        let current = match (locked_entry, store_entry) {
            (Some(l), Some(e)) => record_matches(&e.record, l),
            _ => false,
        };
        if current {
            outcomes.push(DepOutcome {
                dep: dep.clone(),
                status: DepStatus::Current,
                detail: store_entry
                    .map(StoreEntry::label)
                    .unwrap_or_else(|| dep.clone()),
            });
            continue;
        }
        if options.locked {
            let detail = if locked_entry.is_none() {
                "no tamias.lock entry (--locked)".to_string()
            } else if store_entry.is_none() {
                "not installed (--locked)".to_string()
            } else {
                "store record drifted from tamias.lock (--locked)".to_string()
            };
            outcomes.push(DepOutcome {
                dep: dep.clone(),
                status: DepStatus::Failed,
                detail,
            });
            continue;
        }
        // Did a store entry for this package exist before the install?
        // Resolved post-install by (scope, name) against the snapshot —
        // `installed` vs `reinstalled` is provenance, not fetch count.
        match install_with_progress(env, dep, options.allow_drivers, surface, report) {
            Ok(entry) => {
                let reinstalled = entries.iter().any(|e| {
                    e.record.name == entry.record.name && e.record.scope == entry.record.scope
                });
                Lock::upsert_record(&lock_path, &entry.record)?;
                lock.upsert(LockEntry::from_record(&entry.record));
                outcomes.push(DepOutcome {
                    dep: dep.clone(),
                    status: if reinstalled {
                        DepStatus::Reinstalled
                    } else {
                        DepStatus::Installed
                    },
                    detail: entry.label(),
                });
            }
            // Consent refusals propagate — the surfaces' consent modals
            // key off `is_driver_consent_refusal` on the returned Err.
            Err(e) if crate::is_driver_consent_refusal(&e) => return Err(e),
            Err(e) => outcomes.push(DepOutcome {
                dep: dep.clone(),
                status: DepStatus::Failed,
                detail: e.to_string(),
            }),
        }
    }
    Ok(SyncReport { outcomes })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use super::*;

    /// A package fixture — `tamias.yaml` + one payload file.
    fn pkg(dir: &Path, name: &str) -> std::path::PathBuf {
        let pkg = dir.join(name);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("tamias.yaml"),
            format!("name: {name}\nversion: 1.0.0\n"),
        )
        .unwrap();
        std::fs::write(pkg.join("a.txt"), "a").unwrap();
        pkg
    }

    /// A project dir with `deps:` refs (absolute fixture paths).
    fn project(dir: &Path, deps: &[String]) -> PathBuf {
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let deps_yaml = deps
            .iter()
            .map(|d| format!("  - {d}\n"))
            .collect::<String>();
        std::fs::write(proj.join("tamias.yaml"), format!("deps:\n{deps_yaml}")).unwrap();
        proj
    }

    fn env(root: &Path) -> Env {
        Env {
            store_root: root.join("store"),
            cache_root: root.join("cache"),
            max_download: None,
            signing: tamias_sign::SigningPolicy::default(),
        }
    }

    fn opts() -> SyncOptions {
        SyncOptions::default()
    }

    #[test]
    fn sync_installs_then_reports_current() {
        let tmp = tempfile::tempdir().unwrap();
        let p = pkg(tmp.path(), "w");
        let proj = project(tmp.path(), &[p.display().to_string()]);
        let env = env(tmp.path());
        let mut events = Vec::new();
        let report =
            sync_project(&env, &proj, Surface::Cli, opts(), &mut |pr| events.push(pr)).unwrap();
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].status, DepStatus::Installed);
        assert!(report.outcomes[0].detail.contains("w"));
        assert!(matches!(events[0], Progress::Fetching { .. }));
        // the lock was created with the installed record
        let lock = Lock::load(&proj.join("tamias.lock")).unwrap();
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "w");
        // second sync → current, no fetch events
        let mut events2 = Vec::new();
        let report2 = sync_project(&env, &proj, Surface::Cli, opts(), &mut |pr| {
            events2.push(pr)
        })
        .unwrap();
        assert_eq!(report2.outcomes[0].status, DepStatus::Current);
        assert!(events2.is_empty());
        assert!(report2.is_ok());
    }

    #[test]
    fn sync_reinstalls_on_lock_drift_and_aggregates_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let p = pkg(tmp.path(), "w");
        let ghost = tmp.path().join("no-such-pkg");
        let proj = project(
            tmp.path(),
            &[p.display().to_string(), ghost.display().to_string()],
        );
        let env = env(tmp.path());
        let report = sync_project(&env, &proj, Surface::Cli, opts(), &mut |_| {}).unwrap();
        assert_eq!(report.outcomes[0].status, DepStatus::Installed);
        assert_eq!(report.outcomes[1].status, DepStatus::Failed);
        assert_eq!(report.failed(), 1);
        // corrupt the lock digest → next sync reinstalls the good dep
        // and still reports the ghost's failure
        let lock_path = proj.join("tamias.lock");
        let mut lock = Lock::load(&lock_path).unwrap();
        lock.packages[0].digest = "sha256:stale".into();
        lock.save(&lock_path).unwrap();
        let report2 = sync_project(&env, &proj, Surface::Cli, opts(), &mut |_| {}).unwrap();
        assert_eq!(report2.outcomes[0].status, DepStatus::Reinstalled);
        assert_eq!(report2.outcomes[1].status, DepStatus::Failed);
        // the lock was repaired by the reinstall
        let lock = Lock::load(&lock_path).unwrap();
        assert_ne!(lock.packages[0].digest, "sha256:stale");
    }

    #[test]
    fn sync_without_lock_entry_reinstalls() {
        let tmp = tempfile::tempdir().unwrap();
        let p = pkg(tmp.path(), "w");
        let dep = p.display().to_string();
        // install once outside sync, project manifest deps on it, no lock
        let env = env(tmp.path());
        install_with_progress(&env, &dep, false, Surface::Cli, &mut |_| {}).unwrap();
        let proj = project(tmp.path(), std::slice::from_ref(&dep));
        let report = sync_project(&env, &proj, Surface::Cli, opts(), &mut |_| {}).unwrap();
        assert_eq!(report.outcomes[0].status, DepStatus::Reinstalled);
        assert!(proj.join("tamias.lock").is_file());
    }

    #[test]
    fn sync_locked_refuses_drift_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let p = pkg(tmp.path(), "w");
        let dep = p.display().to_string();
        let proj = project(tmp.path(), std::slice::from_ref(&dep));
        let env = env(tmp.path());
        let locked = SyncOptions {
            allow_drivers: false,
            locked: true,
        };
        // nothing installed, no lock → failed, nothing fetched
        let report = sync_project(&env, &proj, Surface::Cli, locked, &mut |_| {}).unwrap();
        assert_eq!(report.outcomes[0].status, DepStatus::Failed);
        assert!(report.outcomes[0].detail.contains("--locked"));
        assert!(Store::new(&env.store_root).entries().unwrap().is_empty());
        // populate via a normal sync → locked sync now reports current
        sync_project(&env, &proj, Surface::Cli, opts(), &mut |_| {}).unwrap();
        let report2 = sync_project(&env, &proj, Surface::Cli, locked, &mut |_| {}).unwrap();
        assert_eq!(report2.outcomes[0].status, DepStatus::Current);
        // drift the lock → locked refuses instead of moving
        let lock_path = proj.join("tamias.lock");
        let mut lock = Lock::load(&lock_path).unwrap();
        lock.packages[0].resolved = "sha256:moved".into();
        lock.save(&lock_path).unwrap();
        let report3 = sync_project(&env, &proj, Surface::Cli, locked, &mut |_| {}).unwrap();
        assert_eq!(report3.outcomes[0].status, DepStatus::Failed);
        assert!(report3.outcomes[0].detail.contains("drifted"));
        // and the store record was untouched
        let entry = Store::new(&env.store_root).find("w").unwrap();
        assert_ne!(entry.record.resolved, "sha256:moved");
    }

    #[test]
    fn sync_missing_manifest_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        assert!(sync_project(&env, tmp.path(), Surface::Cli, opts(), &mut |_| {}).is_err());
    }

    #[test]
    fn sync_empty_deps_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project(tmp.path(), &[]);
        let env = env(tmp.path());
        let report = sync_project(&env, &proj, Surface::Cli, opts(), &mut |_| {}).unwrap();
        assert!(report.outcomes.is_empty());
        assert!(report.is_ok());
    }
}
