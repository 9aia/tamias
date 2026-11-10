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
//!
//! [`sync_project`] (the `sync` module) reuses the same pipeline to
//! reconcile a project manifest's `deps:` against the store +
//! `tamias.lock` — install-if-missing-or-drifted per dep, never
//! removes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tamias_cache_local::FetchCache;
use tamias_core::{dir_digest, payload_root, Error, Manifest, StoreEntry, StoreRecord};
use tamias_source::{Planned, Refs, Sources};
use tamias_source_cf::{CfRefKind, CfSource};
use tamias_source_fs::{FsRefKind, FsSource};
use tamias_source_git::{GitRefKind, GitSource};
use tamias_source_http::{HttpRefKind, HttpSource};
use tamias_store_local::Store;

mod include;
mod sync;
pub use sync::{sync_project, DepOutcome, DepStatus, SyncOptions, SyncReport};

/// Store + fetch-cache roots the pipeline writes through.
#[derive(Clone, Debug)]
pub struct Env {
    /// The local store root (`<tamias-home>/store`-style).
    pub store_root: PathBuf,
    /// The fetch-cache root.
    pub cache_root: PathBuf,
    /// Fetch cap for http(s)/`cf:` tarballs — the configured
    /// `limits.max_download` when the caller threads it through;
    /// `None` → the source's built-in default (512 MiB). Per-package
    /// overrides can't apply here: the package name isn't known until
    /// after the fetch.
    pub max_download: Option<u64>,
    /// Package-signature policy — `signing.*` config +
    /// `TAMIAS_ALLOWED_SIGNERS` resolved by [`tamias_sign::SigningPolicy::resolve`].
    /// Sources that carry a signature (registry `cf:` refs) are
    /// verified before the store commit; `required` rejects unsigned
    /// installs.
    pub signing: tamias_sign::SigningPolicy,
}

impl Env {
    /// An `Env` whose `max_download` comes from `config`'s global
    /// `limits` — `None` config → `None` (the source's built-in
    /// default, the same 512 MiB `effective_limits` falls back to).
    /// The one resolution `tamias install`/`add`/`update`, `tamias
    /// serve`'s `install` op, and `tamias ui`'s install jobs share.
    #[must_use]
    pub fn for_config(
        store_root: PathBuf,
        cache_root: PathBuf,
        config: Option<&tamias_config::Config>,
    ) -> Self {
        Self {
            store_root,
            cache_root,
            max_download: config.map(|c| c.effective_limits(None, None).max_download),
            signing: tamias_sign::SigningPolicy::resolve(config),
        }
    }
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
    /// `tamias ui` — the TUI's bundled-driver consent modal.
    Tui,
}

impl Surface {
    /// The clause after "refusing to install + run them; …".
    fn consent_refusal(self) -> &'static str {
        match self {
            Self::Cli => "re-run with --allow-drivers or TAMIAS_ALLOW_DRIVERS=1 to consent",
            Self::Rpc => "pass \"allow_drivers\":true or set TAMIAS_ALLOW_DRIVERS=1 to consent",
            Self::Tui => "answer the consent prompt to allow them",
        }
    }

    /// The clause after "bundled drivers need …" in the missing-driver
    /// error.
    fn drivers_hint(self) -> &'static str {
        match self {
            Self::Cli => "--allow-drivers or TAMIAS_ALLOW_DRIVERS=1",
            Self::Rpc => "\"allow_drivers\":true or TAMIAS_ALLOW_DRIVERS=1",
            Self::Tui => "the consent prompt",
        }
    }
}

