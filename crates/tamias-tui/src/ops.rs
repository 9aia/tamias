//! Blocking ops — every store/cache/registry/install mutation the TUI
//! runs, on a worker thread so the event loop never stalls. Each op
//! calls the *same* library functions `tamias-serve`'s dispatch and
//! the CLI verbs use (`tamias_install::install_with_progress`,
//! `Store::{verify,remove,retarget}`, `tamias_driver::dispatch`,
//! `FetchCache::clean`, `tamias_registry_open::{load_config,
//! registry_target,open_registry}`) — nothing is reimplemented and
//! nothing shells out to the `tamias` binary.
//!
//! `run` takes a `log` sink: install/update stream their
//! [`tamias_install::Progress`] phases through it, and every op
//! returns a one-line summary the status bar/log can show.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tamias_cache_local::FetchCache;
use tamias_core::{short_digest, Error, Lock, Manifest, StoreEntry, StoreRecord};
use tamias_install::{self, Progress, Surface};
use tamias_registry::{parse_registry_ref, RegistryEntry, RegistryRef};
use tamias_registry_open as ro;
use tamias_store_local::Store;

use crate::app::Msg;
use crate::Ctx;

/// `cache clean` default window — the shared
/// [`tamias_cache_local::DEFAULT_MAX_AGE`] (same 30 days as
/// `tamias cache clean`).
const CACHE_MAX_AGE: Duration = tamias_cache_local::DEFAULT_MAX_AGE;

/// One unit of blocking work — `Clone` so a bundled-driver consent
/// refusal can hand the job back to the UI to re-run with consent.
#[derive(Clone, Debug)]
pub enum Job {
    /// `Store::verify` — digest re-check, read-only.
    Verify(String),
    /// Driver `uninstall` dispatch + `Store::remove` (serve's rule).
    Remove(String),
    /// Re-install from the recorded `source_ref`.
    Update {
        /// `@scope/name@label`, label, or bare name.
        name: String,
        /// Bundled-driver consent (`--allow-drivers` equivalent).
        allow_drivers: bool,
    },
    /// `update` over every installed entry (`--all`).
    UpdateAll {
        /// Bundled-driver consent.
        allow_drivers: bool,
    },
    /// `Store::retarget` + `tamias.lock` sync (`tamias pin`).
    Pin {
        /// Entry to retarget.
        name: String,
        /// The new source ref.
        reference: String,
    },
    /// `FetchCache::clean` (>30d) or `clean_all`.
    CacheClean {
        /// `true` → evict everything.
        all: bool,
    },
    /// Host summary — store/cache/registry/driver checks.
    Doctor,
    /// Registry `search` against the resolved target.
    Search {
        /// Name/scope substring.
        query: String,
        /// `:registry`/`--registry` override; `None` → config/env
        /// routing (search.default → publish.default → local root).
        target: Option<String>,
    },
    /// `tamias_install` over an installable ref.
    Install {
        /// The source ref (`./dir`, `git:…`, `https://….tgz`, `cf:…`,
        /// or a registry entry's resolved tarball ref).
        reference: String,
        /// Bundled-driver consent.
        allow_drivers: bool,
    },
    // --- phase 4: provider lifecycle ---
    /// Services-pane data — every installed package's provider
    /// services + `systemctl is-enabled` state (worker: shells out).
    ServicesRefresh,
    /// `systemctl --user` lifecycle verb on a unit base name.
    UnitAction {
        /// Unit base name (no `.service`).
        unit: String,
        /// The verb.
        action: tamias_provider::UnitAction,
    },
    /// Provider deploy — kind driver `deploy` + declarative units
    /// (`tamias deploy <name> [--config]`).
    ProviderDeploy {
        /// Store label.
        name: String,
        /// Optional driver `--config` path.
        config: Option<String>,
    },
    /// Provider teardown + store remove — the heavy `tamias
    /// uninstall <name>` (driver uninstall, shared-unit survivor
    /// re-render, unit removal).
    ProviderUninstall {
        /// Store label.
        name: String,
    },
    /// Foreground provider-service exec — spawns the resolved argv
    /// with a scrubbed env, streams stdout/stderr into the log until
    /// exit or `x` (stream job — see [`stream`]).
    RunService {
        /// Store label of the provider package.
        package: String,
        /// Provider service name; `None` → sole-service pick (errors
        /// listing choices when ambiguous).
        service: Option<String>,
    },
    /// `journalctl --user` tail (`follow: false`) or follow
    /// (`follow: true`) over a package's units or a bare unit name —
    /// same argv `tamias logs`/`tamias service logs` build (stream
    /// job).
    Logs {
        /// Store label or unit base name.
        name: String,
        /// `-f` follow mode — runs until `x` kills it.
        follow: bool,
    },
    // --- phase 5: manifest deps form ---
    /// `tamias add` equivalent — install the ref, push it into
    /// `tamias.yaml`/`tamias.yml` `deps:`, upsert `tamias.lock`.
    AddDep {
        /// The source ref.
        reference: String,
        /// Bundled-driver consent.
        allow_drivers: bool,
    },
    /// Drop a ref from the project manifest's `deps:` — manifest-only
    /// (no CLI verb uninstalls on dep removal; `tamias remove` is the
    /// Installed pane's job).
    RemoveDep {
        /// The dep ref to drop.
        reference: String,
    },
    /// Replace a dep ref — install the new ref (consent applies),
    /// swap the `deps:` entry, upsert `tamias.lock`.
    SetDep {
        /// The dep ref being replaced.
        old: String,
        /// The new source ref.
        new: String,
        /// Bundled-driver consent.
        allow_drivers: bool,
    },
    // --- phase 6: authoring ---
    /// `tamias bundle pack <dir> [out]`.
    Pack {
        /// Payload dir holding `tamias.yaml`.
        dir: String,
        /// Optional output `.tgz` path.
        out: Option<String>,
    },
    /// `tamias publish <dir> [registry]` — `open_registry` +
    /// `registry.publish`, the same path `tamias-serve`'s `publish`
    /// op and the CLI take (named-registry limits apply inside
    /// `open_registry`).
    Publish {
        /// Payload dir.
        dir: String,
        /// Registry name/ref override; `None` → publish.default →
        /// local default root.
        registry: Option<String>,
    },
    /// The configured `registries` names + routing defaults — the
    /// publish picker's data.
    Registries,
}

