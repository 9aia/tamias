#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-install — the **one** install pipeline, shared by `tamias
//! install`/`add`/`update` (the CLI) and the `install` op of `tamias
//! serve` (the JSON-RPC surface):
//!
//! ```text
//! ref → parse → plan → fetch-cache hit or fetch+commit
//!     → payload manifest → bundled-driver consent gate
//!     → `template.extends` parent materialization (`.scarab-parent/`)
//!     → register bundled drivers → driver `install` dispatch
//!     → store commit → declared `bins[]` deploy
//! ```
//!
//! Driver work runs *before* the store commit: a missing or failing
//! driver fails the install with no committed entry (the fetch-cache
//! entry is content-addressed and stays). Bundled `drivers/` are
//! arbitrary code shipped inside the package — they are never
//! registered or executed without explicit consent (`--allow-drivers`,
//! `"allow_drivers":true`, `TAMIAS_ALLOW_DRIVERS=1`, or a recorded
//! per-package consent — see [`consent`]).
//!
//! The `bins[]` deploy is the mirror image — *after* the commit, on
//! every install path (CLI, serve op, sync dep, TUI job), via
//! [`install_with_progress_opts`]'s `deploy_bins` tail; a deploy
//! failure fails the install post-commit (`tamias bin deploy --force`
//! is the retry).
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
use tamias_source_registry::RegistryRefKind;
use tamias_store_local::Store;

mod consent;
mod extends;
mod include;
mod sync;
pub use sync::{sync_project, DepOutcome, DepStatus, SyncOptions, SyncReport, SyncSource};

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
    /// Bin dir the post-commit `bins[]` deploy writes into — `None`
    /// → [`tamias_bin::default_bin_dir`] (`TAMIAS_BIN_DIR` →
    /// `XDG_BIN_HOME` → `~/.local/bin`). Surfaces with their own
    /// bin-dir resolution (the CLI's `Env::bin_dir`) pass `Some`.
    pub bin_dir: Option<PathBuf>,
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
            bin_dir: None,
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
    /// The post-commit declared-`bins[]` deploy — emitted once per
    /// top-level install whose manifest declares bins (or that
    /// produced deploy warnings). `actions` are the
    /// `bin: <path> (deployed|already)` lines; `warnings` are
    /// collision/foreign-file notices. The CLI prints actions like
    /// driver actions and warnings to stderr; other surfaces may
    /// drop them.
    BinsDeployed {
        /// Deploy action lines from [`tamias_bin::deploy_bins`].
        actions: Vec<String>,
        /// Collision/foreign-file warnings from the deploy.
        warnings: Vec<String>,
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
    install_with_progress_opts(env, reference, allow_drivers, surface, report, true)
}

/// [`install_with_progress`] plus the post-commit `bins[]` deploy
/// toggle — `deploy_bins: false` is the CLI's `--no-bins`: the entry
/// commits but declared bins stay undeployed (`tamias bin deploy`
/// materializes them later). Every other caller deploys: the deploy
/// lives *here*, in the one pipeline, so `tamias install`/`add`/
/// `update`, `tamias serve`'s `install` op, `sync` deps, and the TUI's
/// install jobs all land declared bins on `PATH` uniformly. Only the
/// top-level entry deploys — `include:`/`extends` bases recurse
/// through [`install_inner`] directly and never reach this tail.
///
/// # Errors
/// Same as [`install_with_progress`], plus post-commit bin-deploy
/// failures (a foreign file at a bin path, I/O) — the entry is
/// committed either way; `tamias bin deploy --force` is the retry.
pub fn install_with_progress_opts(
    env: &Env,
    reference: &str,
    allow_drivers: bool,
    surface: Surface,
    report: &mut dyn FnMut(Progress),
    deploy_bins: bool,
) -> Result<StoreEntry, Error> {
    let entry = install_inner(
        env,
        reference,
        allow_drivers,
        surface,
        report,
        &mut include::IncludeState::default(),
    )?;
    if deploy_bins {
        deploy_committed_bins(env, &entry, report)?;
    }
    Ok(entry)
}

