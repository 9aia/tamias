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

use std::path::{Path, PathBuf};
use std::time::Duration;

use tamias_cache_local::FetchCache;
use tamias_core::{Error, Lock, StoreEntry};
use tamias_install::{self, Progress, Surface};
use tamias_registry::{parse_registry_ref, RegistryEntry, RegistryRef};
use tamias_registry_open as ro;
use tamias_store_local::Store;

use crate::Ctx;

/// `cache clean` default window — same 30 days as `tamias cache clean`.
const CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 86_400);

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
        }
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
            other => other,
        }
    }
}

/// What a finished job hands back — `Text` is the one-line summary;
/// `Search` carries typed results for the Registry pane.
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
/// the config's global `limits` (per-package overrides can't apply:
/// the name isn't known until after the fetch).
fn install_env(ctx: &Ctx) -> tamias_install::Env {
    tamias_install::Env {
        store_root: ctx.store_root.clone(),
        cache_root: ctx.cache_root.clone(),
        max_download: Some(env_config(ctx).effective_limits(None, None).max_download),
    }
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

/// Keep `tamias.lock` honest after `update`/`pin` — the CLI's rule:
/// when a lockfile exists in the cwd *and* already pins the package,
/// refresh its entry; never create one here.
fn sync_lock(record: &tamias_core::StoreRecord) -> Result<(), Error> {
    let lock_path = PathBuf::from("tamias.lock");
    if !lock_path.is_file() {
        return Ok(());
    }
    let mut lock = Lock::load(&lock_path)?;
    let Some(entry) = lock
        .packages
        .iter_mut()
        .find(|e| e.name == record.name && e.scope == record.scope)
    else {
        return Ok(());
    };
    entry.source_ref = record.source_ref.clone();
    entry.resolved = record.resolved.clone();
    entry.digest = record.digest.clone();
    entry.version = record.version.clone();
    lock.save(&lock_path)
}

/// `verify` — recompute the payload digest against the record.
fn verify(ctx: &Ctx, name: &str) -> Result<Outcome, Error> {
    let report = Store::new(&ctx.store_root).verify(name)?;
    if report.ok {
        Ok(Outcome::Text(format!(
            "{}: ok ({})",
            report.name,
            short(&report.recorded)
        )))
    } else {
        Err(Error::Failed(format!(
            "{}: digest mismatch — recorded {}, computed {}",
            report.name,
            short(&report.recorded),
            short(&report.computed)
        )))
    }
}

/// `remove` — the serve/CLI rule: driver `uninstall` dispatch first
/// (a failing driver keeps the store entry); a missing driver on a
/// non-`files` kind warns that provider resources may remain.
fn remove(ctx: &Ctx, name: &str, log: &mut dyn FnMut(String)) -> Result<Outcome, Error> {
    let store = Store::new(&ctx.store_root);
    let entry = store.find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    if !tamias_driver::dispatch(kind, "uninstall", &entry.dir, &entry.record.name, None)?
        && kind != "files"
    {
        log(format!(
            "warning — no driver for type '{kind}'; provider resources may remain"
        ));
    }
    let record = store.remove(name)?;
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

/// `search` — same routing as serve's `search` op: `target` flag →
/// `TAMIAS_REGISTRY` → `search.default` → `publish.default` → the
/// local default root; then `registry.search(query)`.
fn search(ctx: &Ctx, query: &str, target: Option<&str>) -> Result<Outcome, Error> {
    let config = env_config(ctx);
    let value = ro::registry_target(
        target,
        &[
            config.search.default.as_deref(),
            config.publish.default.as_deref(),
        ],
    );
    let registry = ro::open_registry(&ctx.registry_root, Some(&config), value.as_deref(), None)?;
    let results = registry.search(query)?;
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
    }
}

/// `sha256:<hex>` → first 12 hex chars (same shortening the CLI/serve
/// surfaces use).
fn short(digest: &str) -> &str {
    let s = digest.strip_prefix("sha256:").unwrap_or(digest);
    s.get(..12).unwrap_or(s)
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