impl Job {
    /// Status-bar / log label for the spinner.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Verify(n) => format!("verify {n}"),
            Self::Remove(n) => format!("remove {n}"),
            Self::Update { name, .. } => format!("update {name}"),
            Self::UpdateAll { .. } => "update all".to_string(),
            Self::Pin { name, .. } => format!("pin {name}"),
            Self::CacheClean { all } => {
                if *all {
                    "cache clean --all".to_string()
                } else {
                    "cache clean >30d".to_string()
                }
            }
            Self::Doctor => "doctor".to_string(),
            Self::Search { query, .. } => format!("search {query:?}"),
            Self::Install { reference, .. } => format!("install {reference}"),
            Self::ServicesRefresh => "services refresh".to_string(),
            Self::UnitAction { unit, action } => {
                format!("{} {}", unit_action_verb(*action), unit)
            }
            Self::ProviderDeploy { name, .. } => format!("deploy {name}"),
            Self::ProviderUninstall { name } => format!("uninstall {name}"),
            Self::RunService { package, service } => match service {
                Some(s) => format!("run {package}/{s}"),
                None => format!("run {package}"),
            },
            Self::Logs { name, follow } => {
                if *follow {
                    format!("logs -f {name}")
                } else {
                    format!("logs {name}")
                }
            }
            Self::AddDep { reference, .. } => format!("add dep {reference}"),
            Self::RemoveDep { reference } => format!("remove dep {reference}"),
            Self::SetDep { old, new, .. } => format!("set dep {old} → {new}"),
            Self::Pack { dir, .. } => format!("pack {dir}"),
            Self::Publish { dir, registry } => match registry {
                Some(r) => format!("publish {dir} → {r}"),
                None => format!("publish {dir}"),
            },
            Self::Registries => "registries".to_string(),
        }
    }

    /// Stream jobs spawn a child and forward its output — they run
    /// through [`stream`], not [`run`], and their child handle comes
    /// back over the channel for `x` to kill.
    #[must_use]
    pub fn is_stream(&self) -> bool {
        matches!(self, Self::RunService { .. } | Self::Logs { .. })
    }

    /// The same job with bundled-driver consent granted — what the
    /// consent modal's `y` re-runs. Non-install jobs pass through.
    #[must_use]
    pub fn with_drivers_allowed(self) -> Self {
        match self {
            Self::Update {
                name,
                allow_drivers: _,
            } => Self::Update {
                name,
                allow_drivers: true,
            },
            Self::UpdateAll { .. } => Self::UpdateAll {
                allow_drivers: true,
            },
            Self::Install { reference, .. } => Self::Install {
                reference,
                allow_drivers: true,
            },
            Self::AddDep {
                reference,
                allow_drivers: _,
            } => Self::AddDep {
                reference,
                allow_drivers: true,
            },
            Self::SetDep {
                old,
                new,
                allow_drivers: _,
            } => Self::SetDep {
                old,
                new,
                allow_drivers: true,
            },
            other => other,
        }
    }
}

/// What a finished job hands back — `Text` is the one-line summary;
/// `Search` carries typed results for the Registry pane; `Services`
/// carries the Services pane's rows.
#[derive(Debug)]
pub enum Outcome {
    /// One-line result for the log/status.
    Text(String),
    /// Registry search hits + the target they came from (shown in the
    /// pane header so `i` installs against the right registry).
    Search {
        /// Resolved target label (`name`, url, or `default`).
        target: String,
        /// Matching index entries.
        results: Vec<RegistryEntry>,
    },
    /// Services-pane rows — every installed provider service with its
    /// unit state.
    Services(Vec<tamias_provider::ServiceRow>),
}

