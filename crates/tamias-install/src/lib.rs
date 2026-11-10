#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-install — the **one** install pipeline, shared by `tamias
//! install`/`add`/`update` (the CLI) and the `install` op of `tamias
//! serve` (the JSON-RPC surface):
//!
//! ```text
//! ref → parse → plan → fetch-cache hit or fetch+commit
//!     → payload manifest → bundled-driver consent gate
//!     → register bundled drivers → driver `install` dispatch
//!     → store commit
//! ```
//!
//! Driver work runs *before* the store commit: a missing or failing
//! driver fails the install with no committed entry (the fetch-cache
//! entry is content-addressed and stays). Bundled `drivers/` are
//! arbitrary code shipped inside the package — they are never
//! registered or executed without explicit consent (`--allow-drivers`,
//! `"allow_drivers":true`, or `TAMIAS_ALLOW_DRIVERS=1`).
//!
//! Progress is reported through [`Progress`] events so each surface
//! keeps its own rendering: the CLI prints `tamias:` lines to stderr
//! and driver actions to stdout; `tamias serve` drops them. Consent
//! hints embedded in errors are surface-tuned via [`Surface`].

use std::path::PathBuf;
use std::sync::Arc;

use tamias_cache_local::FetchCache;
use tamias_core::{payload_root, Error, Manifest, StoreEntry, StoreRecord};
use tamias_source::{Planned, Refs, Sources};
use tamias_source_cf::{CfRefKind, CfSource};
use tamias_source_fs::{FsRefKind, FsSource};
use tamias_source_git::{GitRefKind, GitSource};
use tamias_source_http::{HttpRefKind, HttpSource};
use tamias_store_local::Store;

/// Store + fetch-cache roots the pipeline writes through.
#[derive(Clone, Debug)]
pub struct Env {
    /// The local store root (`<tamias-home>/store`-style).
    pub store_root: PathBuf,
    /// The fetch-cache root.
    pub cache_root: PathBuf,
}

/// Which surface is installing — tunes the consent hints embedded in
/// errors (`--allow-drivers` for the CLI, `"allow_drivers":true` for
/// the JSON-RPC op).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Surface {
    /// `tamias install --allow-drivers`.
    #[default]
    Cli,
    /// `tamias serve` `{"op":"install","allow_drivers":true}`.
    Rpc,
}

impl Surface {
    /// The clause after "refusing to install + run them; …".
    fn consent_refusal(self) -> &'static str {
        match self {
            Self::Cli => "re-run with --allow-drivers or TAMIAS_ALLOW_DRIVERS=1 to consent",
            Self::Rpc => "pass \"allow_drivers\":true or set TAMIAS_ALLOW_DRIVERS=1 to consent",
        }
    }

    /// The clause after "bundled drivers need …" in the missing-driver
    /// error.
    fn drivers_hint(self) -> &'static str {
        match self {
            Self::Cli => "--allow-drivers or TAMIAS_ALLOW_DRIVERS=1",
            Self::Rpc => "\"allow_drivers\":true or TAMIAS_ALLOW_DRIVERS=1",
        }
    }
}

/// A progress event emitted during [`install_with_progress`] — the CLI
/// renders them (`tamias:` lines to stderr, driver actions to stdout);
/// `tamias serve` drops them via [`install`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// `tamias: fetching {reference}` — before the fetch.
    Fetching {
        /// The ref being installed.
        reference: String,
    },
    /// `tamias: installing {label}` — after the manifest loads, before
    /// driver dispatch + the store commit.
    Installing {
        /// `@scope/name@version` (or the bare name when unversioned).
        label: String,
    },
    /// A bundled-driver registration action — the CLI prints it as
    /// `  {action}` on stdout.
    DriverAction {
        /// The action line (`driver: <path>`).
        action: String,
    },
    /// `tamias: done — {label}` — after the store commit.
    Done {
        /// The committed entry's label.
        label: String,
    },
}

/// Install `reference` silently — no progress events, [`Surface::Rpc`]
/// consent hints. The `tamias serve` `install` op's shape.
///
/// # Errors
/// Fails on ref/plan/fetch/manifest/driver/store errors — see
/// [`install_with_progress`].
pub fn install(env: &Env, reference: &str, allow_drivers: bool) -> Result<StoreEntry, Error> {
    install_with_progress(env, reference, allow_drivers, Surface::Rpc, &mut |_| {})
}