/// Whether `err` is the bundled-driver consent refusal — the one
/// failure an interactive surface can recover from by re-running with
/// consent (`tamias ui`'s modal). Matches the marker the pipeline
/// embeds; the [`Surface`] only changes the trailing hint.
#[must_use]
pub fn is_driver_consent_refusal(err: &Error) -> bool {
    matches!(err, Error::Failed(msg) if msg.contains("refusing to install + run them"))
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
    /// `include:` composition, driver dispatch + the store commit.
    Installing {
        /// `@scope/name@version` (or the bare name when unversioned).
        label: String,
    },
    /// `tamias: including {base}` — an `include:` base resolved to a
    /// store payload dir or an installable ref (which then streams its
    /// own install events).
    Including {
        /// The resolved base — a store label or the ref verbatim.
        base: String,
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
    install_inner(
        env,
        reference,
        allow_drivers,
        surface,
        report,
        &mut include::IncludeState::default(),
    )
}

/// The pipeline body — [`install_with_progress`] with the caller's
/// [`include::IncludeState`] threaded through so `include:` ref
/// recursion shares the depth cap + cycle set. `pub(crate)` for
/// [`include::resolve_base`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_inner(
    env: &Env,
    reference: &str,
    allow_drivers: bool,
    surface: Surface,
    report: &mut dyn FnMut(Progress),
    state: &mut include::IncludeState,
) -> Result<StoreEntry, Error> {
    let span = tracing::info_span!("install", reference);
    let _enter = span.enter();
    let spec = refs().parse(reference)?;
    let planned = sources(env.max_download).plan(&*spec)?;
    report(Progress::Fetching {
        reference: reference.to_string(),
    });
    let (payload_dir, resolved_id, signature) = fetch_or_cached(env, &planned)?;
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
    // `include:` composition — resolve each base (store name → stored
    // payload dir; ref → recursive install), overlay bases under the
    // local payload into a scratch dir, and run *everything* below —
    // signature gate, driver discovery/dispatch, store commit — on the
    // COMPOSED tree. The store record's digest therefore covers the
    // composed payload (`verify` recomputes honestly), while the
    // fetch-cache entry stays the RAW fetched payload: composition is
    // post-cache and deterministic from store state, so it is never
    // cached. The composed manifest keeps `include:` verbatim for
    // provenance ([`include::pin_manifest`]).
    //
    // Non-transitive on the store-name path: a base store entry's
    // payload is whatever was committed at its own install — already
    // composed if it had `include:` then; we never re-resolve it.
    let mut compose_scratch = None;
    let work_root = if manifest.include.is_empty() {
        root.clone()
    } else {
        let label = include::pending_label(
            &manifest.name,
            manifest.scope.as_deref(),
            manifest.version.as_deref(),
            &resolved_id,
        );
        if !state.in_progress.insert(label.clone()) {
            return Err(Error::Failed(format!(
                "include cycle: {label} includes itself"
            )));
        }
        let bases = include::resolve_bases(
            env,
            &manifest.include,
            state,
            allow_drivers,
            surface,
            report,
        );
        state.in_progress.remove(&label);
        let bases = bases?;
        let scratch =
            tempfile::tempdir().map_err(Error::io("create include-compose scratch".to_string()))?;
        let base_refs: Vec<&Path> = bases.iter().map(PathBuf::as_path).collect();
        tamias_core::overlay(&base_refs, &root, scratch.path())?;
        include::pin_manifest(&root, scratch.path())?;
        let dir = scratch.path().to_path_buf();
        compose_scratch = Some(scratch);
        dir
    };
    let _compose_guard = compose_scratch;
    // Provenance gate: a source-carried signature (registry `cf:` refs)
    // must verify against `allowed_signers`, and `signing.required`
    // rejects unsigned installs — before any driver runs or the store
    // commits (a refused install leaves no partial state). The digest
    // covers the COMPOSED payload — publish flattens includes the same
    // way, so a flattened+signed package verifies here.
    let payload_digest = dir_digest(&work_root)?;
    tamias_sign::enforce_signature(
        &manifest.name,
        manifest.scope.as_deref(),
        manifest.version.as_deref(),
        &payload_digest,
        signature.as_deref(),
        &env.signing,
    )?;
    // Bundled drivers are arbitrary code shipped inside the package — they
    // must never be registered or executed without explicit consent. Fail
    // *before* the store entry is written so a refused install leaves no
    // partial state (the fetch-cache entry is content-addressed and stays).
    let shipped = tamias_driver::bundled_drivers(&work_root)?;
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
        signature: signature.clone(),
        installed_at: 0,
    };
    // Driver work runs *before* the store commit: a missing or failing
    // driver fails the install with no committed entry (the fetch-cache
    // entry is content-addressed and stays).
    let kind = manifest.kind.as_deref().unwrap_or("files");
    for action in tamias_driver::register_bundled_drivers(&work_root)? {
        report(Progress::DriverAction { action });
    }
    if !tamias_driver::dispatch(kind, "install", &work_root, &record.name, None)? && kind != "files"
    {
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
    let entry = Store::new(&env.store_root).install(&work_root, record)?;
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

/// The source registry — `max_download` caps http(s)/`cf:` tarball
/// fetches (the configured `limits.max_download`, `None` → default).
fn sources(max_download: Option<u64>) -> Sources {
    let cap = |s: HttpSource| match max_download {
        Some(bytes) => s.with_max_download(bytes),
        None => s,
    };
    let cf = match max_download {
        Some(bytes) => CfSource::new().with_max_download(bytes),
        None => CfSource::new(),
    };
    Sources::new()
        .with(Arc::new(GitSource::new()))
        .with(Arc::new(cf))
        .with(Arc::new(cap(HttpSource::new())))
        .with(Arc::new(FsSource::new()))
}

/// `remove` — the lightweight uninstall shared by `tamias serve`'s
/// `remove` op and `tamias ui`'s Remove job: find the entry, dispatch
/// the kind driver's `uninstall` verb *before* dropping the store
/// entry (a failing driver keeps it), then `Store::remove`. A missing
/// driver on a non-`files` kind reports through `warn` — provider
/// resources may remain. (`files` is built in; `tamias uninstall` /
/// `tamias-provider::uninstall` is the provider-aware teardown, and
/// the CLI's bare `remove` skips dispatch entirely.)
///
/// # Errors
/// Store lookup, driver dispatch, or store removal failures.
pub fn remove(
    store_root: &Path,
    name: &str,
    warn: &mut dyn FnMut(String),
) -> Result<StoreRecord, Error> {
    let store = Store::new(store_root);
    let entry = store.find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    if !tamias_driver::dispatch(kind, "uninstall", &entry.dir, &entry.record.name, None)?
        && kind != "files"
    {
        warn(format!(
            "no driver for type '{kind}'; provider resources may remain"
        ));
    }
    store.remove(name)
}

/// plan → cache-hit or fetch+commit → `(payload_dir, resolved_id,
/// signature)`. Cache hits carry no signature — only `cf:` plans have
/// one and they never prefetch-hit (no `prefetch_identity`), so a
/// signed package is always re-fetched and re-verified.
fn fetch_or_cached(
    env: &Env,
    planned: &Planned,
) -> Result<(PathBuf, String, Option<String>), Error> {
    let cache = FetchCache::new(&env.cache_root);
    if let Some((key, resolved_id)) = planned.prefetch_identity() {
        // Integrity-checked hit: `hit` evicts a tampered or pre-metadata
        // entry and reports it as a miss, so a poisoned cache can never
        // reach `Store::install` — the fetch below refetches instead.
        if let Some(dir) = cache.hit(&key)? {
            tracing::debug!(cache_key = %key, "fetch-cache hit");
            return Ok((dir, resolved_id, None));
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
    Ok((committed, fetched.resolved, fetched.signature))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tamias_registry::Registry as _;

    use super::*;

    /// Serializes tests that mutate process env (`TAMIAS_HOME`,
    /// `TAMIAS_ALLOW_DRIVERS`). `cargo test` runs tests on threads in
    /// one process, so mutations must be serialized *and* restored
    /// even on panic.
    static DRVENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard for [`DRVENV`]: restores the process environment and
    /// working directory on drop — panic-safe. Poison is ignored: an
    /// unwinding sibling already restored everything via its `Drop`.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        cwd: std::path::PathBuf,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = DRVENV.lock().unwrap_or_else(|e| e.into_inner());
            let env = std::env::vars_os().collect();
            let cwd = std::env::current_dir().unwrap();
            Self {
                _lock: lock,
                env,
                cwd,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, _) in std::env::vars_os() {
                if !self.env.iter().any(|(sk, _)| *sk == k) {
                    std::env::remove_var(&k);
                }
            }
            for (k, v) in &self.env {
                std::env::set_var(k, v);
            }
            let _ = std::env::set_current_dir(&self.cwd);
        }
    }

    fn env() -> (tempfile::TempDir, Env) {
        let dir = tempfile::tempdir().unwrap();
        let env = Env {
            store_root: dir.path().join("store"),
            cache_root: dir.path().join("cache"),
            max_download: None,
            signing: tamias_sign::SigningPolicy::default(),
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
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
            max_download: None,
            signing: tamias_sign::SigningPolicy::default(),
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
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
            max_download: None,
            signing: tamias_sign::SigningPolicy::default(),
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

    /// `ssh-keygen -Y` needs a real key — same approach as sturn's wal
    /// tests and `tamias-sign`'s (ssh-keygen is a de-facto dep for
    /// git-over-ssh anyway).
    fn gen_key(dir: &std::path::Path, name: &str) -> PathBuf {
        let key = dir.join(name);
        let out = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .output()
            .unwrap();
        assert!(out.status.success(), "ssh-keygen keygen failed");
        key
    }

    /// `<principal> <pubkey>` allowed_signers line via `ssh-keygen -y`.
    fn allow(dir: &std::path::Path, principal: &str, key: &std::path::Path) -> PathBuf {
        let pubout = std::process::Command::new("ssh-keygen")
            .args(["-y", "-f"])
            .arg(key)
            .output()
            .unwrap();
        assert!(pubout.status.success());
        let pubkey = String::from_utf8_lossy(&pubout.stdout).trim().to_string();
        let path = dir.join(format!("allowed-{principal}"));
        std::fs::write(&path, format!("{principal} {pubkey}\n")).unwrap();
        path
    }

    /// A loopback port nothing else holds — bind :0, read, release.
    fn free_addr() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        addr
    }

    /// Publish `name@1.0.0` to a served local registry, signed when
    /// `key` is `Some`. Returns the `cf:` install ref.
    fn publish_served(
        tmp: &std::path::Path,
        name: &str,
        key: Option<&std::path::Path>,
    ) -> (String, tempfile::TempDir) {
        let src = pkg(tmp, name);
        let reg_dir = tempfile::tempdir().unwrap();
        let registry = tamias_registry_local::LocalRegistry::new(reg_dir.path());
        let manifest = Manifest::load(&src).unwrap();
        let signature = key.map(|k| {
            tamias_sign::sign_package(&src, &manifest, k, tamias_sign::DEFAULT_NAMESPACE).unwrap()
        });
        registry
            .publish(&src, &manifest, signature.as_deref())
            .unwrap();
        let addr = free_addr();
        let served = reg_dir.path().to_path_buf();
        let listen = addr.clone();
        std::thread::spawn(move || {
            let _ = tamias_registry_local::serve_http(&listen, &served);
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect(&addr).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        (format!("cf:http://{addr}/{name}@1.0.0"), reg_dir)
    }

    fn signing_env(
        store: &std::path::Path,
        cache: &std::path::Path,
        signers: &std::path::Path,
    ) -> Env {
        Env {
            store_root: store.to_path_buf(),
            cache_root: cache.to_path_buf(),
            max_download: None,
            signing: tamias_sign::SigningPolicy {
                allowed_signers: Some(signers.to_path_buf()),
                required: false,
                namespace: tamias_sign::DEFAULT_NAMESPACE.to_string(),
            },
        }
    }

    #[test]
    fn signed_cf_install_verifies_and_records() {
        let tmp = tempfile::tempdir().unwrap();
        let key = gen_key(tmp.path(), "signkey");
        let signers = allow(tmp.path(), "signed-pkg", &key);
        let (reference, _reg) = publish_served(tmp.path(), "signed-pkg", Some(&key));
        let env = signing_env(
            &tmp.path().join("store"),
            &tmp.path().join("cache"),
            &signers,
        );
        let entry = install(&env, &reference, false).unwrap();
        assert!(entry.record.signature.is_some(), "signature recorded");
        // the recorded signature re-verifies against the same anchors
        let status = tamias_sign::record_signature_status(
            &entry.record,
            Some(&signers),
            tamias_sign::DEFAULT_NAMESPACE,
        )
        .unwrap();
        assert_eq!(status, tamias_sign::SignatureStatus::Verified);
    }

    #[test]
    fn signed_cf_install_rejects_bad_and_missing_signers() {
        let tmp = tempfile::tempdir().unwrap();
        let key = gen_key(tmp.path(), "signkey");
        let other = gen_key(tmp.path(), "otherkey");
        let wrong = allow(tmp.path(), "signed-pkg", &other);
        let (reference, _reg) = publish_served(tmp.path(), "signed-pkg", Some(&key));
        // wrong authorized key → hard error, no store entry
        let env = signing_env(&tmp.path().join("store"), &tmp.path().join("cache"), &wrong);
        let err = install(&env, &reference, false).unwrap_err().to_string();
        assert!(err.contains("signature verification failed"), "{err}");
        assert!(Store::new(&env.store_root).find("signed-pkg").is_err());
        // signed package + no allowed_signers → fail closed
        let mut env2 = signing_env(
            &tmp.path().join("store2"),
            &tmp.path().join("cache2"),
            &wrong,
        );
        env2.signing.allowed_signers = None;
        let err = install(&env2, &reference, false).unwrap_err().to_string();
        assert!(err.contains("no allowed_signers"), "{err}");
    }

    #[test]
    fn required_rejects_unsigned_but_allows_signed() {
        let tmp = tempfile::tempdir().unwrap();
        let key = gen_key(tmp.path(), "signkey");
        let signers = allow(tmp.path(), "req-pkg", &key);
        let (signed_ref, _r1) = publish_served(tmp.path(), "req-pkg", Some(&key));
        let (unsigned_ref, _r2) = publish_served(tmp.path(), "plain-pkg", None);
        let mut env = signing_env(
            &tmp.path().join("store"),
            &tmp.path().join("cache"),
            &signers,
        );
        env.signing.required = true;
        // unsigned registry package → refused before store commit
        let err = install(&env, &unsigned_ref, false).unwrap_err().to_string();
        assert!(err.contains("signing.required"), "{err}");
        assert!(Store::new(&env.store_root).find("plain-pkg").is_err());
        // signed one installs fine under required
        let entry = install(&env, &signed_ref, false).unwrap();
        assert!(entry.record.signature.is_some());
    }

    /// A package dir whose manifest carries an `include:` list plus
    /// extra payload files — `(dir, files…)` written verbatim.
    fn pkg_with_include(dir: &std::path::Path, name: &str, includes: &[String]) -> PathBuf {
        let pkg = dir.join(name);
        std::fs::create_dir_all(&pkg).unwrap();
        // single-quote every entry — `@scope/…` and paths are YAML
        // indicators otherwise; `'` inside an entry doubles per YAML.
        let list = includes
            .iter()
            .map(|i| format!("  - '{}'\n", i.replace('\'', "''")))
            .collect::<String>();
        std::fs::write(
            pkg.join("tamias.yaml"),
            format!("name: {name}\nversion: 1.0.0\ninclude:\n{list}"),
        )
        .unwrap();
        pkg
    }

    #[test]
    fn include_store_name_composes_manifest_stays_provenance() {
        let (tmp, env) = env();
        // base payload: a file + a mergeable yaml doc
        let base = tmp.path().join("basepkg");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("tamias.yaml"), "name: base\nversion: 1.0.0\n").unwrap();
        std::fs::write(base.join("base.txt"), "B").unwrap();
        std::fs::write(base.join("cfg.yaml"), "k: base\nshared: b\n").unwrap();
        install(&env, base.to_str().unwrap(), false).unwrap();
        // top includes the store name `base`, overrides `k`, adds files
        let top = pkg_with_include(tmp.path(), "top", &["base".to_string()]);
        std::fs::write(top.join("top.txt"), "T").unwrap();
        std::fs::write(top.join("cfg.yaml"), "k: top\nlocal: t\n").unwrap();
        let mut events = Vec::new();
        let entry =
            install_with_progress(&env, top.to_str().unwrap(), false, Surface::Cli, &mut |p| {
                events.push(p)
            })
            .unwrap();
        assert_eq!(entry.record.name, "top");
        // the composed payload is what got committed
        let dir = Store::new(&env.store_root).find("top").unwrap().dir;
        assert_eq!(std::fs::read_to_string(dir.join("base.txt")).unwrap(), "B");
        assert_eq!(std::fs::read_to_string(dir.join("top.txt")).unwrap(), "T");
        let cfg = std::fs::read_to_string(dir.join("cfg.yaml")).unwrap();
        assert!(cfg.contains("k: top"), "{cfg}"); // local wins
        assert!(cfg.contains("shared: b"), "{cfg}"); // base merged in
                                                     // the manifest keeps include: for provenance, verbatim
        let manifest = std::fs::read_to_string(dir.join("tamias.yaml")).unwrap();
        assert!(manifest.contains("include:"), "{manifest}");
        assert!(manifest.contains("- 'base'"), "{manifest}");
        // verify recomputes the COMPOSED digest honestly
        assert!(Store::new(&env.store_root).verify("top").unwrap().ok);
        // progress surfaced the base resolution
        assert!(events
            .iter()
            .any(|p| matches!(p, Progress::Including { base } if base == "base@1.0.0")));
    }

    #[test]
    fn include_ordering_earlier_wins() {
        let (tmp, env) = env();
        for (name, k) in [("basea", "A"), ("baseb", "B")] {
            let b = tmp.path().join(name);
            std::fs::create_dir_all(&b).unwrap();
            std::fs::write(
                b.join("tamias.yaml"),
                format!("name: {name}\nversion: 1.0.0\n"),
            )
            .unwrap();
            std::fs::write(b.join("cfg.yaml"), format!("k: {k}\n")).unwrap();
            install(&env, b.to_str().unwrap(), false).unwrap();
        }
        let top = pkg_with_include(tmp.path(), "top", &["basea".into(), "baseb".into()]);
        std::fs::write(top.join("cfg.yaml"), "local: t\n").unwrap();
        install(&env, top.to_str().unwrap(), false).unwrap();
        let dir = Store::new(&env.store_root).find("top").unwrap().dir;
        let cfg = std::fs::read_to_string(dir.join("cfg.yaml")).unwrap();
        // earlier include (basea) outranks the later one (baseb)
        assert!(cfg.contains("k: A"), "{cfg}");
        assert!(!cfg.contains("k: B"), "{cfg}");
        assert!(cfg.contains("local: t"), "{cfg}");
    }

    #[test]
    fn include_ref_recurses_and_store_base_is_not_recomposed() {
        let (tmp, env) = env();
        // mid includes base BY REF (fs path) — recursion installs it
        let base = tmp.path().join("basepkg");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("tamias.yaml"), "name: base\nversion: 1.0.0\n").unwrap();
        std::fs::write(base.join("base.txt"), "B").unwrap();
        let base_ref = base.display().to_string();
        let mid = pkg_with_include(tmp.path(), "mid", &[base_ref]);
        std::fs::write(mid.join("mid.txt"), "M").unwrap();
        install(&env, mid.to_str().unwrap(), false).unwrap();
        // top includes `mid` BY STORE NAME — mid's stored payload is
        // already composed (base.txt rides inside); no re-resolution.
        let top = pkg_with_include(tmp.path(), "top", &["mid".to_string()]);
        std::fs::write(top.join("top.txt"), "T").unwrap();
        install(&env, top.to_str().unwrap(), false).unwrap();
        let dir = Store::new(&env.store_root).find("top").unwrap().dir;
        for f in ["base.txt", "mid.txt", "top.txt"] {
            assert!(dir.join(f).is_file(), "missing {f}");
        }
        // the transitive base's manifest did not leak into top's
        let manifest = std::fs::read_to_string(dir.join("tamias.yaml")).unwrap();
        assert!(manifest.contains("name: top"), "{manifest}");
    }

    #[test]
    fn include_missing_name_and_ref_errors() {
        let (tmp, env) = env();
        for entry in ["ghost", "@acme/ghost"] {
            let p = pkg_with_include(tmp.path(), "top", &[entry.to_string()]);
            std::fs::write(p.join("a.txt"), "a").unwrap();
            let err = install(&env, p.to_str().unwrap(), false)
                .unwrap_err()
                .to_string();
            assert!(err.contains("not installed"), "{err}");
            assert!(err.contains("tamias install"), "{err}");
        }
        // a path-shaped include that doesn't exist fails the same way
        let missing = tmp.path().join("no-such-dir");
        let p = pkg_with_include(tmp.path(), "top2", &[missing.display().to_string()]);
        std::fs::write(p.join("a.txt"), "a").unwrap();
        let err = install(&env, p.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not installed"), "{err}");
        assert!(Store::new(&env.store_root).find("top").is_err());
        assert!(Store::new(&env.store_root).find("top2").is_err());
    }

    #[test]
    fn include_cycle_errors() {
        let (tmp, env) = env();
        // a ⇄ b via fs-path includes — recursion must not hang
        let pa = pkg_with_include(tmp.path(), "ca", &[]);
        let pb = pkg_with_include(tmp.path(), "cb", &[]);
        // rewrite manifests with mutual includes (abs paths are fs refs)
        let a_ref = pa.display().to_string();
        let b_ref = pb.display().to_string();
        std::fs::write(
            pa.join("tamias.yaml"),
            format!("name: ca\nversion: 1.0.0\ninclude:\n  - '{b_ref}'\n"),
        )
        .unwrap();
        std::fs::write(
            pb.join("tamias.yaml"),
            format!("name: cb\nversion: 1.0.0\ninclude:\n  - '{a_ref}'\n"),
        )
        .unwrap();
        let err = install(&env, pa.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("include cycle"), "{err}");
        assert!(Store::new(&env.store_root).find("ca").is_err());
        assert!(Store::new(&env.store_root).find("cb").is_err());
    }

    #[test]
    fn include_self_via_store_name_cycles() {
        let (tmp, env) = env();
        // v1 of `me` installs plain; a same-identity payload that
        // includes the store name `me` must fail as a cycle — the
        // in-progress label equals the stored entry's label.
        let v1 = tmp.path().join("mev1");
        std::fs::create_dir_all(&v1).unwrap();
        std::fs::write(v1.join("tamias.yaml"), "name: me\nversion: 1.0.0\n").unwrap();
        std::fs::write(v1.join("a.txt"), "a").unwrap();
        install(&env, v1.to_str().unwrap(), false).unwrap();
        let v2 = pkg_with_include(tmp.path(), "mev2", &["me".to_string()]);
        // same package identity (name+version), different payload dir
        std::fs::write(
            v2.join("tamias.yaml"),
            "name: me\nversion: 1.0.0\ninclude:\n  - me\n",
        )
        .unwrap();
        std::fs::write(v2.join("b.txt"), "b").unwrap();
        let err = install(&env, v2.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("include cycle"), "{err}");
    }

    #[test]
    fn include_depth_cap_errors() {
        let (tmp, env) = env();
        // chain p0 → p1 → … → p8 → p9; p8's include resolves at depth 8
        let mut dirs = Vec::new();
        for i in 0..=9 {
            let d = tmp.path().join(format!("p{i}"));
            std::fs::create_dir_all(&d).unwrap();
            dirs.push(d);
        }
        for i in 0..=8 {
            let next = dirs[i + 1].display().to_string();
            std::fs::write(
                dirs[i].join("tamias.yaml"),
                format!("name: p{i}\nversion: 1.0.0\ninclude:\n  - '{next}'\n"),
            )
            .unwrap();
            std::fs::write(dirs[i].join("a.txt"), "a").unwrap();
        }
        std::fs::write(dirs[9].join("tamias.yaml"), "name: p9\nversion: 1.0.0\n").unwrap();
        std::fs::write(dirs[9].join("a.txt"), "a").unwrap();
        let err = install(&env, dirs[0].to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nesting cap"), "{err}");
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
