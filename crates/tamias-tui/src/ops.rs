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
    /// `tamias sync` — reconcile the project dep set against the
    /// store + `tamias.lock` (install missing/drifted, never removes).
    /// `from_lock` treats `tamias.lock` itself as the spec (`:sync
    /// --from-lock`).
    Sync {
        /// Bundled-driver consent.
        allow_drivers: bool,
        /// Install `tamias.lock` entries verbatim instead of the
        /// manifest's `deps:`.
        from_lock: bool,
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
            Self::Pin { name, reference } => format!("pin {name} → {reference}"),
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
            Self::Sync { from_lock, .. } => {
                if *from_lock {
                    "sync --from-lock".to_string()
                } else {
                    "sync deps".to_string()
                }
            }
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

    /// Does this job mutate the store or provider state the panes
    /// show? `Done` uses it to decide the post-job store `reload` +
    /// services-refresh — read-only jobs (verify, doctor, search,
    /// registries, cache clean, pack, publish, stream exits,
    /// services refresh itself) skip both.
    #[must_use]
    pub fn mutates(&self) -> bool {
        matches!(
            self,
            Self::Install { .. }
                | Self::Remove(_)
                | Self::Update { .. }
                | Self::UpdateAll { .. }
                | Self::Pin { .. }
                | Self::AddDep { .. }
                | Self::RemoveDep { .. }
                | Self::SetDep { .. }
                | Self::Sync { .. }
                | Self::ProviderDeploy { .. }
                | Self::ProviderUninstall { .. }
                | Self::UnitAction { .. }
        )
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
            Self::Sync {
                allow_drivers: _,
                from_lock,
            } => Self::Sync {
                allow_drivers: true,
                from_lock,
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
pub(crate) fn unit_action_verb(action: tamias_provider::UnitAction) -> &'static str {
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
            Progress::Including { base } => log(format!("including {base}")),
            Progress::DriverAction { action } => log(format!("driver: {action}")),
            Progress::BinsDeployed { actions, warnings } => {
                for warning in warnings {
                    log(format!("warning: {warning}"));
                }
                for action in actions {
                    log(action);
                }
            }
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
            "{}: digest mismatch — recorded {}, computed {} — `u`/`U` reinstalls",
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
            "{}: signature verification failed — check `signing.allowed_signers` in tamias.yml",
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
    match ro::open_registry(
        &ctx.registry_root,
        Some(&config),
        target.as_deref(),
        None,
        None,
    )
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
        tamias_registry_open::search(&ctx.registry_root, Some(&config), target, query).map_err(
            |e| {
                Error::Failed(format!(
                    "{e} — `:registry <name>` switches target · `:doctor` diagnoses"
                ))
            },
        )?;
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

/// Sibling versions of a registry page entry — the index's
/// `entries()` filtered to the same `(name, scope)`, in index order
/// (the local adapter sorts by scope/name/version; http registries
/// return their own order — both keep every retained version, so no
/// `search` substring query is needed). `target` is the `:registry`
/// override; `None` → the same `search.default` → `publish.default`
/// → local-root routing [`search`] uses, so the list always comes
/// from the registry the page's install ref resolved against.
///
/// This is a plain read for the detail page's dedicated fetch
/// thread — deliberately not a [`Job`]: it must never occupy `busy`
/// or queue ahead of a user op.
///
/// # Errors
/// Fails on registry resolution or the index read.
pub fn versions(
    ctx: &Ctx,
    name: &str,
    scope: Option<&str>,
    target: Option<&str>,
) -> Result<Vec<RegistryEntry>, Error> {
    let config = env_config(ctx);
    let value = ro::registry_target(
        target,
        &[
            config.search.default.as_deref(),
            config.publish.default.as_deref(),
        ],
    );
    let registry = ro::open_registry(
        &ctx.registry_root,
        Some(&config),
        value.as_deref(),
        None,
        None,
    )?;
    Ok(registry
        .entries()?
        .into_iter()
        .filter(|e| e.name == name && e.scope.as_deref() == scope)
        .collect())
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

/// The project manifest the Project pane edits — `./tamias.yaml`
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
            "tamias.toml found — the deps form edits YAML manifests only \
             (rename it or add tamias.yaml)"
                .to_string(),
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

/// `tamias sync` — the shared [`tamias_install::sync_project`] over
/// the project the Project pane edits (same [`project_manifest`]
/// discovery, so `tamias.toml`-only projects get the same refusal).
/// `from_lock` swaps the spec to `tamias.lock` itself — the manifest
/// isn't even required then. Per-dep outcomes stream to the log; any
/// failure is an error after the lines are logged (consent refusals
/// already propagated through `is_driver_consent_refusal` upstream).
fn sync_deps(
    ctx: &Ctx,
    allow_drivers: bool,
    from_lock: bool,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, Error> {
    // `--from-lock` needs no manifest at all — `tamias.lock` in the
    // cwd is the whole spec (a `tamias.toml`-only project syncs fine).
    let dir = if from_lock {
        PathBuf::from(".")
    } else {
        let (path, _manifest) = project_manifest()?;
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let report = tamias_install::sync_project(
        &install_env(ctx),
        &dir,
        Surface::Tui,
        tamias_install::SyncOptions {
            allow_drivers,
            locked: false,
            source: if from_lock {
                tamias_install::SyncSource::Lock
            } else {
                tamias_install::SyncSource::Deps
            },
        },
        &mut |p| match p {
            Progress::Fetching { reference } => log(format!("fetching {reference}")),
            Progress::Installing { label } => log(format!("installing {label}")),
            Progress::Including { base } => log(format!("including {base}")),
            Progress::DriverAction { action } => log(format!("driver: {action}")),
            Progress::BinsDeployed { actions, warnings } => {
                for warning in warnings {
                    log(format!("warning: {warning}"));
                }
                for action in actions {
                    log(action);
                }
            }
            Progress::Done { label } => log(format!("done — {label}")),
        },
    )?;
    for o in &report.outcomes {
        log(format!("{} {} — {}", o.status.as_str(), o.dep, o.detail));
    }
    if report.failed() > 0 {
        return Err(Error::Failed(format!(
            "sync: {} dep(s) failed — see log",
            report.failed()
        )));
    }
    Ok(Outcome::Text(if from_lock {
        format!(
            "sync --from-lock: {} dep(s) reconciled",
            report.outcomes.len()
        )
    } else {
        format!("sync: {} dep(s) reconciled", report.outcomes.len())
    }))
}

/// `tamias bundle pack <dir> [out]` — `tamias-publish`'s pack.
fn pack(dir: &str, out: Option<&str>, ctx: &Ctx) -> Result<Outcome, Error> {
    let out_path = out.map(PathBuf::from);
    // Default flatten — `include:` bases resolve read-only against the
    // store; the TUI has no keep-includes toggle.
    let packed = tamias_publish::pack(
        Path::new(dir),
        out_path.as_deref(),
        tamias_publish::Includes::Flatten,
        &ctx.store_root,
    )?;
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
        &ctx.store_root,
        false,
        None,
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
    // serve surface (key resolution is interactive-by-flag). Retention
    // resolves from config (`publish --keep` is a CLI/serve flag).
    let outcome = plan.publish(None)?;
    let mut text = format!(
        "published {} → {}",
        outcome.label(),
        target.as_deref().unwrap_or("default")
    );
    if !outcome.pruned.is_empty() {
        text.push_str(&format!("; pruned: {}", outcome.pruned.join(", ")));
    }
    Ok(Outcome::Text(text))
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

/// One row of the Publish pane's registries table — a configured
/// `registries:` entry or a routing default. Richer than the
/// `(name, kind)` pairs [`registry_names`] returns for `:` completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryRow {
    pub name: String,
    /// `local`/`http`/`cf` — empty on the synthetic default rows.
    pub kind: String,
    /// `url` (or the resolved root for url-less locals); the pointed-at
    /// registry name on the `*.default` rows.
    pub target: String,
}

/// The Publish pane's registries table — each configured registry
/// with its kind and url/root target, plus the `publish.default`/
/// `search.default` routing rows (blank kind, target = the registry
/// they point at). Same BTreeMap ordering as [`registry_names`].
pub fn registry_rows(ctx: &Ctx) -> Vec<RegistryRow> {
    let config = env_config(ctx);
    let mut out: Vec<RegistryRow> = config
        .registries
        .iter()
        .map(|(name, reg)| {
            let kind = reg.effective_kind();
            RegistryRow {
                name: name.clone(),
                kind: kind.clone(),
                // Url-less locals resolve to the default registry root;
                // http/cf without a url is a config error `open_registry`
                // reports — the table just shows an empty target.
                target: reg.url.clone().unwrap_or_else(|| {
                    if kind == "local" {
                        ctx.registry_root.display().to_string()
                    } else {
                        String::new()
                    }
                }),
            }
        })
        .collect();
    if let Some(d) = &config.publish.default {
        out.push(RegistryRow {
            name: "publish.default".into(),
            kind: String::new(),
            target: d.clone(),
        });
    }
    if let Some(d) = &config.search.default {
        out.push(RegistryRow {
            name: "search.default".into(),
            kind: String::new(),
            target: d.clone(),
        });
    }
    out
}

/// `:registries`/the Publish pane's list — names + kinds, one per
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
            let (program, args) =
                tamias_provider::ServiceBackend::new().log_stream(&units, LOG_LINES, *follow)?;
            (program, args, PathBuf::from("."), Vec::new())
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
            let trust = entry
                .record
                .trust
                .as_deref()
                .map_or_else(String::new, |t| format!(" — trust: {t}"));
            Ok(Outcome::Text(format!(
                "installed {} ({}){trust}",
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
        Job::Sync {
            allow_drivers,
            from_lock,
        } => sync_deps(ctx, *allow_drivers, *from_lock, log),
        Job::Pack { dir, out } => pack(dir, out.as_deref(), ctx),
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
    use crate::testkit::EnvGuard;

    fn ctx(dir: &std::path::Path) -> Ctx {
        // A nonexistent explicit config file → `load_config` returns
        // defaults, so the developer's real `~/.config/tamias` (or a
        // stray `TAMIAS_CONFIG`) can't leak registry routing into the
        // assertions below.
        Ctx::new(
            dir.join("store"),
            dir.join("cache"),
            dir.join("registry"),
            Some(dir.join("no-such-config.yml")),
        )
    }

    fn entry(file: &str, digest: &str) -> RegistryEntry {
        RegistryEntry {
            name: "w".into(),
            scope: None,
            version: Some("1.0.0".into()),
            kind: Some("files".into()),
            description: None,
            keywords: Vec::new(),
            homepage: None,
            repository: None,
            license: None,
            authors: Vec::new(),
            digest: digest.into(),
            signature: None,
            publisher: None,
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
    fn versions_filters_to_name_and_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        // A hand-written local index — `entries()` reads it verbatim,
        // every retained version included (no publish needed).
        std::fs::create_dir_all(&c.registry_root).unwrap();
        std::fs::write(
            c.registry_root.join("index.json"),
            r#"[
                {"name":"w","version":"0.1.0","digest":"sha256:a","file":"w/0.1.0.tgz"},
                {"name":"w","version":"0.2.0","digest":"sha256:b","file":"w/0.2.0.tgz"},
                {"name":"w","scope":"s","version":"1.0.0","digest":"sha256:c","file":"@s/w/1.0.0.tgz"},
                {"name":"other","version":"1.0.0","digest":"sha256:d","file":"other/1.0.0.tgz"}
            ]"#,
        )
        .unwrap();
        // Unscoped `w` → both unscoped versions, index order; the
        // scoped twin and `other` stay out.
        let list = versions(&c, "w", None, None).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].version.as_deref(), Some("0.1.0"));
        assert_eq!(list[1].version.as_deref(), Some("0.2.0"));
        let scoped = versions(&c, "w", Some("s"), None).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].file, "@s/w/1.0.0.tgz");
        assert!(versions(&c, "ghost", None, None).unwrap().is_empty());
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

    // --- end-to-end job coverage ---

    /// A payload dir whose `tamias.yaml` is `manifest` verbatim plus
    /// one payload file.
    fn pkg(dir: &Path, name: &str, manifest: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("tamias.yaml"), manifest).unwrap();
        std::fs::write(p.join("a.txt"), "a").unwrap();
        p
    }

    /// The record `Store::install` writes for a hand-placed entry —
    /// `source_ref` points at `payload` so `update`/`sync` can
    /// re-install it.
    fn record(name: &str, payload: &Path, kind: Option<&str>) -> StoreRecord {
        StoreRecord {
            name: name.into(),
            scope: None,
            version: Some("1.0.0".into()),
            kind: kind.map(str::to_string),
            description: None,
            keywords: Vec::new(),
            homepage: None,
            repository: None,
            license: None,
            authors: Vec::new(),
            source_ref: payload.display().to_string(),
            resolved: "sha256:0123456789abcdef".into(),
            digest: String::new(),
            signature: None,
            signer: None,
            trust: None,
            installed_at: 0,
        }
    }

    /// Place `payload` in `c`'s store directly (no pipeline) — the
    /// fast fixture for ops that only need an entry to exist.
    fn place(c: &Ctx, payload: &Path, name: &str, kind: Option<&str>) -> StoreEntry {
        Store::new(&c.store_root)
            .install(payload, record(name, payload, kind))
            .unwrap()
    }

    /// `run` expecting `Outcome::Text` — panics with the error
    /// otherwise.
    fn text(ctx: &Ctx, job: &Job) -> String {
        match run(ctx, job, &mut |_| {}) {
            Ok(Outcome::Text(t)) => t,
            other => panic!("{job:?} → {other:?}"),
        }
    }

    /// install → verify → tamper → update → update-all → pin →
    /// remove, including the warn arms (siblings, missing driver).
    /// `tamias.lock` lands in the cwd — the guard owns it.
    #[test]
    fn install_verify_update_pin_remove_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = ctx(tmp.path());
        let w = pkg(tmp.path(), "w", "name: w\nversion: 1.0.0\n");
        let w_ref = w.display().to_string();

        // install through the real pipeline — progress lines stream
        let mut lines = Vec::new();
        let out = run(
            &c,
            &Job::Install {
                reference: w_ref.clone(),
                allow_drivers: false,
            },
            &mut |l| lines.push(l),
        )
        .unwrap();
        let Outcome::Text(t) = out else {
            panic!("expected text")
        };
        assert!(t.contains("installed w@1.0.0"), "{t}");
        assert!(lines.iter().any(|l| l.contains("fetching")));
        assert!(lines.iter().any(|l| l.contains("installing")));
        assert!(lines.iter().any(|l| l.contains("done")));
        assert_eq!(Store::new(&c.store_root).entries().unwrap().len(), 1);

        // verify — clean, then tampered, then restored
        let t = text(&c, &Job::Verify("w".into()));
        assert!(t.contains("w@1.0.0: ok"), "{t}");
        assert!(t.contains("signature:"), "{t}");
        let payload_file = c.store_root.join("w@1.0.0/a.txt");
        std::fs::write(&payload_file, "b").unwrap();
        let err = run(&c, &Job::Verify("w".into()), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("digest mismatch"), "{err}");
        std::fs::write(&payload_file, "a").unwrap();
        assert!(run(&c, &Job::Verify("w".into()), &mut |_| {}).is_ok());

        // a second installable entry for update-all; a third placed
        // entry with a non-files kind for the missing-driver warn
        let w2 = pkg(tmp.path(), "w2", "name: w2\nversion: 2.0.0\n");
        place(&c, &w2, "w2", None);

        // update --all re-installs every recorded ref
        let t = text(
            &c,
            &Job::UpdateAll {
                allow_drivers: false,
            },
        );
        assert!(t.contains("updated 2 package(s)"), "{t}");
        // empty-store arm
        let tmp2 = tempfile::tempdir().unwrap();
        let c2 = ctx(tmp2.path());
        assert_eq!(
            text(
                &c2,
                &Job::UpdateAll {
                    allow_drivers: false
                }
            ),
            "nothing installed"
        );

        // update one + pin — both sync tamias.lock in the cwd
        let t = text(
            &c,
            &Job::Update {
                name: "w".into(),
                allow_drivers: false,
            },
        );
        assert!(t.contains("updated w@1.0.0"), "{t}");
        // pin retargets + syncs an existing lock — seed one first so
        // `Lock::sync_record` takes its update path (it never creates
        // the file; that's `add`'s job)
        let rec = Store::new(&c.store_root).find("w").unwrap().record;
        Lock::upsert_record(Path::new("tamias.lock"), &rec).unwrap();
        let t = text(
            &c,
            &Job::Pin {
                name: "w".into(),
                reference: "./elsewhere".into(),
            },
        );
        assert!(t.contains("pinned w → ./elsewhere"), "{t}");
        let lock = std::fs::read_to_string(tmp.path().join("tamias.lock")).unwrap();
        assert!(lock.contains("./elsewhere"), "{lock}");
        // update on a missing entry errors
        assert!(run(
            &c,
            &Job::Update {
                name: "ghost".into(),
                allow_drivers: false
            },
            &mut |_| {}
        )
        .is_err());

        // remove — the non-files kind warns about leftover provider
        // resources; a same-name sibling warns and skips dispatch.
        // (`odd` is placed *after* update-all: the pipeline rewrites
        // the record from the manifest, dropping the fixture `kind`.)
        let odd = pkg(tmp.path(), "odd", "name: odd\nversion: 1.0.0\n");
        place(&c, &odd, "odd", Some("weird"));
        let mut warns = Vec::new();
        let out = run(&c, &Job::Remove("odd".into()), &mut |l| warns.push(l)).unwrap();
        assert!(matches!(out, Outcome::Text(ref t) if t.contains("removed odd")));
        assert!(
            warns
                .iter()
                .any(|l| l.contains("no driver for type 'weird'")),
            "{warns:?}"
        );
        // second version of `w` — removing it leaves w@1.0.0 behind
        let w3 = pkg(tmp.path(), "w3", "name: w\nversion: 3.0.0\n");
        let mut rec = record("w", &w3, None);
        rec.version = Some("3.0.0".into());
        Store::new(&c.store_root).install(&w3, rec).unwrap();
        let mut warns = Vec::new();
        run(&c, &Job::Remove("w@3.0.0".into()), &mut |l| warns.push(l)).unwrap();
        assert!(
            warns.iter().any(|l| l.contains("sibling installs remain")),
            "{warns:?}"
        );
        assert!(run(&c, &Job::Remove("ghost".into()), &mut |_| {}).is_err());
    }

    /// `project_manifest` discovery order + the dep-form jobs
    /// (`add`/`remove`/`set`/`sync`) against a real cwd manifest.
    #[test]
    fn project_manifest_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        // nothing → the default tamias.yaml
        let (path, m) = project_manifest().unwrap();
        assert_eq!(path, PathBuf::from("tamias.yaml"));
        assert!(m.deps.is_empty());
        // a lone tamias.toml is refused — the form edits YAML only
        std::fs::write(tmp.path().join("tamias.toml"), "[project]\n").unwrap();
        assert!(project_manifest().unwrap_err().to_string().contains("YAML"));
        std::fs::remove_file(tmp.path().join("tamias.toml")).unwrap();
        // yml is picked up; yaml wins when both exist
        std::fs::write(tmp.path().join("tamias.yml"), "name: y\ndeps: []\n").unwrap();
        let (path, _) = project_manifest().unwrap();
        assert_eq!(path, PathBuf::from("tamias.yml"));
        std::fs::write(tmp.path().join("tamias.yaml"), "name: x\ndeps: []\n").unwrap();
        let (path, m) = project_manifest().unwrap();
        assert_eq!(path, PathBuf::from("tamias.yaml"));
        assert_eq!(m.name, "x");
    }

    /// add/remove/set dep + sync — the manifest+lock write path.
    #[test]
    fn dep_form_ops() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let _g = EnvGuard::enter(&proj);
        let c = ctx(tmp.path());
        let dep = pkg(tmp.path(), "dep", "name: dep\nversion: 1.0.0\n");
        let dep2 = pkg(tmp.path(), "dep2", "name: dep2\nversion: 1.0.0\n");
        let dep_ref = dep.display().to_string();
        let dep2_ref = dep2.display().to_string();
        std::fs::write(
            proj.join("tamias.yaml"),
            "name: proj\nversion: 0.1.0\ndeps: []\n",
        )
        .unwrap();

        // add — installs, records the dep, upserts tamias.lock
        let t = text(
            &c,
            &Job::AddDep {
                reference: dep_ref.clone(),
                allow_drivers: false,
            },
        );
        assert!(t.contains("added") && t.contains("tamias.yaml"), "{t}");
        let (_, m) = project_manifest().unwrap();
        assert_eq!(m.deps, [dep_ref.as_str()]);
        assert!(proj.join("tamias.lock").is_file());
        // re-adding dedups the deps entry
        text(
            &c,
            &Job::AddDep {
                reference: dep_ref.clone(),
                allow_drivers: false,
            },
        );
        let (_, m) = project_manifest().unwrap();
        assert_eq!(m.deps.len(), 1);

        // remove — missing ref errors; present ref drops manifest-only
        assert!(run(
            &c,
            &Job::RemoveDep {
                reference: "nope".into()
            },
            &mut |_| {}
        )
        .is_err());
        let t = text(
            &c,
            &Job::RemoveDep {
                reference: dep_ref.clone(),
            },
        );
        assert!(t.contains("removed dep"), "{t}");
        let (_, m) = project_manifest().unwrap();
        assert!(m.deps.is_empty());

        // set — identical refs and missing olds error; a real swap
        // installs the new ref first, then rewrites deps + lock
        assert!(run(
            &c,
            &Job::SetDep {
                old: dep_ref.clone(),
                new: dep_ref.clone(),
                allow_drivers: false
            },
            &mut |_| {}
        )
        .is_err());
        assert!(run(
            &c,
            &Job::SetDep {
                old: "ghost".into(),
                new: dep2_ref.clone(),
                allow_drivers: false
            },
            &mut |_| {}
        )
        .is_err());
        // re-add dep, then swap it for dep2 — dep2 is already listed?
        // no: deps is empty now, so set still needs `old` present.
        text(
            &c,
            &Job::AddDep {
                reference: dep_ref.clone(),
                allow_drivers: false,
            },
        );
        let t = text(
            &c,
            &Job::SetDep {
                old: dep_ref.clone(),
                new: dep2_ref.clone(),
                allow_drivers: false,
            },
        );
        assert!(t.contains("dep"), "{t}");
        let (_, m) = project_manifest().unwrap();
        assert_eq!(m.deps, [dep2_ref.as_str()]);

        // sync — deps already installed reconcile cleanly; --from-lock
        // reads tamias.lock verbatim
        let t = text(
            &c,
            &Job::Sync {
                allow_drivers: false,
                from_lock: false,
            },
        );
        assert!(t.contains("sync:"), "{t}");
        let t = text(
            &c,
            &Job::Sync {
                allow_drivers: false,
                from_lock: true,
            },
        );
        assert!(t.contains("sync --from-lock:"), "{t}");
        // a dep that can't install fails the sync after logging
        std::fs::write(
            proj.join("tamias.yaml"),
            "name: proj\nversion: 0.1.0\ndeps: [/nonexistent-tamias-cov-dep]\n",
        )
        .unwrap();
        let err = run(
            &c,
            &Job::Sync {
                allow_drivers: false,
                from_lock: false,
            },
            &mut |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("dep(s) failed"), "{err}");
        // --from-lock with no lock file → an empty lock, zero deps
        let proj2 = tmp.path().join("proj2");
        std::fs::create_dir_all(&proj2).unwrap();
        std::env::set_current_dir(&proj2).unwrap();
        let t = text(
            &c,
            &Job::Sync {
                allow_drivers: false,
                from_lock: true,
            },
        );
        assert!(t.contains("0 dep(s)"), "{t}");
    }

    /// pack/publish/registries + the cache-clean/doctor/search jobs.
    #[test]
    fn authoring_and_read_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        let pkgdir = pkg(
            tmp.path(),
            "authorable",
            "name: authorable\nversion: 1.0.0\n",
        );

        // pack → a real .tgz
        let out_path = tmp.path().join("out.tgz");
        let t = text(
            &c,
            &Job::Pack {
                dir: pkgdir.display().to_string(),
                out: Some(out_path.display().to_string()),
            },
        );
        assert!(t.contains("packed") && out_path.is_file(), "{t}");
        assert!(run(
            &c,
            &Job::Pack {
                dir: "/nonexistent-tamias-cov".into(),
                out: None
            },
            &mut |_| {}
        )
        .is_err());

        // publish → the local registry root gains an index entry the
        // search job then finds
        let t = text(
            &c,
            &Job::Publish {
                dir: pkgdir.display().to_string(),
                registry: None,
            },
        );
        assert!(t.contains("published authorable"), "{t}");
        assert!(c.registry_root.join("index.json").is_file());
        match run(
            &c,
            &Job::Search {
                query: "authorable".into(),
                target: None,
            },
            &mut |_| {},
        )
        .unwrap()
        {
            Outcome::Search { results, .. } => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].name, "authorable");
            }
            other => panic!("expected search outcome: {other:?}"),
        }
        // publish failure — no manifest in the dir
        let empty = tempfile::tempdir().unwrap();
        assert!(run(
            &c,
            &Job::Publish {
                dir: empty.path().display().to_string(),
                registry: None
            },
            &mut |_| {}
        )
        .is_err());
        // registries — no config → the empty report
        assert_eq!(text(&c, &Job::Registries), "no registries configured");

        // cache clean — both windows on an empty cache
        for all in [false, true] {
            let t = text(&c, &Job::CacheClean { all });
            assert!(t.contains("evicted 0"), "{t}");
        }

        // doctor — all four checks ok on a healthy fixture
        let t = text(&c, &Job::Doctor);
        for needle in ["store:", "cache:", "registry:", "drivers:"] {
            assert!(t.contains(needle), "{t}");
        }
        assert!(t.contains("ok "), "{t}");

        // doctor failure arms — unwritable store dir + unparseable
        // registry index each fail a check
        let c_bad = ctx(&tmp.path().join("bad"));
        std::fs::create_dir_all(&c_bad.store_root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&c_bad.store_root, std::fs::Permissions::from_mode(0o555))
                .unwrap();
            let err = run(&c_bad, &Job::Doctor, &mut |_| {})
                .unwrap_err()
                .to_string();
            assert!(err.contains("check(s) failed"), "{err}");
            std::fs::set_permissions(&c_bad.store_root, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::create_dir_all(&c_bad.registry_root).unwrap();
        std::fs::write(c_bad.registry_root.join("index.json"), "not json").unwrap();
        let err = run(&c_bad, &Job::Doctor, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("registry"), "{err}");
        // search against the broken index errors too
        assert!(run(
            &c_bad,
            &Job::Search {
                query: "x".into(),
                target: None
            },
            &mut |_| {}
        )
        .is_err());
        // a broken config file warns and falls back to defaults —
        // doctor still runs (the env_config error arm)
        let bad_cfg = tmp.path().join("bad.yml");
        std::fs::write(&bad_cfg, "registries: [unclosed\n").unwrap();
        let c_badcfg = Ctx::new(
            tmp.path().join("s2"),
            tmp.path().join("c2"),
            tmp.path().join("r2"),
            Some(bad_cfg),
        );
        assert!(run(&c_badcfg, &Job::Doctor, &mut |_| {}).is_ok());
    }

    /// Named-registry routing: `entry_install_ref`'s kind arms,
    /// `registry_names`, `versions` with a target override.
    #[test]
    fn named_registry_routing() {
        let tmp = tempfile::tempdir().unwrap();
        let localdir = tmp.path().join("localdir");
        std::fs::create_dir_all(&localdir).unwrap();
        std::fs::write(
            localdir.join("index.json"),
            r#"[{"name":"w","version":"1.0.0","digest":"sha256:a","file":"w/1.0.0.tgz"}]"#,
        )
        .unwrap();
        let cfg = tmp.path().join("tamias.yml");
        std::fs::write(
            &cfg,
            format!(
                "registries:\n  \
                 localdir:\n    kind: local\n    url: {}\n  \
                 localnou:\n    kind: local\n  \
                 httpr:\n    kind: http\n    url: http://reg.example.com/r\n  \
                 cfreg:\n    kind: cf\n    url: https://cf.example.com\n  \
                 weird:\n    kind: bogus\n    url: ./rel\n  \
                 nourl:\n    kind: http\npublish:\n  default: httpr\nsearch:\n  default: localdir\n",
                localdir.display()
            ),
        )
        .unwrap();
        let c = Ctx::new(
            tmp.path().join("store"),
            tmp.path().join("cache"),
            tmp.path().join("registry"),
            Some(cfg),
        );
        let hex = "b".repeat(64);
        let e = entry("w/1.0.0.tgz", &format!("sha256:{hex}"));

        // named local with a url → that dir; without → registry_root
        let r = entry_install_ref(&c, &e, Some("localdir")).unwrap();
        assert_eq!(r, localdir.join("w/1.0.0.tgz").display().to_string());
        let r = entry_install_ref(&c, &e, Some("localnou")).unwrap();
        assert_eq!(r, c.registry_root.join("w/1.0.0.tgz").display().to_string());
        // http + cf share the Http base — digest pins the url
        for name in ["httpr", "cfreg"] {
            let r = entry_install_ref(&c, &e, Some(name)).unwrap();
            assert!(r.ends_with(&format!("w/1.0.0.tgz#sha256:{hex}")), "{r}");
        }
        // an unrecognized kind falls back to ref-grammar parsing
        let r = entry_install_ref(&c, &e, Some("weird")).unwrap();
        assert_eq!(
            r,
            PathBuf::from("./rel")
                .join("w/1.0.0.tgz")
                .display()
                .to_string()
        );
        // http/cf without a url is a hard error
        assert!(entry_install_ref(&c, &e, Some("nourl")).is_err());

        // registry_names — sorted names+kinds plus the routing defaults
        let names = registry_names(&c);
        let got: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
        for want in [
            "cfreg",
            "httpr",
            "localdir",
            "localnou",
            "nourl",
            "publish.default → httpr",
            "search.default → localdir",
            "weird",
        ] {
            assert!(got.contains(&want), "{got:?}");
        }
        assert!(names.iter().any(|(n, k)| n == "httpr" && k == "http"));
        // :registries report renders name (kind) lines
        let t = text(&c, &Job::Registries);
        assert!(t.contains("httpr (http)"), "{t}");
        assert!(t.contains("publish.default → httpr"), "{t}");

        // registry_rows — the Publish pane's table: kind + target per
        // entry, routing defaults as blank-kind rows pointing at their
        // target. Url-less locals resolve to the registry root.
        let rows = registry_rows(&c);
        let row = |name: &str| rows.iter().find(|r| r.name == name).unwrap();
        assert_eq!(row("httpr").kind, "http");
        assert_eq!(row("httpr").target, "http://reg.example.com/r");
        assert_eq!(row("localdir").target, localdir.display().to_string());
        assert_eq!(
            row("localnou").target,
            c.registry_root.display().to_string()
        );
        assert_eq!(row("publish.default").kind, "");
        assert_eq!(row("publish.default").target, "httpr");
        assert_eq!(row("search.default").target, "localdir");

        // versions against the named target reads that registry's index
        let list = versions(&c, "w", None, Some("localdir")).unwrap();
        assert_eq!(list.len(), 1);
        // and the default routing (search.default → localdir) agrees
        let list = versions(&c, "w", None, None).unwrap();
        assert_eq!(list.len(), 1);
    }

    /// Provider lifecycle: services refresh, deploy, uninstall, unit
    /// actions (real `systemd --user` when available), and the
    /// `run`/`logs` stream jobs.
    #[test]
    fn provider_lifecycle_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = ctx(tmp.path());
        // a provider package — one good service + ingress hooks so
        // deploy's warning arm fires too
        let prov = pkg(
            tmp.path(),
            "prov",
            "name: prov\nversion: 1.0.0\nprovider:\n  name: provprov\n  \
             services:\n    - name: web\n      exec: \"/bin/echo hi-from-service\"\n  \
             ingress:\n    - public_url: https://ci.example.com\n      path: /hook\n      \
             hooks:\n        - forge: github\n          repo: o/r\n          events: [push]\n",
        );
        place(&c, &prov, "prov", None);
        // a second provider pkg whose services can't exec — the
        // stream jobs' error arms
        let svc = pkg(
            tmp.path(),
            "svc",
            "name: svc\nversion: 1.0.0\nprovider:\n  name: svcp\n  \
             services:\n    - name: web\n      exec: \"/bin/echo svc-web\"\n    - name: void\n      exec: \"''\"\n    - name: bad\n      exec: \"/nonexistent-tamias-cov-bin\"\n",
        );
        place(&c, &svc, "svc", None);
        // a provider-less package — `run`/`logs` error arms
        let plain = pkg(tmp.path(), "plain", "name: plain\nversion: 1.0.0\n");
        place(&c, &plain, "plain", None);

        // services refresh — rows for both packages' services
        match run(&c, &Job::ServicesRefresh, &mut |_| {}).unwrap() {
            Outcome::Services(rows) => {
                assert!(rows.iter().any(|r| r.package == "prov@1.0.0"), "{rows:?}");
                assert!(rows.iter().any(|r| r.unit == "provprov-web"), "{rows:?}");
            }
            other => panic!("expected services: {other:?}"),
        }
        // empty store → empty rows
        let c_empty = ctx(&tmp.path().join("empty"));
        match run(&c_empty, &Job::ServicesRefresh, &mut |_| {}).unwrap() {
            Outcome::Services(rows) => assert!(rows.is_empty()),
            _ => panic!("expected services"),
        }

        // deploy — writes the unit under the guarded XDG dir and
        // reports the ingress-hook warning
        let mut lines = Vec::new();
        let out = run(
            &c,
            &Job::ProviderDeploy {
                name: "prov".into(),
                config: None,
            },
            &mut |l| lines.push(l),
        )
        .unwrap();
        assert!(matches!(out, Outcome::Text(ref t) if t.contains("deploy prov done")));
        assert!(
            tmp.path()
                .join("xdg/systemd/user/provprov-web.service")
                .is_file(),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("ingress hook")),
            "{lines:?}"
        );
        // deploy errors — missing package (the config arg rides along)
        assert!(run(
            &c,
            &Job::ProviderDeploy {
                name: "ghost".into(),
                config: None
            },
            &mut |_| {}
        )
        .is_err());
        assert!(run(
            &c_empty,
            &Job::ProviderDeploy {
                name: "ghost".into(),
                config: Some("/dev/null".into()),
            },
            &mut |_| {}
        )
        .is_err());

        // unit action — error path always; the success path rides a
        // real transient `systemd-run --user` unit when the user bus
        // answers (skipped silently elsewhere)
        assert!(run(
            &c,
            &Job::UnitAction {
                unit: "tamias-cov-no-such-unit".into(),
                action: tamias_provider::UnitAction::Start,
            },
            &mut |_| {}
        )
        .is_err());
        let unit = format!("tamias-cov-{}", std::process::id());
        // a sleep keeps the transient unit alive long enough for the
        // start/stop round-trip (`/bin/true` exits before `start` lands)
        let spawned = std::process::Command::new("systemd-run")
            .args(["--user", "--unit", &unit, "/bin/sleep", "60"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if spawned {
            // the transient unit exists — start/stop really run
            let out = run(
                &c,
                &Job::UnitAction {
                    unit: unit.clone(),
                    action: tamias_provider::UnitAction::Start,
                },
                &mut |_| {},
            );
            assert!(out.is_ok(), "{out:?}");
            let t = text(
                &c,
                &Job::UnitAction {
                    unit: unit.clone(),
                    action: tamias_provider::UnitAction::Stop,
                },
            );
            assert!(t.contains("stop"), "{t}");
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "stop", &unit])
                .status();
        }

        // uninstall — provider teardown + store drop; the unit file
        // the deploy wrote is gone afterwards
        let out = run(
            &c,
            &Job::ProviderUninstall {
                name: "prov".into(),
            },
            &mut |_| {},
        )
        .unwrap();
        assert!(matches!(out, Outcome::Text(ref t) if t.contains("uninstalled prov")));
        assert!(!tmp
            .path()
            .join("xdg/systemd/user/provprov-web.service")
            .exists());
        assert!(Store::new(&c.store_root).find("prov").is_err());
        assert!(run(
            &c,
            &Job::ProviderUninstall {
                name: "ghost".into()
            },
            &mut |_| {}
        )
        .is_err());

        // --- stream jobs ---
        // run <pkg> <svc> — echo exits immediately; its line pumps
        // through Msg::Log and the StreamChild handle arrives
        let (tx, rx) = std::sync::mpsc::channel();
        let out = stream(
            &c,
            &Job::RunService {
                package: "svc".into(),
                service: Some("web".into()),
            },
            &tx,
        )
        .unwrap();
        assert!(
            matches!(out, Outcome::Text(ref t) if t.contains("exited ok")),
            "{out:?}"
        );
        let mut saw_child = false;
        let mut saw_line = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::StreamChild(_) => saw_child = true,
                Msg::Log(l) if l.contains("svc-web") => saw_line = true,
                _ => {}
            }
        }
        assert!(saw_child && saw_line);
        // service: None is ambiguous (3 services) → the choices error
        assert!(stream(
            &c,
            &Job::RunService {
                package: "svc".into(),
                service: None
            },
            &tx
        )
        .is_err());
        // empty exec argv, unknown service, missing provider, missing
        // package, and a spawn failure all error before/at spawn
        for (pkg_name, svc_name) in [
            ("svc", "void"),
            ("svc", "ghost-svc"),
            ("plain", "web"),
            ("ghost", "web"),
            ("svc", "bad"),
        ] {
            assert!(
                stream(
                    &c,
                    &Job::RunService {
                        package: pkg_name.into(),
                        service: Some(svc_name.into())
                    },
                    &tx
                )
                .is_err(),
                "{pkg_name}/{svc_name} should fail"
            );
        }
        // logs — bare unit tail exits; an invalid unit name fails
        // validation; a store entry's provider units resolve too
        let out = stream(
            &c,
            &Job::Logs {
                name: "tamias-cov-no-such-unit".into(),
                follow: false,
            },
            &tx,
        )
        .unwrap();
        assert!(
            matches!(out, Outcome::Text(ref t) if t.contains("exited")),
            "{out:?}"
        );
        assert!(stream(
            &c,
            &Job::Logs {
                name: "bad name!".into(),
                follow: false
            },
            &tx
        )
        .is_err());
        // `svc` was uninstalled? no — only `prov` was; svc's units
        // tail through units_of
        let out = stream(
            &c,
            &Job::Logs {
                name: "svc".into(),
                follow: false,
            },
            &tx,
        )
        .unwrap();
        assert!(matches!(out, Outcome::Text(_)), "{out:?}");
        // a provider-less store entry errors in units_of
        assert!(stream(
            &c,
            &Job::Logs {
                name: "plain".into(),
                follow: false
            },
            &tx
        )
        .is_err());
        // follow mode runs until killed — journalctl -f never exits
        // on its own; the StreamChild handle is the kill path
        let journalctl = std::process::Command::new("journalctl")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if journalctl {
            let (tx2, rx2) = std::sync::mpsc::channel();
            let c2 = c.clone();
            let handle = std::thread::spawn(move || {
                stream(
                    &c2,
                    &Job::Logs {
                        name: "tamias-cov-follow".into(),
                        follow: true,
                    },
                    &tx2,
                )
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let child = loop {
                match rx2.try_recv() {
                    Ok(Msg::StreamChild(ch)) => break ch,
                    Ok(_) => {}
                    Err(_) => {
                        assert!(std::time::Instant::now() < deadline, "no stream child");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            };
            child.lock().unwrap().kill().unwrap();
            let out = handle.join().unwrap().unwrap();
            assert!(
                matches!(out, Outcome::Text(ref t) if t.contains("exited")),
                "{out:?}"
            );
        }
        // non-stream jobs refuse the stream entry point; stream jobs
        // refuse run()
        assert!(stream(&c, &Job::Verify("svc".into()), &tx).is_err());
        assert!(run(
            &c,
            &Job::Logs {
                name: "x".into(),
                follow: false
            },
            &mut |_| {}
        )
        .is_err());
    }

    /// The install/sync progress closures' deep arms — `include:`
    /// base recursion, bundled-driver consent (refusal, then granted
    /// → registration), and the declared-`bins[]` deploy report.
    #[test]
    fn install_progress_deep_arms() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        let c = ctx(tmp.path());
        let base = pkg(tmp.path(), "base", "name: base\nversion: 1.0.0\n");
        let base_ref = base.display().to_string();
        // A package exercising every progress arm: an `include:` ref
        // (recursion), a portable bundled driver (consent gate), and
        // a `bins[]` shim decl (post-commit deploy).
        let rich = |name: &str, bin: &str| {
            let p = pkg(
                tmp.path(),
                name,
                &format!(
                    "name: {name}\nversion: 1.0.0\ninclude: [\"{base_ref}\"]\n\
                     bins:\n  - name: {bin}\n    shim: cli\n    driver: files\n"
                ),
            );
            std::fs::create_dir_all(p.join("drivers")).unwrap();
            std::fs::write(p.join("drivers/tamias-driver-x"), "#!/bin/sh\n").unwrap();
            p
        };
        let top = rich("top", "topbin");
        let top2 = rich("top2", "top2bin");
        let top_ref = top.display().to_string();
        let top2_ref = top2.display().to_string();

        // no consent → the refusal the UI's modal detects
        let err = run(
            &c,
            &Job::Install {
                reference: top_ref.clone(),
                allow_drivers: false,
            },
            &mut |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("refusing to install"), "{err}");

        // `:sync` over a project manifest dep — the sync closure's
        // progress arms fire identically (install is the shared tail)
        std::fs::write(
            tmp.path().join("tamias.yaml"),
            format!("name: proj\ndeps: [\"{top2_ref}\"]\n"),
        )
        .unwrap();
        let mut lines = Vec::new();
        let out = run(
            &c,
            &Job::Sync {
                allow_drivers: true,
                from_lock: false,
            },
            &mut |l| lines.push(l),
        )
        .unwrap();
        assert!(
            matches!(out, Outcome::Text(ref t) if t.contains("sync:")),
            "{out:?}"
        );
        assert!(lines.iter().any(|l| l.contains("including")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("driver:")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("bin:")), "{lines:?}");
        // the include base committed to the store; the bundled driver
        // registered under the guarded tamias home; the shim deployed
        // into the guarded bin dir
        assert!(Store::new(&c.store_root).find("base").is_ok());
        assert!(tmp.path().join("home/drivers/tamias-driver-x").exists());
        assert!(tmp.path().join("bin/top2bin").exists());

        // the same arms through the plain install job
        let mut lines = Vec::new();
        let out = run(
            &c,
            &Job::Install {
                reference: top_ref,
                allow_drivers: true,
            },
            &mut |l| lines.push(l),
        )
        .unwrap();
        assert!(matches!(out, Outcome::Text(ref t) if t.contains("installed top@1.0.0")));
        assert!(lines.iter().any(|l| l.contains("including")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("bin:")), "{lines:?}");
        assert!(tmp.path().join("bin/topbin").exists());
    }

    /// `label`/`is_stream`/`with_drivers_allowed` over every variant —
    /// the status-bar words and consent re-run mapping.
    #[test]
    fn job_labels_all_variants() {
        let jobs = vec![
            Job::Verify("w".into()),
            Job::Remove("w".into()),
            Job::Update {
                name: "w".into(),
                allow_drivers: false,
            },
            Job::UpdateAll {
                allow_drivers: false,
            },
            Job::Pin {
                name: "w".into(),
                reference: "r".into(),
            },
            Job::CacheClean { all: false },
            Job::CacheClean { all: true },
            Job::Doctor,
            Job::Search {
                query: "q".into(),
                target: Some("t".into()),
            },
            Job::Install {
                reference: "r".into(),
                allow_drivers: false,
            },
            Job::ServicesRefresh,
            Job::UnitAction {
                unit: "u".into(),
                action: tamias_provider::UnitAction::Start,
            },
            Job::UnitAction {
                unit: "u".into(),
                action: tamias_provider::UnitAction::Stop,
            },
            Job::UnitAction {
                unit: "u".into(),
                action: tamias_provider::UnitAction::Restart,
            },
            Job::UnitAction {
                unit: "u".into(),
                action: tamias_provider::UnitAction::Enable,
            },
            Job::UnitAction {
                unit: "u".into(),
                action: tamias_provider::UnitAction::Disable,
            },
            Job::ProviderDeploy {
                name: "w".into(),
                config: Some("c".into()),
            },
            Job::ProviderUninstall { name: "w".into() },
            Job::RunService {
                package: "w".into(),
                service: Some("s".into()),
            },
            Job::RunService {
                package: "w".into(),
                service: None,
            },
            Job::Logs {
                name: "w".into(),
                follow: true,
            },
            Job::Logs {
                name: "w".into(),
                follow: false,
            },
            Job::AddDep {
                reference: "r".into(),
                allow_drivers: false,
            },
            Job::RemoveDep {
                reference: "r".into(),
            },
            Job::SetDep {
                old: "o".into(),
                new: "n".into(),
                allow_drivers: false,
            },
            Job::Sync {
                allow_drivers: false,
                from_lock: true,
            },
            Job::Pack {
                dir: "d".into(),
                out: Some("o".into()),
            },
            Job::Publish {
                dir: "d".into(),
                registry: Some("r".into()),
            },
            Job::Registries,
        ];
        for j in &jobs {
            assert!(!j.label().is_empty(), "{j:?}");
        }
        assert!(Job::RunService {
            package: "w".into(),
            service: None
        }
        .is_stream());
        assert!(Job::Logs {
            name: "w".into(),
            follow: true
        }
        .is_stream());
        assert!(!Job::Verify("w".into()).is_stream());
        // consent mapping — every install-family job flips, the rest
        // pass through
        for j in jobs {
            let allowed = j.clone().with_drivers_allowed();
            let flipped = match (&j, &allowed) {
                (
                    Job::Update {
                        allow_drivers: a, ..
                    }
                    | Job::Install {
                        allow_drivers: a, ..
                    }
                    | Job::AddDep {
                        allow_drivers: a, ..
                    },
                    Job::Update {
                        allow_drivers: b, ..
                    }
                    | Job::Install {
                        allow_drivers: b, ..
                    }
                    | Job::AddDep {
                        allow_drivers: b, ..
                    },
                ) => *b && !*a,
                (Job::UpdateAll { allow_drivers: a }, Job::UpdateAll { allow_drivers: b }) => {
                    *b && !*a
                }
                (
                    Job::SetDep {
                        allow_drivers: a, ..
                    },
                    Job::SetDep {
                        allow_drivers: b, ..
                    },
                ) => *b && !*a,
                (
                    Job::Sync {
                        allow_drivers: a, ..
                    },
                    Job::Sync {
                        allow_drivers: b, ..
                    },
                ) => *b && !*a,
                _ => matches!(
                    allowed,
                    Job::Verify(_)
                        | Job::Remove(_)
                        | Job::Pin { .. }
                        | Job::CacheClean { .. }
                        | Job::Doctor
                        | Job::Search { .. }
                        | Job::ServicesRefresh
                        | Job::UnitAction { .. }
                        | Job::ProviderDeploy { .. }
                        | Job::ProviderUninstall { .. }
                        | Job::RunService { .. }
                        | Job::Logs { .. }
                        | Job::RemoveDep { .. }
                        | Job::Pack { .. }
                        | Job::Publish { .. }
                        | Job::Registries
                ),
            };
            assert!(flipped, "{j:?} → {allowed:?}");
        }
    }

    /// Label arms the all-variants sweep doesn't spell out, doctor
    /// error branches, empty-store update-all, a failing update, and
    /// a bad-signature verify.
    #[test]
    fn label_edges_doctor_and_error_arms() {
        // label() arms outside the variants sweep
        assert_eq!(
            Job::Sync {
                allow_drivers: false,
                from_lock: false
            }
            .label(),
            "sync deps"
        );
        assert_eq!(
            Job::Publish {
                dir: "d".into(),
                registry: None
            }
            .label(),
            "publish d"
        );

        let tmp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::enter(tmp.path());
        // doctor with file-instead-of-dir roots → the store/cache/
        // registry `Err` check arms all fire
        let f = tmp.path().join("afile");
        std::fs::write(&f, b"x").unwrap();
        let c_bad = Ctx::new(&f, &f, &f, Some(tmp.path().join("no-cfg.yml")));
        let err = run(&c_bad, &Job::Doctor, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("store"), "{err}");
        assert!(err.contains("cache"), "{err}");

        // update-all over an empty store → the early "nothing
        // installed" return
        let c = ctx(tmp.path());
        let t = text(
            &c,
            &Job::UpdateAll {
                allow_drivers: false,
            },
        );
        assert!(t.contains("nothing installed"), "{t}");

        // update whose recorded ref no longer exists → install_ref
        // error propagates through update()
        let payload = pkg(tmp.path(), "gone", "name: gone\nversion: 1.0.0\n");
        place(&c, &payload, "gone", None);
        std::fs::remove_dir_all(&payload).unwrap();
        assert!(run(
            &c,
            &Job::Update {
                name: "gone".into(),
                allow_drivers: false
            },
            &mut |_| {}
        )
        .is_err());

        // verify on a record carrying a bogus signature → the
        // signature-status arm errors (or propagates) either way
        let mut rec = record("signed", Path::new("/x"), Some("files"));
        rec.signature = Some("not-a-real-signature".into());
        let signed_payload = pkg(tmp.path(), "signed", "name: signed\nversion: 1.0.0\n");
        Store::new(&c.store_root)
            .install(&signed_payload, rec)
            .unwrap();
        assert!(run(&c, &Job::Verify("signed".into()), &mut |_| {}).is_err());

        // doctor with no discoverable drivers → the `none` arm in
        // the drivers check line (PATH emptied under the env guard)
        std::env::set_var("PATH", "");
        let res = run(&c, &Job::Doctor, &mut |_| {});
        let rendered = format!("{res:?}");
        assert!(rendered.contains("drivers"), "{rendered}");
    }
}