/// Install `reference`, reporting [`Progress`] events to `report`.
///
/// # Errors
/// Fails on an unparseable ref, a planning/fetch failure, a missing or
/// invalid package manifest, refused bundled drivers (no consent), a
/// missing driver for a non-`files` content type, a failing driver, or
/// a store-commit failure. Nothing is committed to the store on
/// failure; the content-addressed fetch-cache entry stays.
pub fn install_with_progress(
    env: &Env,
    reference: &str,
    allow_drivers: bool,
    surface: Surface,
    report: &mut dyn FnMut(Progress),
) -> Result<StoreEntry, Error> {
    let span = tracing::info_span!("install", reference);
    let _enter = span.enter();
    let spec = refs().parse(reference)?;
    let planned = sources().plan(&*spec)?;
    report(Progress::Fetching {
        reference: reference.to_string(),
    });
    let (payload_dir, resolved_id) = fetch_or_cached(env, &planned)?;
    let root = payload_root(&payload_dir)?;
    let manifest = Manifest::load(&root)?;
    let base = manifest.scope.as_ref().map_or_else(
        || manifest.name.clone(),
        |scope| format!("@{scope}/{}", manifest.name),
    );
    let installing = match &manifest.version {
        Some(v) => format!("{base}@{v}"),
        None => base,
    };
    report(Progress::Installing { label: installing });
    // Bundled drivers are arbitrary code shipped inside the package — they
    // must never be registered or executed without explicit consent. Fail
    // *before* the store entry is written so a refused install leaves no
    // partial state (the fetch-cache entry is content-addressed and stays).
    let shipped = tamias_driver::bundled_drivers(&root)?;
    if !shipped.is_empty() && !tamias_driver::drivers_allowed(allow_drivers) {
        return Err(Error::Failed(format!(
            "package ships bundled driver(s): {} — refusing to install + run them; \
             {}",
            shipped.join(", "),
            surface.consent_refusal()
        )));
    }
    let record = StoreRecord {
        name: manifest.name.clone(),
        scope: manifest.scope.clone(),
        version: manifest.version.clone(),
        kind: manifest.kind.clone(),
        source_ref: reference.to_string(),
        resolved: resolved_id,
        digest: String::new(),
        installed_at: 0,
    };
    // Driver work runs *before* the store commit: a missing or failing
    // driver fails the install with no committed entry (the fetch-cache
    // entry is content-addressed and stays).
    let kind = manifest.kind.as_deref().unwrap_or("files");
    for action in tamias_driver::register_bundled_drivers(&root)? {
        report(Progress::DriverAction { action });
    }
    if !tamias_driver::dispatch(kind, "install", &root, &record.name, None)? && kind != "files" {
        return Err(Error::Failed(format!(
            "no driver for content type '{kind}' — install `tamias-driver-{kind}` on PATH \
             or ship one in the package's `drivers/` dir{}",
            if shipped.is_empty() {
                String::new()
            } else {
                format!("; bundled drivers need {}", surface.drivers_hint())
            }
        )));
    }
    let entry = Store::new(&env.store_root).install(&root, record)?;
    report(Progress::Done {
        label: entry.label(),
    });
    Ok(entry)
}

/// The ref grammar — kinds in claim order; the fs fallback (paths) is last.
fn refs() -> Refs {
    Refs::new()
        .with(Arc::new(GitRefKind))
        .with(Arc::new(CfRefKind))
        .with(Arc::new(HttpRefKind))
        .with(Arc::new(FsRefKind))
}

/// The source registry.
fn sources() -> Sources {
    Sources::new()
        .with(Arc::new(GitSource::new()))
        .with(Arc::new(CfSource::new()))
        .with(Arc::new(HttpSource::new()))
        .with(Arc::new(FsSource::new()))
}