/// The post-commit half of the bin lifecycle — deploy the entry's
/// declared `bins[]` into the bin dir. Runs on *every* install path
/// (see [`install_with_progress_opts`]); roots match the uninstall
/// side's ([`tamias_bin::default_bin_dir`] /
/// [`tamias_bin::default_alias_root`] — `TAMIAS_BIN_DIR`/
/// `XDG_BIN_HOME` resolution is the same one the CLI's `Env` uses).
/// A deploy failure fails the install *after* the commit: the entry
/// is honest state and `bin deploy --force` is the retry path.
fn deploy_committed_bins(
    env: &Env,
    entry: &StoreEntry,
    report: &mut dyn FnMut(Progress),
) -> Result<(), Error> {
    let bin_dir = env
        .bin_dir
        .clone()
        .unwrap_or_else(tamias_bin::default_bin_dir);
    let deployed = tamias_bin::deploy_bins(
        entry,
        &bin_dir,
        &tamias_bin::default_alias_root(&env.store_root),
        false,
    )?;
    if !deployed.actions.is_empty() || !deployed.warnings.is_empty() {
        report(Progress::BinsDeployed {
            actions: deployed.actions,
            warnings: deployed.warnings,
        });
    }
    Ok(())
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
    let (payload_dir, resolved_id, signature, _) = fetch_or_cached(env, &planned)?;
    let root = payload_root(&payload_dir)?;
    let manifest = Manifest::load(&root)?;
    // Sealed packs are ciphertext-only (ADR-0004): every payload file
    // must be `.age` with the age magic — a stray plaintext byte is a
    // hard error before anything dispatches or commits. `sealed:`
    // manifests can't declare `include:`, so `root` is the tree the
    // store will commit.
    tamias_core::sealed::check_ciphertext_only(&root, &manifest)?;
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
    // Trust-tier snapshot for reporting — `enforce_signature` above is
    // still the gate; this records *which* tier the provenance earned
    // (official/signed/pinned/unpinned/bad) plus the verified signing
    // principal so `list`/`info`/`verify` can badge it without
    // re-running ssh-keygen. `reference` is the verbatim ref — pin
    // detection reads it.
    let (trust, signer) = tamias_sign::trust_and_signer(
        reference,
        signature.as_deref(),
        &manifest.name,
        manifest.scope.as_deref(),
        manifest.version.as_deref(),
        &payload_digest,
        &env.signing,
    )?;
    // Bundled drivers are arbitrary code shipped inside the package — they
    // must never be registered or executed without explicit consent. Fail
    // *before* the store entry is written so a refused install leaves no
    // partial state (the fetch-cache entry is content-addressed and stays).
    // Consent comes from the flag/env OR a recorded per-package consent
    // bound to this exact payload digest (`consent` module — P-005 §D.3).
    let shipped = tamias_driver::bundled_drivers(&work_root)?;
    // The store label this install will commit under — the consent key.
    // Computed before `record` consumes `resolved_id`.
    let consent_label = include::pending_label(
        &manifest.name,
        manifest.scope.as_deref(),
        manifest.version.as_deref(),
        &resolved_id,
    );
    if !shipped.is_empty()
        && !tamias_driver::drivers_allowed(allow_drivers)
        && !consent::consented(&consent_label, &payload_digest)
    {
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
        // P-006 display metadata — install-time snapshot so `list`/
        // `info` report it without re-reading the payload manifest.
        description: manifest.description.clone(),
        keywords: manifest.keywords.clone(),
        homepage: manifest.homepage.clone(),
        repository: manifest.repository.clone(),
        license: manifest.license.clone(),
        authors: manifest.authors.clone(),
        source_ref: reference.to_string(),
        resolved: resolved_id,
        digest: String::new(),
        signature: signature.clone(),
        signer,
        trust: Some(trust.as_str().to_string()),
        installed_at: 0,
    };
    // `template.extends:` — materialize the parent's payload at
    // `.scarab-parent/` inside the tree the driver *and* the store
    // commit see, so the `install` dispatch below and later
    // `deploy`/`uninstall` verbs (which read the committed store
    // payload) all get the whole ancestor chain. Runs AFTER the
    // signature/trust/consent gates above: those digests cover the
    // package's own (include-composed) bytes — publish doesn't
    // flatten `extends`, so a signed parent-extending pack still
    // verifies — while `Store::install` digests the final tree
    // including `.scarab-parent/` (`verify` stays honest).
    let mut extends_scratch = None;
    let mut driver_root = work_root.clone();
    let extends_parents = extends::parents(&work_root)?;
    if !extends_parents.is_empty() {
        if driver_root == root {
            // `work_root` aliases the content-addressed fetch cache —
            // it must never be written into; stage a private copy.
            let scratch = tempfile::tempdir()
                .map_err(Error::io("create extends-compose scratch".to_string()))?;
            tamias_core::copy_tree(&work_root, scratch.path())?;
            driver_root = scratch.path().to_path_buf();
            extends_scratch = Some(scratch);
        }
        extends::materialize(
            env,
            &driver_root,
            &consent_label,
            &extends_parents,
            state,
            allow_drivers,
            surface,
            report,
        )?;
    }
    let _extends_guard = extends_scratch;
    // Declared `bins[]` + `tools/bin/<triple>/` payloads — the same
    // trust rule as `drivers/bin/`: a payload that ships platform dirs
    // but none for this host fails the install, and every declared
    // payload bin must resolve to a real file in the committed tree.
    // Runs before driver dispatch + the store commit, so a broken bin
    // declaration leaves no partial state.
    tamias_bin::check_bins(&driver_root, &manifest)?;
    // Driver work runs *before* the store commit: a missing or failing
    // driver fails the install with no committed entry (the fetch-cache
    // entry is content-addressed and stays).
    let kind = manifest.kind.as_deref().unwrap_or("files");
    for action in tamias_driver::register_bundled_drivers(&driver_root)? {
        report(Progress::DriverAction { action });
    }
    if !tamias_driver::dispatch(kind, "install", &driver_root, &record.name, None)?
        && kind != "files"
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
    let entry = Store::new(&env.store_root).install(&driver_root, record)?;
    // Record per-package consent *after* the commit — a failed install
    // must not leave consent for bytes that never landed. Advisory: a
    // consent-file write failure doesn't fail the install (the operator
    // just re-consents next time).
    if !shipped.is_empty() {
        if let Err(e) = consent::record_consent(&consent_label, &payload_digest, &shipped) {
            eprintln!("warning: failed to record driver consent: {e}");
        }
    }
    report(Progress::Done {
        label: entry.label(),
    });
    Ok(entry)
}