/// The `systemctl` verb word for a [`tamias_provider::UnitAction`].
fn unit_action_verb(action: tamias_provider::UnitAction) -> &'static str {
    match action {
        tamias_provider::UnitAction::Start => "start",
        tamias_provider::UnitAction::Stop => "stop",
        tamias_provider::UnitAction::Restart => "restart",
        tamias_provider::UnitAction::Enable => "enable",
        tamias_provider::UnitAction::Disable => "disable",
    }
}

/// The effective config for read-side resolution — same
/// warn-and-ignore posture as `tamias-serve`/`cli_config`: a broken
/// config warns and yields defaults rather than failing every op.
fn env_config(ctx: &Ctx) -> tamias_config::Config {
    match ro::load_config(ctx.config_file.as_deref()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("tamias-tui: warning — ignoring config file: {e}");
            tamias_config::Config::default()
        }
    }
}

/// `install`/`update`'s `tamias_install::Env` — `max_download` from
/// the config's global `limits` via the shared [`tamias_install::Env::for_config`]
/// (per-package overrides can't apply: the name isn't known until
/// after the fetch).
fn install_env(ctx: &Ctx) -> tamias_install::Env {
    tamias_install::Env::for_config(
        ctx.store_root.clone(),
        ctx.cache_root.clone(),
        Some(&env_config(ctx)),
    )
}

/// Run `reference` through the shared install pipeline, streaming
/// [`Progress`] phases to `log`. [`Surface::Tui`] tunes the consent
/// hints; a bundled-driver refusal surfaces as
/// [`tamias_install::is_driver_consent_refusal`] so the UI can modal.
fn install_ref(
    ctx: &Ctx,
    reference: &str,
    allow_drivers: bool,
    log: &mut dyn FnMut(String),
) -> Result<StoreEntry, Error> {
    tamias_install::install_with_progress(
        &install_env(ctx),
        reference,
        allow_drivers,
        Surface::Tui,
        &mut |p| match p {
            Progress::Fetching { reference } => log(format!("fetching {reference}")),
            Progress::Installing { label } => log(format!("installing {label}")),
            Progress::DriverAction { action } => log(format!("driver: {action}")),
            Progress::Done { label } => log(format!("done — {label}")),
        },
    )
}

/// Keep `tamias.lock` honest after `update`/`pin` — the shared
/// [`Lock::sync_record`] rule (same as the CLI's `sync_lock`).
fn sync_lock(record: &tamias_core::StoreRecord) -> Result<(), Error> {
    Lock::sync_record(Path::new("tamias.lock"), record)
}

/// `verify` — recompute the payload digest against the record, plus
/// the signature status when the entry carries one.
fn verify(ctx: &Ctx, name: &str) -> Result<Outcome, Error> {
    let store = Store::new(&ctx.store_root);
    let report = store.verify(name)?;
    if !report.ok {
        return Err(Error::Failed(format!(
            "{}: digest mismatch — recorded {}, computed {}",
            report.name,
            short(&report.recorded),
            short(&report.computed)
        )));
    }
    let entry = store.find(name)?;
    let config = env_config(ctx);
    let policy = tamias_sign::SigningPolicy::resolve(Some(&config));
    let signature = tamias_sign::record_signature_status(
        &entry.record,
        policy.allowed_signers.as_deref(),
        &policy.namespace,
    )?;
    if signature == tamias_sign::SignatureStatus::Failed {
        return Err(Error::Failed(format!(
            "{}: signature verification failed",
            report.name
        )));
    }
    Ok(Outcome::Text(format!(
        "{}: ok ({}) — signature: {}",
        report.name,
        short(&report.recorded),
        signature.as_str()
    )))
}

/// `remove` — the shared lightweight uninstall
/// ([`tamias_install::remove`], serve's rule): driver `uninstall`
/// dispatch first (a failing driver keeps the store entry); a missing
/// driver on a non-`files` kind warns that provider resources may
/// remain.
fn remove(ctx: &Ctx, name: &str, log: &mut dyn FnMut(String)) -> Result<Outcome, Error> {
    let record = tamias_install::remove(&ctx.store_root, name, &mut |m| {
        log(format!("warning — {m}"));
    })?;
    Ok(Outcome::Text(format!("removed {}", record.name)))
}