/// plan → cache-hit or fetch+commit → `(payload_dir, resolved_id)`.
fn fetch_or_cached(env: &Env, planned: &Planned) -> Result<(PathBuf, String), Error> {
    let cache = FetchCache::new(&env.cache_root);
    if let Some((key, resolved_id)) = planned.prefetch_identity() {
        // Integrity-checked hit: `hit` evicts a tampered or pre-metadata
        // entry and reports it as a miss, so a poisoned cache can never
        // reach `Store::install` — the fetch below refetches instead.
        if let Some(dir) = cache.hit(&key)? {
            tracing::debug!(cache_key = %key, "fetch-cache hit");
            return Ok((dir, resolved_id));
        }
    }
    tracing::debug!("fetch-cache miss — fetching");
    let scratch = cache.scratch()?;
    let fetched = match planned.fetch(&scratch) {
        Ok(fetched) => fetched,
        Err(e) => {
            cache.discard(&scratch);
            return Err(e);
        }
    };
    let committed = cache.commit(&scratch, &fetched.content_key)?;
    tracing::debug!(content_key = %fetched.content_key, "fetched + cached");
    Ok((committed, fetched.resolved))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Serializes tests that mutate `TAMIAS_HOME`/`TAMIAS_ALLOW_DRIVERS`.
    static DRVENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env() -> (tempfile::TempDir, Env) {
        let dir = tempfile::tempdir().unwrap();
        let env = Env {
            store_root: dir.path().join("store"),
            cache_root: dir.path().join("cache"),
        };
        (dir, env)
    }

    fn pkg(dir: &std::path::Path, name: &str) -> PathBuf {
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

    #[test]
    fn install_reports_progress_and_commits() {
        let (tmp, env) = env();
        let dir = pkg(tmp.path(), "w");
        let reference = dir.to_str().unwrap().to_string();
        let mut events = Vec::new();
        let entry = install_with_progress(&env, &reference, false, Surface::Cli, &mut |p| {
            events.push(p)
        })
        .unwrap();
        assert_eq!(entry.record.name, "w");
        assert!(matches!(&events[0], Progress::Fetching { .. }));
        assert!(matches!(&events[1], Progress::Installing { .. }));
        assert!(matches!(&events[2], Progress::Done { .. }));
        // the silent convenience path installs the same way
        let (tmp2, env2) = self::env();
        let dir2 = pkg(tmp2.path(), "w2");
        let entry2 = install(&env2, dir2.to_str().unwrap(), false).unwrap();
        assert_eq!(entry2.record.name, "w2");
    }

    #[test]
    fn consent_gate_and_surface_hints() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
        };
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(pkg.join("drivers")).unwrap();
        std::fs::write(pkg.join("tamias.yaml"), "name: sd\nversion: 1.0.0\n").unwrap();
        std::fs::write(
            pkg.join("drivers/tamias-driver-files"),
            "#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        let r = pkg.to_str().unwrap().to_string();
        // no consent → refused before any store entry; the surface picks
        // the consent wording
        let cli_err = install_with_progress(&env, &r, false, Surface::Cli, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(cli_err.contains("--allow-drivers"), "{cli_err}");
        let rpc_err = install(&env, &r, false).unwrap_err().to_string();
        assert!(rpc_err.contains("\"allow_drivers\":true"), "{rpc_err}");
        assert!(Store::new(&env.store_root).find("sd").is_err());
        // consent via the flag proceeds and reports the driver action
        let mut events = Vec::new();
        install_with_progress(&env, &r, true, Surface::Cli, &mut |p| events.push(p)).unwrap();
        assert!(events
            .iter()
            .any(|p| matches!(p, Progress::DriverAction { .. })));
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn unknown_kind_fails_without_entry() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
        };
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("tamias.yaml"),
            "name: su\nversion: 1.0.0\ntype: zzz-no-driver\n",
        )
        .unwrap();
        std::fs::write(pkg.join("a.txt"), "a").unwrap();
        let err = install(&env, pkg.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("zzz-no-driver"), "{err}");
        assert!(Store::new(&env.store_root).find("su").is_err());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn bad_ref_and_missing_manifest_fail() {
        let (_tmp, env) = env();
        assert!(install(&env, "", false).is_err());
        assert!(install(&env, "not-a-ref", false).is_err());
        // a dir with no tamias.yaml fails at manifest load
        let dir = tempfile::tempdir().unwrap();
        assert!(install(&env, dir.path().to_str().unwrap(), false).is_err());
        // a nameless manifest fails validation
        let nameless = tempfile::tempdir().unwrap();
        std::fs::write(nameless.path().join("tamias.yaml"), "version: 1.0.0\n").unwrap();
        assert!(install(&env, nameless.path().to_str().unwrap(), false).is_err());
    }
}