/// The ref grammar — kinds in claim order; the fs fallback (paths) is
/// last. `RegistryRefKind` sits before `FsRefKind` because a scoped
/// registry ref (`9aia:@scope/pkg@1.0.0`) contains `/` and would
/// otherwise be claimed as a path; its `claims` is shape-only (no
/// config IO) and `parse` does the registry lookup.
fn refs() -> Refs {
    Refs::new()
        .with(Arc::new(GitRefKind))
        .with(Arc::new(CfRefKind))
        .with(Arc::new(HttpRefKind))
        .with(Arc::new(RegistryRefKind::new()))
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
/// The driver `uninstall` verb is **version-aware** (T-029): drivers
/// key shared operator state (`~/.config/<product>/…`) on the bare
/// `--name`, so the verb runs only when the entry being dropped is the
/// last installed one sharing that name — removing one of several
/// versions must not tear down state the survivors still use. The
/// store entry itself is always dropped.
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
    let siblings = store.siblings_of(&entry)?;
    if siblings.is_empty() {
        if !tamias_driver::dispatch(kind, "uninstall", &entry.dir, &entry.record.name, None)?
            && kind != "files"
        {
            warn(format!(
                "no driver for type '{kind}'; provider resources may remain"
            ));
        }
    } else {
        warn(format!(
            "driver uninstall for {} skipped — sibling installs remain: {}",
            entry.label(),
            siblings
                .iter()
                .map(StoreEntry::label)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // Deployed executable surfaces — declared `bins[]` and the legacy
    // `bin/` payload links — while the entry dir still exists so
    // ownership checks (copy byte-compare, payload listing) can read
    // it. Best-effort: a cleanup failure warns, never fails the
    // removal (the store entry is the source of truth being dropped).
    if let Err(err) = tamias_bin::cleanup_after_remove(
        name,
        &entry,
        &tamias_bin::default_bin_dir(),
        &tamias_bin::default_alias_root(store_root),
    ) {
        warn(format!("bin cleanup for {name}: {err}"));
    }
    store.remove(name)
}

/// plan → cache-hit or fetch+commit → `(payload_dir, resolved_id,
/// signature, cache_hit)`. Cache hits carry no signature — only `cf:`
/// plans have one and they never prefetch-hit (no `prefetch_identity`),
/// so a signed package is always re-fetched and re-verified. `cache_hit`
/// reports whether the payload came from the cache — a prefetch hit, or
/// (for sources without `prefetch_identity`, e.g. fs paths) a commit
/// that found an intact existing entry.
fn fetch_or_cached(
    env: &Env,
    planned: &Planned,
) -> Result<(PathBuf, String, Option<String>, bool), Error> {
    let cache = FetchCache::new(&env.cache_root);
    if let Some((key, resolved_id)) = planned.prefetch_identity() {
        // Integrity-checked hit: `hit` evicts a tampered or pre-metadata
        // entry and reports it as a miss, so a poisoned cache can never
        // reach `Store::install` — the fetch below refetches instead.
        if let Some(dir) = cache.hit(&key)? {
            tracing::debug!(cache_key = %key, "fetch-cache hit");
            return Ok((dir, resolved_id, None, true));
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
    // Sources without `prefetch_identity` still cache by content key —
    // probe before commit so `cache_hit` is honest for them too. `hit`
    // verifies integrity (evicting a tampered entry as a miss), the same
    // check `commit` makes internally.
    let cache_hit = cache.hit(&fetched.content_key)?.is_some();
    let committed = cache.commit(&scratch, &fetched.content_key)?;
    tracing::debug!(content_key = %fetched.content_key, "fetched + cached");
    Ok((committed, fetched.resolved, fetched.signature, cache_hit))
}

/// The manifest identity a [`plan_install`] reports.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PlanManifest {
    /// Bare package name.
    pub name: String,
    /// `@scope/` namespace when declared.
    pub scope: Option<String>,
    /// Author-declared version.
    pub version: Option<String>,
    /// Content type (`type:` in the manifest) — `None` means `files`.
    pub kind: Option<String>,
    /// The `sealed:` format (`age/v1`) when the pack is
    /// ciphertext-only — `None` = plaintext payload.
    pub sealed: Option<String>,
}

/// How one `include:` entry resolves — see [`include::plan_include`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IncludeResolution {
    /// An installed store entry — carries its label
    /// (`@scope/name@version` or the sha short form).
    StoreLabel(String),
    /// Not a store name, but an installable ref — a real install would
    /// recursively install it as a base.
    WouldInstall,
    /// Neither a store name nor a resolvable ref — a real install
    /// fails naming this entry (the reason lands in
    /// [`InstallPlan::problems`]).
    Missing,
}

/// One classified `include:` entry.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PlannedInclude {
    /// The manifest's `include:` entry, verbatim.
    pub entry: String,
    /// How it resolves.
    pub resolves_to: IncludeResolution,
}

/// One `provider.services` entry — what `tamias deploy` would render
/// later; **not** part of install.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PlannedService {
    /// Service/unit name.
    pub name: String,
    /// The `ExecStart` command line.
    pub exec: String,
}

/// Everything `tamias install <ref>` WOULD do — the dry-run report
/// behind `install --plan` (proposal P-005 §D.2). Produced by
/// [`plan_install`]; nothing is dispatched to a driver and nothing is
/// committed to the store (the content-addressed fetch-cache entry is
/// inert without a store commit, so fetching is honest).
#[derive(Clone, Debug, serde::Serialize)]
pub struct InstallPlan {
    /// The ref verbatim.
    pub reference: String,
    /// The fetched payload's manifest identity.
    pub manifest: PlanManifest,
    /// Whether the payload resolved from the fetch cache.
    pub cache_hit: bool,
    /// `dir_digest` of the RAW fetched payload — the digest trust and
    /// signature were assessed against. When [`Self::has_includes`] is
    /// set a real install digests the COMPOSED tree instead, so this is
    /// the honest raw-payload approximation.
    pub payload_digest: String,
    /// `include:` is non-empty — composition applies on a real install
    /// (see [`Self::payload_digest`]).
    pub has_includes: bool,
    /// `include:` entries classified without recursing installs.
    pub includes: Vec<PlannedInclude>,
    /// `template.extends:` parents classified the same way (store
    /// name → stored payload; ref → a real install would recursively
    /// install the parent, then materialize it at `.scarab-parent/`
    /// for one parent or `.scarab-parents/<i>/` for several). Empty
    /// = the manifest declares no parents. A malformed `extends`
    /// lands in [`Self::problems`].
    pub template_extends: Vec<PlannedInclude>,
    /// Driver files the payload ships under `drivers/` (portable files
    /// plus `bin/<triple>/` payloads, reported as `bin/<triple>/<name>`).
    pub bundled_drivers: Vec<String>,
    /// Bundled drivers exist and no consent was given — a real install
    /// would refuse. Consent comes from [`tamias_driver::drivers_allowed`]
    /// (`--allow-drivers` / `TAMIAS_ALLOW_DRIVERS=1`) OR a recorded
    /// per-package consent matching this payload's digest (the `consent`
    /// module — P-005 §D.3). NOTE: for `include:` packages the recorded
    /// digest is the COMPOSED tree's, which a plan can't compute — the
    /// raw-payload check may over-report `consent_required` there.
    pub consent_required: bool,
    /// The driver a real install would dispatch to for the manifest's
    /// kind (`files` when undeclared) — `tamias-driver-<kind>` on PATH
    /// or `<tamias-home>/drivers/`. `None` = nothing found: fine for
    /// `files` (built in), a problem otherwise. NOTE: resolution may
    /// run `capabilities` probes on discovered drivers — trusted local
    /// binaries, 10s timeout — acceptable for a plan.
    pub driver: Option<PathBuf>,
    /// Declared `bins[]` command names (`tamias bin deploy` would
    /// materialize them on a real install).
    pub bins: Vec<String>,
    /// `provider.services` — what `tamias deploy` would later create.
    /// Clearly NOT part of install.
    pub provider_services: Vec<PlannedService>,
    /// The trust tier the provenance earns (`official`/`signed`/
    /// `pinned`/`unpinned`/`bad`, or `unknown` when assessment itself
    /// failed — see `problems`).
    pub trust: String,
    /// The verified signing principal, when one checked out.
    pub signer: Option<String>,
    /// The signature check's spelling (`verified`/`bad`/`unchecked`)
    /// when the source carried a signature; `None` = unsigned (or the
    /// check itself failed — see `problems`).
    pub signature_status: Option<String>,
    /// Would-be failures the real install would hit: the signature
    /// gate (`enforce_signature`), a missing driver for a non-`files`
    /// kind, refused bundled-driver consent, unresolvable `include:`
    /// entries. A plan SURFACES these rather than hard-failing.
    pub problems: Vec<String>,
}

/// `tamias install --plan` — the dry-run half of [`install_inner`]:
/// parse → plan → fetch-or-cache → manifest, then *report* instead of
/// act. `include:` entries are classified ([`include::plan_include`]),
/// never resolved — no recursive installs, no overlay, no composed
/// digest. Driver work stops at resolution ([`tamias_driver::find_driver`]
/// may `capabilities`-probe discovered drivers — trusted local
/// binaries, 10s timeout); bundled-driver registration, dispatch, and
/// the store commit never run.
///
/// Trust/signature are computed exactly as install does, except a
/// would-be [`tamias_sign::enforce_signature`] failure is captured in
/// [`InstallPlan::problems`] — a dry-run shows "this would fail"
/// instead of hiding it or refusing to plan.
///
/// # Errors
/// Fails on an unparseable ref, a planning/fetch failure, a missing or
/// invalid package manifest, an unreadable store, or an I/O-level
/// digest/driver-listing failure. Would-be install failures land in
/// [`InstallPlan::problems`] instead.
pub fn plan_install(env: &Env, reference: &str, allow_drivers: bool) -> Result<InstallPlan, Error> {
    let spec = refs().parse(reference)?;
    let planned = sources(env.max_download).plan(&*spec)?;
    let (payload_dir, resolved_id, signature, cache_hit) = fetch_or_cached(env, &planned)?;
    let root = payload_root(&payload_dir)?;
    let manifest = Manifest::load(&root)?;
    let mut problems = Vec::new();

    // The sealed ciphertext-only gate — a real install hard-fails, so
    // the plan surfaces it as a would-be problem instead.
    if let Err(e) = tamias_core::sealed::check_ciphertext_only(&root, &manifest) {
        problems.push(format!("sealed pack check would fail: {e}"));
    }

    // `include:` classification — a real install resolves each entry
    // (store name → stored payload dir; ref → recursive install) and
    // digests the COMPOSED tree; the plan only reports the resolution
    // class. Store entries are read once: nothing installs during a
    // plan, so the set can't change mid-classification.
    let entries = Store::new(&env.store_root).entries()?;
    let mut includes = Vec::with_capacity(manifest.include.len());
    for text in &manifest.include {
        let resolves_to = match include::plan_include(&entries, text) {
            include::IncludePlan::Store(label) => IncludeResolution::StoreLabel(label),
            include::IncludePlan::WouldInstall => IncludeResolution::WouldInstall,
            include::IncludePlan::Missing(reason) => {
                problems.push(format!("{reason} — install would fail"));
                IncludeResolution::Missing
            }
        };
        includes.push(PlannedInclude {
            entry: text.clone(),
            resolves_to,
        });
    }

    // `template.extends:` — same store-name-then-ref classification as
    // `include:`; a real install materializes each parent payload at
    // `.scarab-parent/` (one) or `.scarab-parents/<i>/` (several)
    // inside the composed tree. A malformed value is a would-be
    // install failure → problems, not a hard error.
    let mut template_extends = Vec::new();
    match extends::parents(&root) {
        Ok(parents) => {
            for p in parents {
                let resolves_to = match include::plan_base(&entries, &p.source, "template.extends")
                {
                    include::IncludePlan::Store(label) => IncludeResolution::StoreLabel(label),
                    include::IncludePlan::WouldInstall => IncludeResolution::WouldInstall,
                    include::IncludePlan::Missing(reason) => {
                        problems.push(format!("{reason} — install would fail"));
                        IncludeResolution::Missing
                    }
                };
                template_extends.push(PlannedInclude {
                    entry: p.source,
                    resolves_to,
                });
            }
        }
        Err(e) => {
            problems.push(format!("template.extends: {e} — install would fail"));
        }
    }

    // The RAW payload digest — see `payload_digest`/`has_includes` on
    // [`InstallPlan`] for the composed-tree caveat.
    let payload_digest = dir_digest(&root)?;
    // Surface, don't fail: run the same gate install runs and record
    // what it would have said.
    if let Err(e) = tamias_sign::enforce_signature(
        &manifest.name,
        manifest.scope.as_deref(),
        manifest.version.as_deref(),
        &payload_digest,
        signature.as_deref(),
        &env.signing,
    ) {
        problems.push(format!("signature gate would fail: {e}"));
    }
    let signature_status = match tamias_sign::signature_status(
        &manifest.name,
        manifest.scope.as_deref(),
        manifest.version.as_deref(),
        &payload_digest,
        signature.as_deref(),
        env.signing.allowed_signers.as_deref(),
        &env.signing.namespace,
    ) {
        Ok(status) => signature.is_some().then(|| status.as_str().to_string()),
        Err(e) => {
            problems.push(format!("signature check failed: {e}"));
            None
        }
    };
    let (trust, signer) = match tamias_sign::trust_and_signer(
        reference,
        signature.as_deref(),
        &manifest.name,
        manifest.scope.as_deref(),
        manifest.version.as_deref(),
        &payload_digest,
        &env.signing,
    ) {
        Ok((tier, principal)) => (tier.as_str().to_string(), principal),
        Err(e) => {
            problems.push(format!("trust assessment failed: {e}"));
            ("unknown".to_string(), None)
        }
    };

    // Bundled drivers — consent comes from `drivers_allowed` OR a
    // recorded per-package consent bound to this payload's digest (the
    // same check `install_inner` runs; see `consent_required`'s doc for
    // the composed-digest caveat on `include:` packages).
    let shipped = tamias_driver::bundled_drivers(&root)?;
    let consent_label = include::pending_label(
        &manifest.name,
        manifest.scope.as_deref(),
        manifest.version.as_deref(),
        &resolved_id,
    );
    let consent_required = !shipped.is_empty()
        && !tamias_driver::drivers_allowed(allow_drivers)
        && !consent::consented(&consent_label, &payload_digest);
    if consent_required {
        problems.push(format!(
            "package ships bundled driver(s): {} — install would refuse without consent \
             (--allow-drivers / TAMIAS_ALLOW_DRIVERS=1)",
            shipped.join(", ")
        ));
    }

    // Driver resolution — `find_driver` may run `capabilities` probes
    // on discovered drivers (trusted local binaries, 10s timeout).
    let kind = manifest.kind.clone().unwrap_or_else(|| "files".to_string());
    let driver = tamias_driver::find_driver(&kind);
    if kind != "files" && driver.is_none() {
        problems.push(format!(
            "no `tamias-driver-{kind}` found — install would fail unless a bundled \
             driver covers '{kind}'"
        ));
    }

    // Declared bins — names always report; the `tools/bin/` platform
    // rule + payload resolution surface as would-be problems (a real
    // install hard-fails via `tamias_bin::check_bins`).
    let bins: Vec<String> = manifest.bins.iter().map(|b| b.name.clone()).collect();
    if let Err(e) = tamias_bin::check_platform(&root) {
        problems.push(format!("{e} — install would fail"));
    }
    for decl in &manifest.bins {
        if decl.payload.is_some() {
            if let Err(e) = tamias_bin::resolve_payload_bin(&root, decl) {
                problems.push(format!("bin '{}': {e} — install would fail", decl.name));
            }
        }
    }

    // `provider:` services — deploy-time resources, reported so the
    // plan shows the full picture; install never touches them.
    let provider_services = manifest
        .provider
        .as_ref()
        .map(|p| {
            p.services
                .iter()
                .map(|s| PlannedService {
                    name: s.name.clone(),
                    exec: s.exec.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(InstallPlan {
        reference: reference.to_string(),
        manifest: PlanManifest {
            name: manifest.name,
            scope: manifest.scope,
            version: manifest.version,
            kind: manifest.kind,
            sealed: manifest.sealed.map(|s| s.format),
        },
        cache_hit,
        payload_digest,
        has_includes: !includes.is_empty(),
        includes,
        template_extends,
        bundled_drivers: shipped,
        consent_required,
        driver,
        bins,
        provider_services,
        trust,
        signer,
        signature_status,
        problems,
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tamias_registry::Registry as _;

    use super::*;

    /// Serializes tests that mutate process env (`TAMIAS_HOME`,
    /// `TAMIAS_ALLOW_DRIVERS`, `TAMIAS_CONSENT_FILE`). `cargo test` runs
    /// tests on threads in one process, so mutations must be serialized
    /// *and* restored even on panic. `pub(crate)` — `consent`'s
    /// `consent_path` test shares the lock.
    pub(crate) static DRVENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard for [`DRVENV`]: restores the process environment and
    /// working directory on drop — panic-safe. Poison is ignored: an
    /// unwinding sibling already restored everything via its `Drop`.
    /// `pub(crate)` — `sync`'s tests share the lock (env reads like
    /// `tamias_home()`/`drivers_allowed` are process-global, so *every*
    /// test that can reach driver discovery must serialize, not just
    /// the mutators).
    pub(crate) struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        cwd: std::path::PathBuf,
    }

    impl EnvGuard {
        pub(crate) fn new() -> Self {
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
            bin_dir: None,
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
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        let dir = pkg(tmp.path(), "w");
        let reference = dir.to_str().unwrap().to_string();
        let mut events = Vec::new();
        let entry = install_with_progress(&env, &reference, false, Surface::Cli, &mut |p| {
            events.push(p)
        })
        .unwrap();
        assert_eq!(entry.record.name, "w");
        // a bare fs path carries no pin and no signature → unpinned
        assert_eq!(entry.record.trust.as_deref(), Some("unpinned"));
        assert_eq!(entry.record.signer, None);
        assert!(matches!(&events[0], Progress::Fetching { .. }));
        assert!(matches!(&events[1], Progress::Installing { .. }));
        assert!(matches!(&events[2], Progress::Done { .. }));
        // the silent convenience path installs the same way
        let (tmp2, env2) = self::env();
        let dir2 = pkg(tmp2.path(), "w2");
        let entry2 = install(&env2, dir2.to_str().unwrap(), false).unwrap();
        assert_eq!(entry2.record.name, "w2");
    }

    /// ADR-0004: a sealed pack's payload is ciphertext-only — the
    /// pipeline carries it opaquely (digest/verify unchanged), a clean
    /// pack installs, and stray plaintext hard-fails before commit.
    #[test]
    fn sealed_pack_gate() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        let write_age = |dir: &std::path::Path, rel: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut bytes = tamias_core::sealed::AGE_MAGIC.to_vec();
            bytes.extend_from_slice(b"\n-> X25519 fake\n\nBODY");
            std::fs::write(path, bytes).unwrap();
        };
        let sealed_yaml = "name: sec\nversion: 1.0.0\nsealed:\n  format: age/v1\n  \
                          recipients: sealed/recipients\n  files:\n    \
                          token: sealed/token.age\n";
        // clean sealed payload installs as `files` (no `type:` needed —
        // `sealed:` is orthogonal to the content type)
        let pkg = tmp.path().join("sec");
        std::fs::create_dir_all(pkg.join("sealed")).unwrap();
        std::fs::write(pkg.join("tamias.yaml"), sealed_yaml).unwrap();
        std::fs::write(pkg.join("sealed/recipients"), "age1pub…\n").unwrap();
        write_age(&pkg, "sealed/token.age");
        let entry = install(&env, pkg.to_str().unwrap(), false).unwrap();
        assert_eq!(entry.record.name, "sec");
        assert_eq!(
            entry.manifest().unwrap().sealed.unwrap().format,
            tamias_core::sealed::AGE_FORMAT
        );
        // plan reports the sealed format, no problems
        let plan = plan_install(&env, pkg.to_str().unwrap(), false).unwrap();
        assert_eq!(plan.manifest.sealed.as_deref(), Some("age/v1"));
        assert!(plan.problems.is_empty(), "{:?}", plan.problems);
        // stray plaintext → install fails before commit; plan surfaces it
        let bad = tmp.path().join("bad");
        std::fs::create_dir_all(bad.join("sealed")).unwrap();
        std::fs::write(
            bad.join("tamias.yaml"),
            sealed_yaml.replace("name: sec", "name: bad"),
        )
        .unwrap();
        std::fs::write(bad.join("sealed/recipients"), "age1pub…\n").unwrap();
        write_age(&bad, "sealed/token.age");
        std::fs::write(bad.join("notes.txt"), "oops").unwrap();
        let err = install(&env, bad.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("notes.txt"), "{err}");
        assert!(Store::new(&env.store_root).find("bad").is_err());
        let plan = plan_install(&env, bad.to_str().unwrap(), false).unwrap();
        assert!(
            plan.problems
                .iter()
                .any(|p| p.contains("sealed pack check") && p.contains("notes.txt")),
            "{:?}",
            plan.problems
        );
    }

    #[test]
    fn consent_gate_and_surface_hints() {
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::set_var("TAMIAS_CONSENT_FILE", tmp.path().join("consented.json"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
            max_download: None,
            signing: tamias_sign::SigningPolicy::default(),
            bin_dir: None,
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

    /// A package dir shipping one bundled driver file.
    fn pkg_with_driver(dir: &std::path::Path, name: &str) -> PathBuf {
        let pkg = dir.join(name);
        std::fs::create_dir_all(pkg.join("drivers")).unwrap();
        std::fs::write(
            pkg.join("tamias.yaml"),
            format!("name: {name}\nversion: 1.0.0\n"),
        )
        .unwrap();
        std::fs::write(
            pkg.join("drivers/tamias-driver-files"),
            "#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        pkg
    }

    /// Env + store/cache for the consent tests — `TAMIAS_CONSENT_FILE`
    /// isolated into the tempdir so the real `~/.config` is never read
    /// or written.
    fn consent_env(tmp: &std::path::Path) -> Env {
        Env {
            store_root: tmp.join("store"),
            cache_root: tmp.join("cache"),
            max_download: None,
            signing: tamias_sign::SigningPolicy::default(),
            bin_dir: None,
        }
    }

    #[test]
    fn consent_memory_skips_reprompt_for_identical_bytes() {
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        let consent_file = tmp.path().join("consented.json");
        std::env::set_var("TAMIAS_CONSENT_FILE", &consent_file);
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = consent_env(tmp.path());
        let pkg = pkg_with_driver(tmp.path(), "sd");
        let r = pkg.to_str().unwrap().to_string();
        // consented install (flag) → records consent for label+digest
        install_with_progress(&env, &r, true, Surface::Cli, &mut |_| {}).unwrap();
        assert!(consent_file.is_file(), "consent should be recorded");
        // drop the store entry so the reinstall is a real pipeline run
        Store::new(&env.store_root).remove("sd").unwrap();
        // identical bytes, NO flag/env → the recorded consent carries it
        install_with_progress(&env, &r, false, Surface::Cli, &mut |_| {}).unwrap();
        // --plan tells the same truth: no consent required
        let plan = plan_install(&env, &r, false).unwrap();
        assert!(!plan.consent_required, "{:?}", plan.problems);
        assert!(plan.problems.is_empty(), "{:?}", plan.problems);
        // tamper the payload → new digest → consent no longer applies
        std::fs::write(pkg.join("b.txt"), "changed").unwrap();
        let err = install_with_progress(&env, &r, false, Surface::Cli, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to install + run them"), "{err}");
        let plan = plan_install(&env, &r, false).unwrap();
        assert!(plan.consent_required);
        // re-consent on the new bytes, then it sticks again
        install_with_progress(&env, &r, true, Surface::Cli, &mut |_| {}).unwrap();
        Store::new(&env.store_root).remove("sd").unwrap();
        install_with_progress(&env, &r, false, Surface::Cli, &mut |_| {}).unwrap();
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn corrupt_consent_file_means_no_consent() {
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        let consent_file = tmp.path().join("consented.json");
        std::env::set_var("TAMIAS_CONSENT_FILE", &consent_file);
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = consent_env(tmp.path());
        let pkg = pkg_with_driver(tmp.path(), "sd");
        let r = pkg.to_str().unwrap().to_string();
        // a corrupt file is treated as empty — install still refuses
        std::fs::write(&consent_file, "not json{{{").unwrap();
        let err = install_with_progress(&env, &r, false, Surface::Cli, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to install + run them"), "{err}");
        // a successful consented install rewrites it as valid JSON
        install_with_progress(&env, &r, true, Surface::Cli, &mut |_| {}).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&consent_file).unwrap()).unwrap();
        assert!(parsed["sd@1.0.0"]["digest"].is_string());
        assert_eq!(parsed["sd@1.0.0"]["drivers"][0], "tamias-driver-files");
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
            bin_dir: None,
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
                official_signers: None,
                required: false,
                namespace: tamias_sign::DEFAULT_NAMESPACE.to_string(),
            },
            bin_dir: None,
        }
    }

    #[test]
    fn signed_cf_install_verifies_and_records() {
        let _g = EnvGuard::new();
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
        // trust tier + verified principal recorded at install
        assert_eq!(entry.record.trust.as_deref(), Some("signed"));
        assert_eq!(entry.record.signer.as_deref(), Some("signed-pkg"));
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
        let _g = EnvGuard::new();
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
        let _g = EnvGuard::new();
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
        let _g = EnvGuard::new();
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
        let _g = EnvGuard::new();
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
        let _g = EnvGuard::new();
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
        let _g = EnvGuard::new();
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
        let _g = EnvGuard::new();
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
        let _g = EnvGuard::new();
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
        let _g = EnvGuard::new();
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

    /// A package dir whose manifest declares `template.extends:` —
    /// `(dir, parent_ref)` written verbatim (single-quoted YAML).
    fn pkg_with_extends(dir: &std::path::Path, name: &str, parent_ref: &str) -> PathBuf {
        let pkg = dir.join(name);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("tamias.yaml"),
            format!(
                "name: {name}\nversion: 1.0.0\ntemplate:\n  extends: '{}'\n",
                parent_ref.replace('\'', "''")
            ),
        )
        .unwrap();
        pkg
    }

    /// A plain payload dir (no `template:` table) — the extends parent.
    fn plain_pkg(dir: &std::path::Path, name: &str) -> PathBuf {
        let pkg = dir.join(name);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("tamias.yaml"),
            format!("name: {name}\nversion: 1.0.0\n"),
        )
        .unwrap();
        pkg
    }

    #[test]
    fn extends_materializes_parent_into_store_payload() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        let parent = plain_pkg(tmp.path(), "base");
        std::fs::write(parent.join("base.txt"), "B").unwrap();
        let child = pkg_with_extends(tmp.path(), "child", &parent.display().to_string());
        std::fs::write(child.join("child.txt"), "C").unwrap();
        install(&env, child.to_str().unwrap(), false).unwrap();
        let dir = Store::new(&env.store_root).find("child").unwrap().dir;
        // the parent payload landed at .scarab-parent/, child files
        // stayed at the root — and the fetch cache was never written
        assert!(dir.join("child.txt").is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join(".scarab-parent/base.txt")).unwrap(),
            "B"
        );
        // the parent's own install committed too (ref resolution runs
        // the full pipeline)
        assert!(Store::new(&env.store_root).find("base").is_ok());
        // --plan classifies the extends ref without installing it
        let (tmp2, env2) = self::env();
        let child2 = pkg_with_extends(tmp2.path(), "child2", &parent.display().to_string());
        std::fs::write(child2.join("c.txt"), "c").unwrap();
        let plan = plan_install(&env2, child2.to_str().unwrap(), false).unwrap();
        assert!(matches!(
            plan.template_extends.first().map(|e| &e.resolves_to),
            Some(IncludeResolution::WouldInstall)
        ));
        assert!(plan.problems.is_empty(), "{:?}", plan.problems);
        assert!(Store::new(&env2.store_root).entries().unwrap().is_empty());
    }

    #[test]
    fn extends_multi_parent_materializes_indexed_dirs() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        let pa = plain_pkg(tmp.path(), "pa");
        std::fs::write(pa.join("a.txt"), "A").unwrap();
        let pb = plain_pkg(tmp.path(), "pb");
        std::fs::write(pb.join("b.txt"), "B").unwrap();
        // list form: string + {source, via} — declaration order is
        // precedence order (lowest first)
        let child = tmp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(
            child.join("tamias.yaml"),
            format!(
                "name: child\nversion: 1.0.0\ntemplate:\n  extends:\n    - '{}'\n    - source: '{}'\n      via: tamias-ref\n",
                pa.display(),
                pb.display()
            ),
        )
        .unwrap();
        install(&env, child.to_str().unwrap(), false).unwrap();
        let dir = Store::new(&env.store_root).find("child").unwrap().dir;
        // several parents → .scarab-parents/<index>/, NOT .scarab-parent/
        assert!(!dir.join(".scarab-parent").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join(".scarab-parents/0/a.txt")).unwrap(),
            "A"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".scarab-parents/1/b.txt")).unwrap(),
            "B"
        );
        // --plan classifies every declared parent
        let (_tmp2, env2) = self::env();
        let plan = plan_install(&env2, child.to_str().unwrap(), false).unwrap();
        assert_eq!(plan.template_extends.len(), 2);
        assert!(plan
            .template_extends
            .iter()
            .all(|e| matches!(e.resolves_to, IncludeResolution::WouldInstall)));
    }

    #[test]
    fn extends_unknown_via_fails_install() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        let parent = plain_pkg(tmp.path(), "base");
        let child = tmp.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(
            child.join("tamias.yaml"),
            format!(
                "name: child\nversion: 1.0.0\ntemplate:\n  extends:\n    source: '{}'\n    via: handl-kit\n",
                parent.display()
            ),
        )
        .unwrap();
        let err = install(&env, child.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("via 'handl-kit'"), "{err}");
        assert!(Store::new(&env.store_root).find("child").is_err());
    }

    #[test]
    fn extends_nests_grandparent_and_store_name_parent() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        // gp ← mid (extends gp by ref) ← top (extends `mid` by store
        // name — installed as a side effect of mid's own resolution? no:
        // mid is only installed if referenced; top extends mid by ref
        // first to get it in the store, then a second child uses the
        // store name.)
        let gp = plain_pkg(tmp.path(), "gp");
        std::fs::write(gp.join("gp.txt"), "G").unwrap();
        let mid = pkg_with_extends(tmp.path(), "mid", &gp.display().to_string());
        std::fs::write(mid.join("mid.txt"), "M").unwrap();
        let top = pkg_with_extends(tmp.path(), "top", &mid.display().to_string());
        std::fs::write(top.join("top.txt"), "T").unwrap();
        install(&env, top.to_str().unwrap(), false).unwrap();
        let dir = Store::new(&env.store_root).find("top").unwrap().dir;
        // top/.scarab-parent = mid's committed payload (mid.txt + its
        // own .scarab-parent = gp's payload)
        assert!(dir.join(".scarab-parent/mid.txt").is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join(".scarab-parent/.scarab-parent/gp.txt")).unwrap(),
            "G"
        );
        // now a child extending `mid` BY STORE NAME gets the same
        // already-composed payload — no re-resolution
        let via_name = pkg_with_extends(tmp.path(), "via-name", "mid");
        std::fs::write(via_name.join("v.txt"), "V").unwrap();
        install(&env, via_name.to_str().unwrap(), false).unwrap();
        let dir2 = Store::new(&env.store_root).find("via-name").unwrap().dir;
        assert_eq!(
            std::fs::read_to_string(dir2.join(".scarab-parent/.scarab-parent/gp.txt")).unwrap(),
            "G"
        );
    }

    #[test]
    fn extends_cycle_and_malformed_ref_error() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        // a ⇄ b via fs-path extends — recursion must not hang
        let a = plain_pkg(tmp.path(), "ea");
        let b = plain_pkg(tmp.path(), "eb");
        std::fs::write(
            a.join("tamias.yaml"),
            format!(
                "name: ea\nversion: 1.0.0\ntemplate:\n  extends: '{}'\n",
                b.display()
            ),
        )
        .unwrap();
        std::fs::write(
            b.join("tamias.yaml"),
            format!(
                "name: eb\nversion: 1.0.0\ntemplate:\n  extends: '{}'\n",
                a.display()
            ),
        )
        .unwrap();
        let err = install(&env, a.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("template.extends cycle"), "{err}");
        // non-string extends fails validation
        let bad = tmp.path().join("badext");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("tamias.yaml"),
            "name: badext\nversion: 1.0.0\ntemplate:\n  extends: 42\n",
        )
        .unwrap();
        std::fs::write(bad.join("a.txt"), "a").unwrap();
        let err = install(&env, bad.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("template.extends"), "{err}");
        assert!(Store::new(&env.store_root).find("badext").is_err());
    }

    #[test]
    fn bad_ref_and_missing_manifest_fail() {
        let _g = EnvGuard::new();
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

    #[test]
    fn plan_reports_without_committing_or_dispatching() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        let dir = pkg(tmp.path(), "w");
        let reference = dir.to_str().unwrap().to_string();
        let plan = plan_install(&env, &reference, false).unwrap();
        assert_eq!(plan.reference, reference);
        assert_eq!(plan.manifest.name, "w");
        assert_eq!(plan.manifest.version.as_deref(), Some("1.0.0"));
        // bare fs path, no signature → unpinned, unsigned
        assert_eq!(plan.trust, "unpinned");
        assert_eq!(plan.signer, None);
        assert_eq!(plan.signature_status, None);
        assert!(!plan.cache_hit, "first fetch can't hit the cache");
        assert!(!plan.payload_digest.is_empty());
        assert!(!plan.has_includes);
        assert!(plan.includes.is_empty());
        assert!(plan.bundled_drivers.is_empty());
        assert!(!plan.consent_required);
        // kind defaults to `files` — built in, no driver needed
        assert!(plan.driver.is_none());
        assert!(plan.provider_services.is_empty());
        assert!(plan.problems.is_empty(), "{:?}", plan.problems);
        // the dry-run committed nothing — but the cache entry is real
        // (the second plan's `cache_hit` proves it)
        assert!(Store::new(&env.store_root).entries().unwrap().is_empty());
        // a second plan resolves the same payload from the cache
        let plan2 = plan_install(&env, &reference, false).unwrap();
        assert!(plan2.cache_hit, "second plan should hit the fetch cache");
        assert!(Store::new(&env.store_root).entries().unwrap().is_empty());
    }

    #[test]
    fn plan_flags_bundled_driver_consent() {
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::set_var("TAMIAS_CONSENT_FILE", tmp.path().join("consented.json"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
            max_download: None,
            signing: tamias_sign::SigningPolicy::default(),
            bin_dir: None,
        };
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(dir.join("drivers")).unwrap();
        std::fs::write(dir.join("tamias.yaml"), "name: sd\nversion: 1.0.0\n").unwrap();
        std::fs::write(dir.join("drivers/tamias-driver-x"), "#!/bin/sh\nexit 0\n").unwrap();
        let reference = dir.to_str().unwrap().to_string();
        let plan = plan_install(&env, &reference, false).unwrap();
        assert_eq!(plan.bundled_drivers, ["tamias-driver-x"]);
        assert!(plan.consent_required);
        assert!(plan
            .problems
            .iter()
            .any(|p| p.contains("bundled driver") && p.contains("consent")));
        // consent clears the problem — nothing was registered either way
        let plan = plan_install(&env, &reference, true).unwrap();
        assert!(!plan.consent_required);
        assert!(plan.problems.is_empty(), "{:?}", plan.problems);
        assert!(Store::new(&env.store_root).entries().unwrap().is_empty());
        // and nothing was registered under <tamias-home>/drivers either
        assert!(!tmp.path().join("th/drivers/tamias-driver-x").exists());
    }

    #[test]
    fn plan_classifies_includes_without_recursing() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        // base installed into the store
        let base = tmp.path().join("basepkg");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("tamias.yaml"), "name: base\nversion: 1.0.0\n").unwrap();
        std::fs::write(base.join("base.txt"), "B").unwrap();
        install(&env, base.to_str().unwrap(), false).unwrap();
        // a ref-include target that exists but isn't installed
        let refbase = tmp.path().join("refbase");
        std::fs::create_dir_all(&refbase).unwrap();
        std::fs::write(refbase.join("tamias.yaml"), "name: rb\n").unwrap();
        // top includes: store name, installable ref, missing name, missing path
        let top = pkg_with_include(
            tmp.path(),
            "top",
            &[
                "base".to_string(),
                refbase.display().to_string(),
                "@acme/ghost".to_string(),
                tmp.path().join("nope").display().to_string(),
            ],
        );
        std::fs::write(top.join("a.txt"), "a").unwrap();
        let plan = plan_install(&env, top.to_str().unwrap(), false).unwrap();
        assert!(plan.has_includes);
        assert_eq!(plan.includes.len(), 4);
        assert_eq!(
            plan.includes[0].resolves_to,
            IncludeResolution::StoreLabel("base@1.0.0".to_string())
        );
        assert_eq!(
            plan.includes[1].resolves_to,
            IncludeResolution::WouldInstall
        );
        assert_eq!(plan.includes[2].resolves_to, IncludeResolution::Missing);
        assert_eq!(plan.includes[3].resolves_to, IncludeResolution::Missing);
        // the two missing entries surface as would-fail problems; the
        // ref-include did NOT recurse — `rb` is still not installed
        assert_eq!(
            plan.problems
                .iter()
                .filter(|p| p.contains("install would fail"))
                .count(),
            2,
            "{:?}",
            plan.problems
        );
        assert!(Store::new(&env.store_root).find("rb").is_err());
        assert!(Store::new(&env.store_root).find("top").is_err());
    }

    #[test]
    fn plan_reports_unknown_kind_driver_and_provider_services() {
        let _g = EnvGuard::new();
        let (tmp, env) = env();
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            "name: prov\nversion: 2.0.0\ntype: zzz-no-driver\n\
             provider:\n  services:\n    - name: web\n      exec: \"/bin/serve --port 8080\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let plan = plan_install(&env, dir.to_str().unwrap(), false).unwrap();
        assert_eq!(plan.manifest.kind.as_deref(), Some("zzz-no-driver"));
        assert!(plan.driver.is_none());
        assert!(plan.problems.iter().any(|p| p.contains("zzz-no-driver")));
        assert_eq!(plan.provider_services.len(), 1);
        assert_eq!(plan.provider_services[0].name, "web");
        assert_eq!(plan.provider_services[0].exec, "/bin/serve --port 8080");
        assert!(Store::new(&env.store_root).entries().unwrap().is_empty());
    }

    /// T-027: the post-commit `bins[]` deploy lives in the shared
    /// pipeline, so `install_with_progress` (the `serve` op / `sync`
    /// dep / TUI job shape) lands declared bins on `PATH` exactly like
    /// the CLI's `install` — `env.bin_dir` is the target, the
    /// `BinsDeployed` progress event reports it, and
    /// `install_with_progress_opts(.., deploy_bins: false)` is the
    /// `--no-bins` opt-out.
    #[test]
    fn install_deploys_declared_bins_unless_opted_out() {
        let _g = EnvGuard::new();
        let bins_pkg = |root: &std::path::Path| {
            let dir = root.join("pkg");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("tamias.yaml"),
                "name: w\nversion: 1.0.0\n\
                 bins:\n  - name: w-tool\n    shim: cli\n    driver: w\n",
            )
            .unwrap();
            std::fs::write(dir.join("a.txt"), "a").unwrap();
            dir
        };

        // default path deploys into env.bin_dir + reports the event
        let (tmp, mut ienv) = env();
        ienv.bin_dir = Some(tmp.path().join("userbin"));
        let dir = bins_pkg(tmp.path());
        let mut events = Vec::new();
        install_with_progress(
            &ienv,
            dir.to_str().unwrap(),
            false,
            Surface::Cli,
            &mut |p| events.push(p),
        )
        .unwrap();
        let shim = tmp.path().join("userbin/w-tool");
        assert!(shim.is_file(), "declared bin not deployed: {shim:?}");
        let script = std::fs::read_to_string(&shim).unwrap();
        assert!(script.contains("# tamias-bin: w"), "{script}");
        assert!(
            events
                .iter()
                .any(|p| matches!(p, Progress::BinsDeployed { .. })),
            "{events:?}"
        );

        // deploy_bins: false — the entry commits, no shim lands
        let (tmp2, mut env2) = env();
        env2.bin_dir = Some(tmp2.path().join("userbin"));
        let dir2 = bins_pkg(tmp2.path());
        let entry = install_with_progress_opts(
            &env2,
            dir2.to_str().unwrap(),
            false,
            Surface::Cli,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert_eq!(entry.record.name, "w");
        assert!(!tmp2.path().join("userbin/w-tool").exists());
    }

    /// T-029 regression: `remove` must not dispatch the driver's
    /// `uninstall` verb while a same-name sibling remains — drivers key
    /// shared operator state (`~/.config/<product>/…`) on `--name`, so
    /// tearing it down mid-version-churn wipes the survivors' live
    /// config. The last same-name removal runs the verb.
    #[test]
    fn remove_skips_driver_uninstall_while_siblings_remain() {
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        // A fake kind driver emulating bupha's contract: `uninstall`
        // deletes the shared name-keyed config and records that it ran.
        // The kind is invented so no real `tamias-driver-*` can shadow it.
        let drivers = tmp.path().join("th/drivers");
        std::fs::create_dir_all(&drivers).unwrap();
        let log = tmp.path().join("driver.log");
        let cfg = tmp.path().join("shared.toml");
        let script = drivers.join("tamias-driver-t029test");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" = uninstall ]; then\n  echo uninstall >> {}\n  rm -f {}\nfi\n",
                log.display(),
                cfg.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(&cfg, "live = true\n").unwrap();
        // Two installed versions of one package name.
        let store = Store::new(tmp.path().join("store"));
        for v in ["0.3.0", "0.3.1"] {
            let payload = tmp.path().join(format!("pkg{v}"));
            std::fs::create_dir_all(&payload).unwrap();
            std::fs::write(payload.join("a.txt"), "a").unwrap();
            store
                .install(
                    &payload,
                    StoreRecord {
                        name: "ci-gate".into(),
                        scope: None,
                        version: Some(v.into()),
                        kind: Some("t029test".into()),
                        description: None,
                        keywords: Vec::new(),
                        homepage: None,
                        repository: None,
                        license: None,
                        authors: Vec::new(),
                        source_ref: "./x".into(),
                        resolved: format!("sha256:{v}"),
                        digest: String::new(),
                        signature: None,
                        signer: None,
                        trust: None,
                        installed_at: 0,
                    },
                )
                .unwrap();
        }
        let warns = std::cell::RefCell::new(Vec::new());
        let mut warn = |m: String| warns.borrow_mut().push(m);
        // Removing 0.3.0 while 0.3.1 remains: the driver verb is
        // skipped, the shared config survives, the store entry drops.
        remove(store.root(), "ci-gate@0.3.0", &mut warn).unwrap();
        assert!(
            cfg.is_file(),
            "shared config must survive a non-last uninstall"
        );
        assert!(
            !log.exists(),
            "driver uninstall must not run while siblings remain"
        );
        assert!(
            warns.borrow().iter().any(|w| w.contains("skipped")),
            "{:?}",
            warns.borrow()
        );
        assert_eq!(store.entries().unwrap().len(), 1);
        // The last version runs the verb — shared config goes with it.
        remove(store.root(), "ci-gate@0.3.1", &mut warn).unwrap();
        assert!(!cfg.exists(), "last uninstall removes shared config");
        assert!(log.exists(), "driver uninstall ran for last version");
        assert!(store.entries().unwrap().is_empty());
        std::env::remove_var("TAMIAS_HOME");
    }
}