/// `update` — re-resolve + re-install from the recorded `source_ref`,
/// then `tamias.lock` sync (same as `tamias update`).
fn update_one(
    ctx: &Ctx,
    name: &str,
    allow_drivers: bool,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, Error> {
    let store = Store::new(&ctx.store_root);
    let reference = store.find(name)?.record.source_ref;
    let entry = install_ref(ctx, &reference, allow_drivers, log)?;
    sync_lock(&entry.record)?;
    Ok(Outcome::Text(format!("updated {}", entry.label())))
}

/// `update --all` — every entry's recorded ref, sequentially; the
/// first failure stops the run (same as the CLI loop).
fn update_all(
    ctx: &Ctx,
    allow_drivers: bool,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, Error> {
    let store = Store::new(&ctx.store_root);
    let refs: Vec<(String, String)> = store
        .entries()?
        .into_iter()
        .map(|e| (e.label(), e.record.source_ref))
        .collect();
    if refs.is_empty() {
        return Ok(Outcome::Text("nothing installed".to_string()));
    }
    log(format!("updating {} package(s)", refs.len()));
    let mut updated = Vec::new();
    for (_label, reference) in refs {
        let entry = install_ref(ctx, &reference, allow_drivers, log)?;
        sync_lock(&entry.record)?;
        updated.push(entry.label());
    }
    Ok(Outcome::Text(format!(
        "updated {} package(s)",
        updated.len()
    )))
}

/// `pin` — retarget the recorded ref, then `tamias.lock` sync.
fn pin(ctx: &Ctx, name: &str, reference: &str) -> Result<Outcome, Error> {
    let record = Store::new(&ctx.store_root).retarget(name, reference)?;
    sync_lock(&record)?;
    Ok(Outcome::Text(format!(
        "pinned {} → {}",
        record.name, record.source_ref
    )))
}

/// `cache clean` — `FetchCache::clean` (>30d) or `clean_all`, the CLI's
/// exact calls.
fn cache_clean(ctx: &Ctx, all: bool) -> Result<Outcome, Error> {
    let cache = FetchCache::new(&ctx.cache_root);
    let evicted = if all {
        cache.clean_all()?
    } else {
        cache.clean(CACHE_MAX_AGE)?
    };
    Ok(Outcome::Text(format!(
        "evicted {} cache entries",
        evicted.len()
    )))
}

/// `doctor` — a store/cache/registry/driver summary in the spirit of
/// `tamias doctor` (the full check list stays a CLI verb for now).
fn doctor(ctx: &Ctx) -> Result<Outcome, Error> {
    let mut lines = Vec::new();
    let mut failed = 0usize;
    let mut check = |name: &str, ok: bool, detail: String| {
        if !ok {
            failed += 1;
        }
        lines.push(format!(
            "{} {name}: {detail}",
            if ok { "ok " } else { "!! " }
        ));
    };

    let store = Store::new(&ctx.store_root);
    match store.entries() {
        Ok(entries) => {
            let writable = writable(&ctx.store_root);
            check(
                "store",
                writable,
                format!("{} ({} installed)", ctx.store_root.display(), entries.len()),
            );
        }
        Err(e) => check("store", false, format!("{}: {e}", ctx.store_root.display())),
    }
    match FetchCache::new(&ctx.cache_root).entries() {
        Ok(entries) => {
            let writable = writable(&ctx.cache_root);
            check(
                "cache",
                writable,
                format!("{} ({} entries)", ctx.cache_root.display(), entries.len()),
            );
        }
        Err(e) => check("cache", false, format!("{}: {e}", ctx.cache_root.display())),
    }
    // Registry — the publish.default target (or the local root), same
    // resolution `doctor_registry` uses.
    let config = env_config(ctx);
    let target = ro::registry_target(
        None,
        &[
            config.search.default.as_deref(),
            config.publish.default.as_deref(),
        ],
    );
    match ro::open_registry(&ctx.registry_root, Some(&config), target.as_deref(), None)
        .and_then(|r| r.entries())
    {
        Ok(entries) => check(
            "registry",
            true,
            format!(
                "{}: {} entr(ies)",
                target.as_deref().unwrap_or("default"),
                entries.len()
            ),
        ),
        Err(e) => check("registry", false, e.to_string()),
    }
    // Drivers — `files` is built in; everything discovered is listed.
    let drivers = tamias_driver::discovered_drivers();
    let files = tamias_driver::find_driver("files")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "built-in".to_string());
    let kinds: Vec<&str> = drivers.iter().map(|(k, _)| k.as_str()).collect();
    check(
        "drivers",
        true,
        format!(
            "files: {files}; {}",
            if kinds.is_empty() {
                "none".into()
            } else {
                kinds.join(", ")
            }
        ),
    );
    if failed > 0 {
        return Err(Error::Failed(format!(
            "doctor: {failed} check(s) failed\n{}",
            lines.join("\n")
        )));
    }
    Ok(Outcome::Text(lines.join("\n")))
}

/// Writability probe — same create+remove probe the CLI's doctor uses.
fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".doctor-probe");
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .is_ok()
}

/// `search` — the shared `tamias_registry_open::search` routing:
/// `target` flag → `TAMIAS_REGISTRY` → `search.default` →
/// `publish.default` → the local default root; then
/// `registry.search(query)`.
fn search(ctx: &Ctx, query: &str, target: Option<&str>) -> Result<Outcome, Error> {
    let config = env_config(ctx);
    let (results, value) =
        tamias_registry_open::search(&ctx.registry_root, Some(&config), target, query)?;
    Ok(Outcome::Search {
        target: value.unwrap_or_else(|| "default".to_string()),
        results,
    })
}

/// The installable source ref for a registry index entry — the honest
/// bridge between `search` results and `tamias_install` (there is no
/// registry-entry ref grammar; entries are packed `.tgz` blobs):
///
/// - **local** registry (default root, `local` kind, or a dir ref):
///   `<root>/<file>` — an fs ref; `FsSource` unpacks the `.tgz`.
/// - **http/cf** registry: `<base>/<file>#sha256:<hex>` — an http ref
///   pinned to the index digest, so provenance is digest-checked.
///
/// `target` is the same override `:registry`/`--registry` carries.
///
/// # Errors
/// Fails when the target resolves to http without a url, or the entry
/// digest can't form a pin (unpinned http refs still install — the
/// store records whatever the fetch resolves to).
pub fn entry_install_ref(
    ctx: &Ctx,
    entry: &RegistryEntry,
    target: Option<&str>,
) -> Result<String, Error> {
    let config = env_config(ctx);
    let value = ro::registry_target(
        target,
        &[
            config.search.default.as_deref(),
            config.publish.default.as_deref(),
        ],
    );
    // Resolve to (kind, base) the way `open_registry` does — name in
    // `registries` first, then a raw dir/http ref, `None` → local root.
    enum Base {
        Local(PathBuf),
        Http(String),
    }
    let base = match &value {
        None => Base::Local(ctx.registry_root.clone()),
        Some(v) => {
            if let Some(named) = config.registries.get(v) {
                match named.effective_kind().as_str() {
                    "local" => Base::Local(
                        named
                            .url
                            .clone()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| ctx.registry_root.clone()),
                    ),
                    // http and cf share the HttpRegistry client — the
                    // tarball URL is `<base>/<file>` either way.
                    "http" | "cf" => Base::Http(named.url.clone().ok_or_else(|| {
                        Error::Failed(format!("registry '{v}': kind http/cf needs a url"))
                    })?),
                    _ => match parse_registry_ref(named.url.as_deref().unwrap_or_default()) {
                        RegistryRef::Local(path) => Base::Local(path),
                        RegistryRef::Http(base) => Base::Http(base),
                    },
                }
            } else {
                match parse_registry_ref(v) {
                    RegistryRef::Local(path) => Base::Local(path),
                    RegistryRef::Http(base) => Base::Http(base),
                }
            }
        }
    };
    match base {
        Base::Local(root) => Ok(root.join(&entry.file).display().to_string()),
        Base::Http(base) => {
            let mut url = format!("{}/{}", base.trim_end_matches('/'), entry.file);
            // Pin to the index digest when it's a well-formed sha256 —
            // `sha256:<64 hex>` per the RegistryEntry contract.
            if let Some(hex) = entry.digest.strip_prefix("sha256:") {
                if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    url.push_str(&format!("#sha256:{hex}"));
                }
            }
            Ok(url)
        }
    }
}

/// Services-pane rows — `tamias-provider`'s service table (store
/// entries → provider manifests → unit state probes).
fn services_refresh(ctx: &Ctx) -> Result<Outcome, Error> {
    Ok(Outcome::Services(tamias_provider::service_rows(
        &ctx.store_root,
    )?))
}

/// A `systemctl --user` lifecycle verb — start/stop/restart/enable/
/// disable on the unit base name.
fn unit_action(unit: &str, action: tamias_provider::UnitAction) -> Result<Outcome, Error> {
    tamias_provider::unit_action(unit, action)?;
    Ok(Outcome::Text(format!(
        "{} {}",
        unit_action_verb(action),
        unit
    )))
}

/// `tamias deploy <name> [--config]` — the `tamias-provider` report's
/// lines/warnings stream to the log; the summary toasts.
fn provider_deploy(
    ctx: &Ctx,
    name: &str,
    config: Option<&str>,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, Error> {
    let report = tamias_provider::deploy(&ctx.store_root, name, config.map(std::path::Path::new))?;
    for line in &report.lines {
        log(line.clone());
    }
    for warning in &report.warnings {
        log(format!("warning — {warning}"));
    }
    Ok(Outcome::Text(format!("deploy {name} done")))
}

/// `tamias uninstall <name>` — provider teardown + store remove; the
/// report streams like deploy's.
fn provider_uninstall(
    ctx: &Ctx,
    name: &str,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, Error> {
    let report = tamias_provider::uninstall(&ctx.store_root, name)?;
    for line in &report.lines {
        log(line.clone());
    }
    for warning in &report.warnings {
        log(format!("warning — {warning}"));
    }
    Ok(Outcome::Text(format!("uninstalled {name}")))
}

/// The project manifest the Manifest pane edits — `./tamias.yaml`
/// first (the file `tamias add` writes), then an existing
/// `./tamias.yml`; a lone `tamias.toml` is refused because
/// [`Manifest::save`] always writes YAML. Missing entirely → the
/// default manifest at `tamias.yaml` (created on first save).
pub fn project_manifest() -> Result<(PathBuf, Manifest), Error> {
    for name in ["tamias.yaml", "tamias.yml"] {
        let path = PathBuf::from(name);
        if path.is_file() {
            return Manifest::load_project_at(&path).map(|m| (path, m));
        }
    }
    if Path::new("tamias.toml").is_file() {
        return Err(Error::Failed(
            "tamias.toml found — the deps form edits YAML manifests only".to_string(),
        ));
    }
    Ok((PathBuf::from("tamias.yaml"), Manifest::default()))
}

/// `tamias add`'s manifest+lock tail — save the project manifest,
/// then upsert `tamias.lock` with the installed record (the lock is
/// created here, same as `add`) — the shared [`Lock::upsert_record`].
fn save_project_dep(path: &Path, project: &Manifest, record: &StoreRecord) -> Result<(), Error> {
    project.save(path)?;
    Lock::upsert_record(Path::new("tamias.lock"), record)
}

/// `tamias add <ref>` — install, then record the ref in the project
/// manifest's `deps:` + `tamias.lock` (the CLI handler's exact tail).
fn add_dep(
    ctx: &Ctx,
    reference: &str,
    allow_drivers: bool,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, Error> {
    let entry = install_ref(ctx, reference, allow_drivers, log)?;
    let (path, mut project) = project_manifest()?;
    if !project.deps.iter().any(|d| d == reference) {
        project.deps.push(reference.to_string());
    }
    save_project_dep(&path, &project, &entry.record)?;
    Ok(Outcome::Text(format!(
        "added {reference} → {} + tamias.lock ({})",
        path.display(),
        short(&entry.record.digest)
    )))
}

/// Drop `reference` from the project manifest's `deps:` — manifest
/// only; the store entry and lock are untouched (no CLI verb couples
/// dep removal to uninstall).
fn remove_dep(reference: &str) -> Result<Outcome, Error> {
    let (path, mut project) = project_manifest()?;
    let before = project.deps.len();
    project.deps.retain(|d| d != reference);
    if project.deps.len() == before {
        return Err(Error::Failed(format!(
            "dep '{reference}' not in {}",
            path.display()
        )));
    }
    project.save(&path)?;
    Ok(Outcome::Text(format!(
        "removed dep {reference} → {}",
        path.display()
    )))
}

/// Replace dep `old` with `new` — install the new ref first (a failed
/// install leaves the manifest untouched), swap the `deps:` entry
/// (deduping when `new` is already listed), upsert `tamias.lock`.
fn set_dep(
    ctx: &Ctx,
    old: &str,
    new: &str,
    allow_drivers: bool,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, Error> {
    if old == new {
        return Err(Error::Failed(
            "set dep: old and new ref are identical".into(),
        ));
    }
    let entry = install_ref(ctx, new, allow_drivers, log)?;
    let (path, mut project) = project_manifest()?;
    let Some(pos) = project.deps.iter().position(|d| d == old) else {
        return Err(Error::Failed(format!(
            "dep '{old}' not in {}",
            path.display()
        )));
    };
    if project.deps.iter().any(|d| d == new) {
        project.deps.remove(pos);
    } else {
        project.deps[pos] = new.to_string();
    }
    save_project_dep(&path, &project, &entry.record)?;
    Ok(Outcome::Text(format!(
        "dep {old} → {new} ({})",
        path.display()
    )))
}

/// `tamias bundle pack <dir> [out]` — `tamias-publish`'s pack.
fn pack(dir: &str, out: Option<&str>) -> Result<Outcome, Error> {
    let out_path = out.map(PathBuf::from);
    let packed = tamias_publish::pack(Path::new(dir), out_path.as_deref())?;
    Ok(Outcome::Text(format!("packed → {}", packed.display())))
}

/// `tamias publish <dir> [registry]` — the shared `publish_plan`
/// resolution (`tamias-registry-open`, same as serve's `publish` op
/// and the CLI): named-registry + per-package limits apply inside
/// `open_registry`; the "publishing" line logs pre-open like before.
fn publish(
    ctx: &Ctx,
    dir: &str,
    registry: Option<&str>,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, Error> {
    let config = env_config(ctx);
    let plan = tamias_registry_open::publish_plan(
        Path::new(dir),
        &ctx.registry_root,
        Some(&config),
        registry,
        &[config.publish.default.as_deref()],
        &mut |manifest, target| {
            log(format!(
                "publishing {} → {}",
                manifest.name,
                target.unwrap_or("default (local root)")
            ));
        },
    )?;
    let target = plan.target.clone();
    // The TUI publish path stays unsigned — `publish --sign` is a CLI/
    // serve surface (key resolution is interactive-by-flag).
    let entry = plan.publish(None)?;
    Ok(Outcome::Text(format!(
        "published {} → {}",
        entry.label(),
        target.as_deref().unwrap_or("default")
    )))
}

/// The configured `registries` as `(name, kind)` pairs plus the
/// routing defaults — the publish picker's data and `:registries`'
/// report. Sorted (BTreeMap) for stable display.
pub fn registry_names(ctx: &Ctx) -> Vec<(String, String)> {
    let config = env_config(ctx);
    let mut out: Vec<(String, String)> = config
        .registries
        .iter()
        .map(|(name, reg)| (name.clone(), reg.effective_kind()))
        .collect();
    if let Some(d) = &config.publish.default {
        out.push((format!("publish.default → {d}"), String::new()));
    }
    if let Some(d) = &config.search.default {
        out.push((format!("search.default → {d}"), String::new()));
    }
    out
}

/// `:registries`/the Author pane's list — names + kinds, one per
/// line, defaults annotated.
fn registries(ctx: &Ctx) -> Result<Outcome, Error> {
    let rows = registry_names(ctx);
    if rows.is_empty() {
        return Ok(Outcome::Text("no registries configured".to_string()));
    }
    Ok(Outcome::Text(
        rows.iter()
            .map(|(name, kind)| {
                if kind.is_empty() {
                    name.clone()
                } else {
                    format!("{name} ({kind})")
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    ))
}

/// `journalctl` lines to tail for `logs` — same default as
/// `tamias logs` (`-n 200`).
const LOG_LINES: u32 = 200;

/// Forward one piped stream's lines to the UI as [`Msg::Log`] —
/// detached: it ends at EOF (or when the channel closes).
fn pump<R: Read + Send + 'static>(pipe: Option<R>, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let Some(pipe) = pipe else { return };
        for line in BufReader::new(pipe).lines() {
            match line {
                Ok(line) => {
                    if tx.send(Msg::Log(line)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Stream jobs — spawn the resolved child with piped stdout/stderr,
/// hand the killable [`Child`] back over [`Msg::StreamChild`], pump
/// both pipes into the log, poll `try_wait` (the lock is only held
/// for the poll itself, so `x`'s `kill` never blocks), and report the
/// exit as the [`Outcome`]. The env is scrubbed through
/// `tamias-shell` — the same policy `tamias run` applies.
///
/// # Errors
/// Spec/argv resolution, spawn, or wait failures.
pub fn stream(ctx: &Ctx, job: &Job, tx: &Sender<Msg>) -> Result<Outcome, Error> {
    let (program, args, workdir, env) = match job {
        Job::RunService { package, service } => {
            let spec =
                tamias_provider::exec_spec(&ctx.store_root, package, service.as_deref(), &[])?;
            let mut argv = spec.argv;
            let Some(program) = argv.first().cloned() else {
                return Err(Error::Failed(format!(
                    "{package}: provider service has an empty exec argv"
                )));
            };
            argv.remove(0);
            (program, argv, spec.workdir, spec.env)
        }
        Job::Logs { name, follow } => {
            // Same resolution as `tamias logs`: a store entry tails all
            // its provider units; anything else is a bare unit name.
            let units = match Store::new(&ctx.store_root).find(name) {
                Ok(entry) => tamias_provider::units_of(&entry)?,
                Err(_) => vec![name.clone()],
            };
            let args = tamias_deploy_systemd::journalctl_args(&units, LOG_LINES, *follow)?;
            (
                "journalctl".to_string(),
                args,
                PathBuf::from("."),
                Vec::new(),
            )
        }
        other => {
            return Err(Error::Failed(format!(
                "not a stream job: {}",
                other.label()
            )))
        }
    };
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(&workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    tamias_shell::scrub_env(&mut cmd, &env);
    let mut child = cmd.spawn().map_err(Error::io(format!("spawn {program}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    let _ = tx.send(Msg::StreamChild(child.clone()));
    pump(stdout, tx.clone());
    pump(stderr, tx.clone());
    let status = loop {
        // The mutex is held only for the poll — `kill_stream` locks
        // it briefly too, so a wedged child can't freeze the UI.
        let waited = child.lock().map(|mut c| c.try_wait());
        match waited {
            Ok(Ok(Some(status))) => break status,
            Ok(Ok(None)) => std::thread::sleep(Duration::from_millis(50)),
            Ok(Err(e)) => return Err(Error::io(format!("wait {program}"))(e)),
            Err(_) => return Err(Error::Failed(format!("{program}: child handle poisoned"))),
        }
    };
    let summary = if status.success() {
        format!("{} exited ok", job.label())
    } else {
        format!("{} exited {status}", job.label())
    };
    Ok(Outcome::Text(summary))
}

/// Dispatch one job — the worker thread's whole body. `log` receives
/// phase lines (install progress, warnings); the return is the
/// [`Outcome`] or the error (a bundled-driver consent refusal is just
/// an error here — the model detects it via
/// [`tamias_install::is_driver_consent_refusal`]).
pub fn run(ctx: &Ctx, job: &Job, log: &mut dyn FnMut(String)) -> Result<Outcome, Error> {
    match job {
        Job::Verify(name) => verify(ctx, name),
        Job::Remove(name) => remove(ctx, name, log),
        Job::Update {
            name,
            allow_drivers,
        } => update_one(ctx, name, *allow_drivers, log),
        Job::UpdateAll { allow_drivers } => update_all(ctx, *allow_drivers, log),
        Job::Pin { name, reference } => pin(ctx, name, reference),
        Job::CacheClean { all } => cache_clean(ctx, *all),
        Job::Doctor => doctor(ctx),
        Job::Search { query, target } => search(ctx, query, target.as_deref()),
        Job::Install {
            reference,
            allow_drivers,
        } => {
            let entry = install_ref(ctx, reference, *allow_drivers, log)?;
            Ok(Outcome::Text(format!(
                "installed {} ({})",
                entry.label(),
                short(&entry.record.digest)
            )))
        }
        Job::ServicesRefresh => services_refresh(ctx),
        Job::UnitAction { unit, action } => unit_action(unit, *action),
        Job::ProviderDeploy { name, config } => provider_deploy(ctx, name, config.as_deref(), log),
        Job::ProviderUninstall { name } => provider_uninstall(ctx, name, log),
        Job::RunService { .. } | Job::Logs { .. } => Err(Error::Failed(
            "stream jobs run through ops::stream, not ops::run".to_string(),
        )),
        Job::AddDep {
            reference,
            allow_drivers,
        } => add_dep(ctx, reference, *allow_drivers, log),
        Job::RemoveDep { reference } => remove_dep(reference),
        Job::SetDep {
            old,
            new,
            allow_drivers,
        } => set_dep(ctx, old, new, *allow_drivers, log),
        Job::Pack { dir, out } => pack(dir, out.as_deref()),
        Job::Publish { dir, registry } => publish(ctx, dir, registry.as_deref(), log),
        Job::Registries => registries(ctx),
    }
}

/// `sha256:<hex>` → first 12 hex chars — the shared
/// [`short_digest`] (same shortening the CLI/serve surfaces use).
fn short(digest: &str) -> &str {
    short_digest(digest)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn ctx(dir: &std::path::Path) -> Ctx {
        Ctx::new(
            dir.join("store"),
            dir.join("cache"),
            dir.join("registry"),
            None,
        )
    }

    fn entry(file: &str, digest: &str) -> RegistryEntry {
        RegistryEntry {
            name: "w".into(),
            scope: None,
            version: Some("1.0.0".into()),
            kind: Some("files".into()),
            digest: digest.into(),
            signature: None,
            file: file.into(),
        }
    }

    #[test]
    fn entry_ref_local_registry_is_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        let e = entry("w/1.0.0.tgz", "sha256:abc");
        // no target → the default local root; the ref is the .tgz path
        let r = entry_install_ref(&c, &e, None).unwrap();
        assert_eq!(r, c.registry_root.join("w/1.0.0.tgz").display().to_string());
        // a raw dir ref resolves the same way
        let other = tmp.path().join("other-reg");
        let r = entry_install_ref(&c, &e, Some(other.to_str().unwrap())).unwrap();
        assert_eq!(r, other.join("w/1.0.0.tgz").display().to_string());
    }

    #[test]
    fn entry_ref_http_registry_is_pinned() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        let hex = "a".repeat(64);
        let e = entry("w/1.0.0.tgz", &format!("sha256:{hex}"));
        let r = entry_install_ref(&c, &e, Some("http://reg.example.com/r")).unwrap();
        assert_eq!(
            r,
            format!("http://reg.example.com/r/w/1.0.0.tgz#sha256:{hex}")
        );
        // a malformed digest installs unpinned rather than failing
        let e2 = entry("w/1.0.0.tgz", "sha256:short");
        let r2 = entry_install_ref(&c, &e2, Some("http://reg.example.com/r")).unwrap();
        assert_eq!(r2, "http://reg.example.com/r/w/1.0.0.tgz");
    }

    #[test]
    fn verify_and_clean_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        // verify on a missing package errors like serve's op
        assert!(run(&c, &Job::Verify("ghost".into()), &mut |_| {}).is_err());
        // cache clean on an empty cache reports zero evictions
        let out = run(&c, &Job::CacheClean { all: true }, &mut |_| {}).unwrap();
        match out {
            Outcome::Text(t) => assert!(t.contains('0')),
            _ => panic!("expected text"),
        }
    }
}
