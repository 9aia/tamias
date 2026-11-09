#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias — the CLI. P2b: the distribution verbs over the source port, the
//! content-addressed cache, and the local store.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tamias_cache_local::FetchCache;
use tamias_config::{ConfigFormat, CONFIG_NAMES};
use tamias_config_toml::TomlFormat;
use tamias_config_yml::YmlFormat;
use tamias_core::{payload_root, Error, Lock, LockEntry, Manifest, StoreEntry, StoreRecord};
use tamias_registry::{parse_registry_ref, Registry, RegistryRef};
use tamias_registry_cf::HttpRegistry;
use tamias_registry_local::{serve_http, LocalRegistry};
use tamias_source::{Planned, Refs, Sources};
use tamias_source_cf::{CfRefKind, CfSource};
use tamias_source_fs::{FsRefKind, FsSource};
use tamias_source_git::{GitRefKind, GitSource};
use tamias_source_http::{HttpRefKind, HttpSource};
use tamias_store_local::Store;

/// Default fetch-cache eviction age for `cache clean` — 30 days.
const CACHE_MAX_AGE_DAYS: u64 = 30;

#[derive(Parser)]
#[command(
    name = "tamias",
    version,
    about = "Package & content manager — fetch, store, cache, dispense"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch → store a package from a ref.
    Install {
        /// ./path | git:<url>[//subdir][#rev] | https://….tgz[#sha256:…]
        reference: String,
        /// Consent to register + run the package's bundled `drivers/`.
        /// Equivalent: `TAMIAS_ALLOW_DRIVERS=1`.
        #[arg(long)]
        allow_drivers: bool,
    },
    /// Install a ref and record it in `tamias.yaml` + `tamias.lock`.
    ///
    /// Writes the *project* manifest (`deps:`/`include:`) — run from the
    /// project dir, not inside a package's source dir (a package's own
    /// `tamias.yaml` is its package manifest).
    Add {
        /// The ref to add.
        reference: String,
        /// Consent to register + run the package's bundled `drivers/`.
        /// Equivalent: `TAMIAS_ALLOW_DRIVERS=1`.
        #[arg(long)]
        allow_drivers: bool,
    },
    /// Drop a store entry (cache untouched).
    Remove {
        /// `@scope/name` or `name`.
        name: String,
    },
    /// List installed packages.
    List {
        /// Only entries of this content type.
        #[arg(long = "type")]
        kind: Option<String>,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Show one package's manifest + provenance.
    Info {
        /// `@scope/name` or `name`.
        name: String,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Digest-check a package against its record.
    Verify {
        /// `@scope/name` or `name`.
        name: String,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Re-resolve + re-install a package from its recorded ref.
    Update {
        /// `@scope/name` or `name` (omit with `--all`).
        name: Option<String>,
        /// Update every installed package.
        #[arg(long)]
        all: bool,
        /// Consent to register + run bundled `drivers/` on re-install.
        /// Equivalent: `TAMIAS_ALLOW_DRIVERS=1`.
        #[arg(long)]
        allow_drivers: bool,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Retarget an installed package's recorded source ref.
    Pin {
        /// `@scope/name` or `name`.
        name: String,
        /// The new ref to record.
        reference: String,
    },
    /// Fetch-cache maintenance.
    Cache {
        #[command(subcommand)]
        cmd: CacheCommand,
    },
    /// Print the tamias version.
    Version,
    /// Print the machine-readable capability manifest.
    Manifest,
    /// Print the `tamias.yaml` JSON Schema.
    Schema,
    /// Print the CLI reference (markdown) generated from the parser.
    GenCliDocs,
    /// Print a shell completion script.
    Completions {
        /// The shell to generate completions for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Diagnose the host metaservice (store, cache, registry, drivers).
    Doctor {
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Show the effective config — YAML (`tamias.yml`) is the standard.
    Config {
        /// Explicit config file (overrides discovery).
        #[arg(long)]
        file: Option<PathBuf>,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Run the ndjson JSON-RPC service over stdio.
    Serve,
    /// Publish a package dir to a registry.
    Publish {
        /// Payload dir (holds `tamias.yaml`).
        dir: PathBuf,
        /// Registry root or http(s) base URL (default `~/.local/share/tamias/registry`).
        #[arg(long)]
        registry: Option<String>,
    },
    /// Search a registry's index.
    Search {
        /// Name/scope substring.
        query: String,
        /// Registry root or http(s) base URL.
        #[arg(long)]
        registry: Option<String>,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Author-side packing.
    Bundle {
        #[command(subcommand)]
        cmd: BundleCommand,
    },
    /// Registry operations.
    Registry {
        #[command(subcommand)]
        cmd: RegistryCommand,
    },
    /// Deploy the registry Worker (Cloudflare).
    DeployRegistry {
        /// Output dir (default `./tamias-registry-worker`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Deploy an installed package's service/runtime (via its provider).
    Deploy {
        /// `@scope/name` or `name`.
        name: String,
        /// Node/config file the provider reads operator params from.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Uninstall — provider teardown + drop the store entry.
    Uninstall {
        /// `@scope/name` or `name`.
        name: String,
    },
    /// Manage a per-user service unit (systemd).
    Service {
        #[command(subcommand)]
        cmd: ServiceCommand,
    },
}

#[derive(Subcommand)]
enum RegistryCommand {
    /// Serve a local registry over HTTP.
    Serve {
        /// Bind address.
        #[arg(long, default_value = "127.0.0.1:8788")]
        addr: String,
        /// Registry root (default `~/.local/share/tamias/registry`).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Evict the fetch cache by age, or everything.
    Clean {
        /// Evict everything regardless of age.
        #[arg(long, conflicts_with = "days")]
        all: bool,
        /// Max age in days (default 30).
        #[arg(long)]
        days: Option<u64>,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BundleCommand {
    /// Pack a payload dir into a `.tgz`.
    Pack {
        /// The payload dir (holds `tamias.yaml`).
        dir: PathBuf,
        /// Output `.tgz` (default `<name>-<version>.tgz`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// Store + cache roots — `TAMIAS_HOME`/`TAMIAS_CACHE`/XDG, else the
/// `~/.local/share` + `~/.cache` defaults.
#[derive(Clone, Debug)]
struct Env {
    store_root: PathBuf,
    cache_root: PathBuf,
    registry_root: PathBuf,
}

impl Env {
    fn from_env() -> Self {
        Self {
            store_root: store_root(),
            cache_root: cache_root(),
            registry_root: registry_root(),
        }
    }
}

fn store_root() -> PathBuf {
    std::env::var_os("TAMIAS_HOME")
        .map(|h| PathBuf::from(h).join("store"))
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("tamias/store"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/tamias/store"))
        })
        .unwrap_or_else(|| PathBuf::from(".tamias/store"))
}

fn cache_root() -> PathBuf {
    std::env::var_os("TAMIAS_CACHE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME").map(|d| PathBuf::from(d).join("tamias/fetch"))
        })
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/tamias/fetch")))
        .unwrap_or_else(|| PathBuf::from(".tamias/fetch"))
}

fn registry_root() -> PathBuf {
    std::env::var_os("TAMIAS_REGISTRY")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("tamias/registry"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/tamias/registry"))
        })
        .unwrap_or_else(|| PathBuf::from(".tamias/registry"))
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

fn install(env: &Env, reference: &str, allow_drivers: bool) -> Result<StoreEntry, Error> {
    let span = tracing::info_span!("install", reference);
    let _enter = span.enter();
    let spec = refs().parse(reference)?;
    let planned = sources().plan(&*spec)?;
    eprintln!("tamias: fetching {reference}");
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
    eprintln!("tamias: installing {installing}");
    // Bundled drivers are arbitrary code shipped inside the package — they
    // must never be registered or executed without explicit consent. Fail
    // *before* the store entry is written so a refused install leaves no
    // partial state (the fetch-cache entry is content-addressed and stays).
    let shipped = tamias_driver::bundled_drivers(&root)?;
    if !shipped.is_empty() && !tamias_driver::drivers_allowed(allow_drivers) {
        return Err(Error::Failed(format!(
            "package ships bundled driver(s): {} — refusing to install + run them; \
             re-run with --allow-drivers or TAMIAS_ALLOW_DRIVERS=1 to consent",
            shipped.join(", ")
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
        println!("  {action}");
    }
    if !tamias_driver::dispatch(kind, "install", &root, &record.name, None)? && kind != "files" {
        return Err(Error::Failed(format!(
            "no driver for content type '{kind}' — install `tamias-driver-{kind}` on PATH \
             or ship one in the package's `drivers/` dir{}",
            if shipped.is_empty() {
                String::new()
            } else {
                "; bundled drivers need --allow-drivers or TAMIAS_ALLOW_DRIVERS=1".to_string()
            }
        )));
    }
    let entry = Store::new(&env.store_root).install(&root, record)?;
    eprintln!("tamias: done — {}", entry.label());
    Ok(entry)
}

fn short(digest: &str) -> &str {
    let s = digest.strip_prefix("sha256:").unwrap_or(digest);
    s.get(..12).unwrap_or(s)
}

/// The `manifest` verb list — the top-level subcommand names, generated
/// from the clap tree so it can't drift from the real verbs.
fn cli_verbs() -> Vec<String> {
    use clap::CommandFactory;
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect()
}

/// The CLI reference (markdown), generated from the clap parser.
fn cli_docs() -> String {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    format!("# tamias CLI\n\n```\n{}\n```\n", cmd.render_long_help())
}

/// Print a JSON value on stdout — the `--json` output shape.
fn print_json(value: &serde_json::Value) -> Result<(), Error> {
    println!(
        "{}",
        serde_json::to_string(value).map_err(|e| Error::Failed(e.to_string()))?
    );
    Ok(())
}

/// One store entry as a JSON object — the record fields plus the derived
/// label, payload size, and on-disk dir.
fn entry_json(entry: &StoreEntry) -> serde_json::Value {
    serde_json::json!({
        "label": entry.label(),
        "name": entry.record.name,
        "scope": entry.record.scope,
        "version": entry.record.version,
        "kind": entry.record.kind,
        "source_ref": entry.record.source_ref,
        "resolved": entry.record.resolved,
        "digest": entry.record.digest,
        "installed_at": entry.record.installed_at,
        "size": entry.size().ok(),
        "dir": entry.dir.display().to_string(),
    })
}

/// Writability probe — create the root if needed, then create + remove a
/// probe file inside it.
fn writable(dir: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let probe = dir.join(".doctor-probe");
    std::fs::write(&probe, b"").map_err(Error::io(format!("write {}", probe.display())))?;
    std::fs::remove_file(&probe).map_err(Error::io(format!("remove {}", probe.display())))?;
    Ok(())
}

/// Keep `tamias.lock` honest after `update`/`pin`: when a lockfile exists
/// in the cwd *and* already pins this package, refresh its entry to the
/// new record. A missing lockfile — or one that doesn't pin the package —
/// is left alone (never created here; that's `add`'s job).
fn sync_lock(record: &StoreRecord) -> Result<(), Error> {
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

/// Open a registry from a `--registry` value (dir or http URL) — the local
/// dir by default.
fn open_registry(env: &Env, value: Option<&str>) -> Box<dyn Registry> {
    let value = value
        .map(str::to_string)
        .unwrap_or_else(|| env.registry_root.display().to_string());
    match parse_registry_ref(&value) {
        RegistryRef::Local(path) => Box::new(LocalRegistry::new(path)),
        RegistryRef::Http(base) => Box::new(HttpRegistry::from_env(base)),
    }
}

fn run(cli: Cli, env: Env) -> Result<(), Error> {
    match cli.command {
        Some(Command::Install {
            reference,
            allow_drivers,
        }) => {
            let entry = install(&env, &reference, allow_drivers)?;
            println!(
                "installed {} ({})",
                entry.label(),
                short(&entry.record.digest)
            );
        }
        Some(Command::Add {
            reference,
            allow_drivers,
        }) => {
            let entry = install(&env, &reference, allow_drivers)?;
            let manifest_path = PathBuf::from("tamias.yaml");
            let mut project = if manifest_path.is_file() {
                // A project manifest needn't name a package — `deps:` /
                // `include:` only is fine.
                Manifest::load_project_at(&manifest_path)?
            } else {
                Manifest::default()
            };
            if !project.deps.contains(&reference) {
                project.deps.push(reference.clone());
            }
            project.save(&manifest_path)?;
            let lock_path = PathBuf::from("tamias.lock");
            let mut lock = Lock::load(&lock_path)?;
            lock.upsert(LockEntry::from_record(&entry.record));
            lock.save(&lock_path)?;
            println!(
                "added {} → tamias.yaml + tamias.lock ({})",
                entry.label(),
                short(&entry.record.digest)
            );
        }
        Some(Command::Remove { name }) => {
            let record = Store::new(&env.store_root).remove(&name)?;
            println!("removed {}@{}", record.name, record.resolved);
        }
        Some(Command::List { kind, json }) => {
            let entries: Vec<StoreEntry> = Store::new(&env.store_root)
                .entries()?
                .into_iter()
                .filter(|e| {
                    kind.as_deref()
                        .is_none_or(|k| e.record.kind.as_deref() == Some(k))
                })
                .collect();
            if json {
                let packages: Vec<serde_json::Value> = entries.iter().map(entry_json).collect();
                print_json(&serde_json::json!({ "packages": packages }))?;
            } else {
                if entries.is_empty() {
                    println!("no packages installed");
                }
                for e in entries {
                    println!(
                        "{}  {}  {}",
                        e.label(),
                        short(&e.record.digest),
                        e.record.source_ref
                    );
                }
            }
        }
        Some(Command::Info { name, json }) => {
            let entry = Store::new(&env.store_root).find(&name)?;
            if json {
                print_json(&entry_json(&entry))?;
            } else {
                println!("name:    {}", entry.record.name);
                if let Some(scope) = &entry.record.scope {
                    println!("scope:   {scope}");
                }
                println!(
                    "version: {}",
                    entry.record.version.as_deref().unwrap_or("-")
                );
                println!("type:    {}", entry.record.kind.as_deref().unwrap_or("-"));
                println!("source:  {}", entry.record.source_ref);
                println!("resolved: {}", entry.record.resolved);
                println!("digest:  {}", entry.record.digest);
                println!("size:    {} bytes", entry.size()?);
                println!("dir:     {}", entry.dir.display());
            }
        }
        Some(Command::Verify { name, json }) => {
            let report = Store::new(&env.store_root).verify(&name)?;
            if json {
                print_json(&serde_json::json!({
                    "name": report.name,
                    "recorded": report.recorded,
                    "computed": report.computed,
                    "ok": report.ok,
                }))?;
            }
            if report.ok {
                if !json {
                    println!("{}: ok ({})", report.name, short(&report.recorded));
                }
            } else {
                return Err(Error::Failed(format!(
                    "{}: digest mismatch — recorded {}, computed {}",
                    report.name,
                    short(&report.recorded),
                    short(&report.computed)
                )));
            }
        }
        Some(Command::Update {
            name,
            all,
            allow_drivers,
            json,
        }) => {
            let store = Store::new(&env.store_root);
            let refs: Vec<String> = if all {
                store
                    .entries()?
                    .into_iter()
                    .map(|e| e.record.source_ref)
                    .collect()
            } else {
                let n = name.ok_or_else(|| Error::Failed("update: pass a name or --all".into()))?;
                vec![store.find(&n)?.record.source_ref]
            };
            eprintln!("tamias: updating {} package(s)", refs.len());
            let mut updated = Vec::new();
            for r in refs {
                let entry = install(&env, &r, allow_drivers)?;
                sync_lock(&entry.record)?;
                let label = entry.label();
                if json {
                    updated.push(label);
                } else {
                    println!("updated {label}");
                }
            }
            if json {
                print_json(&serde_json::json!({ "updated": updated }))?;
            }
        }
        Some(Command::Pin { name, reference }) => {
            let record = Store::new(&env.store_root).retarget(&name, &reference)?;
            sync_lock(&record)?;
            println!("pinned {} → {}", record.name, record.source_ref);
        }
        Some(Command::Cache { cmd }) => {
            let cache = FetchCache::new(&env.cache_root);
            let CacheCommand::Clean { all, days, json } = cmd;
            let evicted = if all {
                cache.clean_all()?
            } else {
                cache.clean(Duration::from_secs(
                    days.unwrap_or(CACHE_MAX_AGE_DAYS) * 86_400,
                ))?
            };
            if json {
                print_json(&serde_json::json!({
                    "evicted": evicted.len(),
                    "entries": evicted
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>(),
                }))?;
            } else {
                println!("evicted {} cache entries", evicted.len());
            }
        }
        Some(Command::Serve) => {
            tamias_serve::serve(tamias_serve::Env {
                store_root: env.store_root.clone(),
                cache_root: env.cache_root.clone(),
            })?;
        }
        Some(Command::Publish { dir, registry }) => {
            let registry = open_registry(&env, registry.as_deref());
            let root = payload_root(&dir)?;
            let manifest = Manifest::load(&root)?;
            eprintln!("tamias: publishing {}", manifest.name);
            let entry = registry.publish(&root, &manifest)?;
            println!("published {}", entry.label());
        }
        Some(Command::Search {
            query,
            registry,
            json,
        }) => {
            let registry = open_registry(&env, registry.as_deref());
            let found = registry.search(&query)?;
            if json {
                let results: Vec<serde_json::Value> = found
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "label": e.label(),
                            "name": e.name,
                            "scope": e.scope,
                            "version": e.version,
                            "kind": e.kind,
                            "digest": e.digest,
                            "file": e.file,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({ "results": results }))?;
            } else {
                for e in &found {
                    println!("{}  {}  {}", e.label(), short(&e.digest), e.file);
                }
            }
        }
        Some(Command::Bundle { cmd }) => {
            let BundleCommand::Pack { dir, out } = cmd;
            let packed = tamias_publish::pack(&dir, out.as_deref())?;
            println!("packed → {}", packed.display());
        }
        Some(Command::Registry { cmd }) => {
            let RegistryCommand::Serve { addr, dir } = cmd;
            let root = dir.unwrap_or_else(|| env.registry_root.clone());
            serve_http(&addr, &root)?;
        }
        Some(Command::DeployRegistry { out }) => {
            let out = out.unwrap_or_else(|| PathBuf::from("tamias-registry-worker"));
            for action in tamias_deploy::Deploy::deploy(&tamias_deploy_cf::CfDeploy, &out)? {
                println!("{action}");
            }
        }
        Some(Command::Deploy { name, config }) => {
            deploy_package(&env, &name, config.as_deref())?;
        }
        Some(Command::Uninstall { name }) => {
            uninstall_package(&env, &name)?;
        }
        Some(Command::Service { cmd }) => {
            run_service(cmd)?;
        }
        Some(Command::Version) => {
            println!("tamias {}", env!("CARGO_PKG_VERSION"));
        }
        // Bare `tamias` prints the long help, not just the version.
        None => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            print!("{}", cmd.render_long_help());
        }
        Some(Command::Doctor { json }) => {
            doctor(&env, json)?;
        }
        Some(Command::Config { file, json }) => {
            let formats: [&dyn ConfigFormat; 2] = [&YmlFormat, &TomlFormat];
            let (path, config) = match &file {
                Some(p) => (Some(p.clone()), tamias_config::load_file(p, &formats)?),
                None => match tamias_config::load_in(Path::new("."), &formats)? {
                    Some((p, c)) => (Some(p), c),
                    None => (None, tamias_config::Config::default()),
                },
            };
            if json {
                let value = serde_json::json!({
                    "file": path.as_ref().map(|p| p.display().to_string()),
                    "search": CONFIG_NAMES,
                    "config": config,
                });
                println!(
                    "{}",
                    serde_json::to_string(&value).map_err(|e| Error::Failed(e.to_string()))?
                );
            } else {
                println!("{}", YmlFormat.render(&config)?);
            }
        }
        Some(Command::Manifest) => {
            // Same `{"name","version","verbs"}` shape as before — the
            // verb list is generated from the clap tree so it can't drift.
            let verbs = cli_verbs()
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "{{\"name\":\"tamias\",\"version\":\"{}\",\"verbs\":[{}]}}",
                env!("CARGO_PKG_VERSION"),
                verbs
            );
        }
        Some(Command::Schema) => {
            let schema = schemars::schema_for!(Manifest);
            println!(
                "{}",
                serde_json::to_string_pretty(&schema).map_err(|e| Error::Failed(e.to_string()))?
            );
        }
        Some(Command::GenCliDocs) => println!("{}", cli_docs()),
        Some(Command::Completions { shell }) => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "tamias", &mut std::io::stdout());
        }
    }
    Ok(())
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Write a per-user systemd unit (does not enable it).
    Install {
        /// Unit base name.
        #[arg(long)]
        unit: String,
        /// `ExecStart`.
        #[arg(long)]
        exec: String,
        /// Unit description.
        #[arg(long, default_value = "tamias service")]
        description: String,
        /// `K=V` environment — repeatable.
        #[arg(long = "env")]
        envs: Vec<String>,
        /// Working directory.
        #[arg(long)]
        workdir: Option<String>,
    },
    /// Enable + start the unit.
    Enable { unit: String },
    /// Disable + stop the unit.
    Disable { unit: String },
    /// Unit status.
    Status { unit: String },
    /// Follow the unit's logs.
    Logs { unit: String },
    /// Disable + remove the unit file.
    Uninstall { unit: String },
}

/// `deploy <name>` — run the installed package's provider deploy step.
fn deploy_package(env: &Env, name: &str, config: Option<&Path>) -> Result<(), Error> {
    let entry = Store::new(&env.store_root).find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    if tamias_driver::dispatch(kind, "deploy", &entry.dir, &entry.record.name, config)? {
        println!("deployed {}", entry.label());
    } else {
        println!("{}: nothing to deploy (type {kind})", entry.label());
    }
    Ok(())
}

/// `uninstall <name>` — provider teardown + drop the store entry.
fn uninstall_package(env: &Env, name: &str) -> Result<(), Error> {
    let store = Store::new(&env.store_root);
    let entry = store.find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    // Teardown runs before the entry is dropped: a failing driver keeps the
    // store entry so the uninstall can be retried instead of leaking
    // provider-side resources under a false "uninstalled".
    if !tamias_driver::dispatch(kind, "uninstall", &entry.dir, &entry.record.name, None)?
        && kind != "files"
    {
        eprintln!("tamias: warning — no driver for type '{kind}'; provider resources may remain");
    }
    let record = store.remove(name)?;
    println!("uninstalled {}@{}", record.name, record.resolved);
    Ok(())
}

/// `service …` — the per-user unit (systemd) mechanism.
fn run_service(cmd: ServiceCommand) -> Result<(), Error> {
    use tamias_deploy_local::{LocalDeploy, ServiceSpec};
    let deploy = LocalDeploy::new();
    match cmd {
        ServiceCommand::Install {
            unit,
            exec,
            description,
            envs,
            workdir,
        } => {
            let env = envs
                .iter()
                .filter_map(|kv| match kv.split_once('=') {
                    Some((k, v)) => Some((k.to_string(), v.to_string())),
                    None => {
                        eprintln!(
                            "tamias: warning — ignoring malformed --env '{kv}' (expected K=V)"
                        );
                        None
                    }
                })
                .collect();
            let spec = ServiceSpec {
                name: unit.clone(),
                description,
                exec,
                env,
                workdir,
            };
            let path = deploy.install(&spec)?;
            println!(
                "wrote {} (enable: tamias service enable {unit})",
                path.display()
            );
        }
        ServiceCommand::Enable { unit } => {
            deploy.enable(&unit)?;
            println!("enabled {unit}");
        }
        ServiceCommand::Disable { unit } => {
            deploy.disable(&unit)?;
            println!("disabled {unit}");
        }
        ServiceCommand::Status { unit } => deploy.status(&unit)?,
        ServiceCommand::Logs { unit } => deploy.logs(&unit)?,
        ServiceCommand::Uninstall { unit } => {
            deploy.uninstall(&unit)?;
            println!("removed unit {unit}");
        }
    }
    Ok(())
}

/// The registry check — a configured `publish.default` registry must
/// answer `entries()`; otherwise the env default local root is probed
/// (a missing index is reachable, not a failure).
fn doctor_registry(env: &Env) -> (String, bool) {
    let formats: [&dyn ConfigFormat; 2] = [&YmlFormat, &TomlFormat];
    let configured = tamias_config::load_in(Path::new("."), &formats)
        .ok()
        .flatten()
        .and_then(|(_, c)| {
            let name = c.publish.default?;
            c.registries.get(&name).map(|r| (name, r.clone()))
        });
    if let Some((name, reg)) = configured {
        let value = reg
            .url
            .clone()
            .unwrap_or_else(|| env.registry_root.display().to_string());
        let registry: Box<dyn Registry> = match parse_registry_ref(&value) {
            RegistryRef::Local(path) => Box::new(LocalRegistry::new(path)),
            RegistryRef::Http(base) => Box::new(HttpRegistry::from_env(base)),
        };
        return match registry.entries() {
            Ok(entries) => (
                format!("{name} ({value}): {} entr(ies)", entries.len()),
                true,
            ),
            Err(e) => (format!("{name} ({value}): {e}"), false),
        };
    }
    match LocalRegistry::new(&env.registry_root).entries() {
        Ok(_) => (env.registry_root.display().to_string(), true),
        Err(e) => (format!("{}: {e}", env.registry_root.display()), false),
    }
}

/// `doctor` — diagnose the host metaservice. Any failed check → non-zero
/// exit (the report is still printed first, `--json` included).
fn doctor(env: &Env, json: bool) -> Result<(), Error> {
    let store_entries = Store::new(&env.store_root).entries();
    let cache_entries = FetchCache::new(&env.cache_root).entries();
    let drivers = tamias_driver::discovered_drivers();

    let (git_detail, git_ok) = match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            true,
        ),
        Ok(out) => (format!("git --version → {}", out.status), false),
        Err(e) => (format!("git not on PATH: {e}"), false),
    };

    // `files` is the built-in kind — the store handles it with no driver
    // binary; a `tamias-driver-files` on PATH/`drivers/` resolves too.
    let (files_detail, files_ok) = match tamias_driver::find_driver("files") {
        Some(path) => (path.display().to_string(), true),
        None => ("built-in".to_string(), true),
    };

    let (registry_detail, registry_ok) = doctor_registry(env);

    let (store_detail, store_ok, packages_detail, packages_ok) = match &store_entries {
        Ok(entries) => (
            env.store_root.display().to_string(),
            writable(&env.store_root).is_ok(),
            entries.len().to_string(),
            true,
        ),
        Err(e) => (
            env.store_root.display().to_string(),
            writable(&env.store_root).is_ok(),
            format!("error: {e}"),
            false,
        ),
    };
    let (cache_detail, cache_ok, cache_entries_detail, cache_entries_ok) = match &cache_entries {
        Ok(entries) => (
            env.cache_root.display().to_string(),
            writable(&env.cache_root).is_ok(),
            entries.len().to_string(),
            true,
        ),
        Err(e) => (
            env.cache_root.display().to_string(),
            writable(&env.cache_root).is_ok(),
            format!("error: {e}"),
            false,
        ),
    };

    let checks: Vec<(&str, String, bool)> = vec![
        ("tamias", env!("CARGO_PKG_VERSION").to_string(), true),
        ("git", git_detail, git_ok),
        ("store", store_detail, store_ok),
        ("packages", packages_detail, packages_ok),
        ("cache", cache_detail, cache_ok),
        ("cache entries", cache_entries_detail, cache_entries_ok),
        ("registry", registry_detail, registry_ok),
        (
            "drivers",
            if drivers.is_empty() {
                format!("files: {files_detail}")
            } else {
                format!(
                    "files: {files_detail}; {}",
                    drivers
                        .iter()
                        .map(|(k, _)| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
            files_ok,
        ),
    ];
    if json {
        let value: Vec<serde_json::Value> = checks
            .iter()
            .map(|(k, v, ok)| serde_json::json!({"check": k, "detail": v, "ok": ok}))
            .collect();
        print_json(&serde_json::Value::Array(value))?;
    } else {
        for (k, v, ok) in &checks {
            println!("{} {k}: {v}", if *ok { "ok " } else { "!! " });
        }
    }
    let failed = checks.iter().filter(|(_, _, ok)| !*ok).count();
    if failed > 0 {
        return Err(Error::Failed(format!("doctor: {failed} check(s) failed")));
    }
    Ok(())
}

fn main() -> ExitCode {
    // A broken pipe on stdout (`tamias list | head`) must not abort the run.
    sigpipe::reset();
    // Opt-in OTLP export; with no endpoint this is the plain tracing setup.
    let _telemetry = tamias_telemetry::init();
    run_main(Cli::parse(), Env::from_env())
}

/// Testable entry — `main` is a thin wrapper so the exit-code + error-print
/// arms are covered in-process.
fn run_main(cli: Cli, env: Env) -> ExitCode {
    match run(cli, env) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tamias: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    /// Serializes tests that mutate the process working directory.
    static CWD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    /// Serializes tests that mutate `TAMIAS_HOME`/`TAMIAS_ALLOW_DRIVERS`.
    static DRVENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env(root: &std::path::Path) -> Env {
        Env {
            store_root: root.join("store"),
            cache_root: root.join("cache"),
            registry_root: root.join("registry"),
        }
    }

    fn run_args(env: Env, args: &[&str]) -> Result<(), Error> {
        run(Cli::try_parse_from(args).unwrap(), env)
    }

    fn pkg(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tamias.yaml"), "name: w\nversion: 1.0.0\n").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        dir
    }

    #[test]
    fn verbs_happy_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
        run_args(e.clone(), &["tamias", "version"]).unwrap();
        run_args(e.clone(), &["tamias", "manifest"]).unwrap();
        run_args(e.clone(), &["tamias", "list"]).unwrap();
        run_args(e.clone(), &["tamias", "cache", "clean"]).unwrap();
        run_args(e.clone(), &["tamias", "doctor"]).unwrap();
        run_args(e.clone(), &["tamias", "doctor", "--json"]).unwrap();
        let out = tmp.path().join("w.tgz");
        run_args(
            e.clone(),
            &[
                "tamias",
                "bundle",
                "pack",
                dir.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        )
        .unwrap();
        run_args(e.clone(), &["tamias", "publish", dir.to_str().unwrap()]).unwrap();
        run_args(e.clone(), &["tamias", "search", "w"]).unwrap();
        let worker = tmp.path().join("worker");
        run_args(
            e.clone(),
            &[
                "tamias",
                "deploy-registry",
                "--out",
                worker.to_str().unwrap(),
            ],
        )
        .unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        run_args(e.clone(), &["tamias", "info", "w"]).unwrap();
        run_args(e.clone(), &["tamias", "verify", "w"]).unwrap();
        run_args(e.clone(), &["tamias", "deploy", "w"]).unwrap();
        run_args(e.clone(), &["tamias", "remove", "w"]).unwrap();
    }

    #[test]
    fn verbs_error_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        assert!(run_args(e.clone(), &["tamias", "info", "ghost"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "verify", "ghost"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "remove", "ghost"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "install", "not-a-ref"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "deploy", "ghost"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "uninstall", "ghost"]).is_err());
    }

    #[test]
    fn install_unknown_kind_fails_without_entry() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            "name: u\nversion: 1.0.0\ntype: zzz-no-driver\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let err = run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap_err();
        assert!(err.to_string().contains("zzz-no-driver"), "{err}");
        assert!(
            err.to_string().contains("tamias-driver-zzz-no-driver"),
            "{err}"
        );
        // nothing committed
        assert!(Store::new(&e.store_root).find("u").is_err());
        // a `files` package still installs with no driver
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("tamias.yaml"), "name: p\nversion: 1.0.0\n").unwrap();
        run_args(e.clone(), &["tamias", "install", plain.to_str().unwrap()]).unwrap();
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn uninstall_driver_failure_keeps_entry() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        let dd = tmp.path().join("th/drivers");
        std::fs::create_dir_all(&dd).unwrap();
        let driver = dd.join("tamias-driver-flaky");
        std::fs::write(
            &driver,
            "#!/bin/sh\ncase \"$1\" in\n  uninstall) exit 1;;\n  *) exit 0;;\nesac\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            "name: fl\nversion: 1.0.0\ntype: flaky\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        // driver teardown fails → uninstall fails, entry kept
        assert!(run_args(e.clone(), &["tamias", "uninstall", "fl"]).is_err());
        assert!(Store::new(&e.store_root).find("fl").is_ok());
        // driver gone → Ok(false): warn + still remove
        std::fs::remove_file(&driver).unwrap();
        run_args(e.clone(), &["tamias", "uninstall", "fl"]).unwrap();
        assert!(Store::new(&e.store_root).find("fl").is_err());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn install_rejects_empty_name() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tamias.yaml"), "version: 1.0.0\n").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        assert!(run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).is_err());
        assert!(Store::new(&e.store_root).entries().unwrap().is_empty());
    }

    #[test]
    fn more_verbs() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        run_args(e.clone(), &["tamias", "cache", "clean", "--days", "1"]).unwrap();
        let f = e.store_root.join("w@1.0.0/a.txt");
        std::fs::write(&f, "tampered").unwrap();
        assert!(run_args(e.clone(), &["tamias", "verify", "w"]).is_err());
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(run_args(
            e.clone(),
            &["tamias", "bundle", "pack", empty.to_str().unwrap()]
        )
        .is_err());
        assert!(run_args(e.clone(), &["tamias", "publish", empty.to_str().unwrap()]).is_err());
    }

    #[test]
    fn add_records_manifest_and_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
        let _g = CWD.lock().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
        std::env::set_current_dir(&prev).unwrap();
        result.unwrap();
        assert!(tmp.path().join("tamias.yaml").is_file());
        assert!(tmp.path().join("tamias.lock").is_file());
    }

    #[test]
    fn install_from_git_and_http() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        // git: file:// repo → covers the Git resolve (ls-remote) + fetch (clone) arms.
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("tamias.yaml"), "name: g\nversion: 1.0.0\n").unwrap();
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q", "-b", "main"]));
        assert!(git(&["add", "-A"]));
        assert!(git(&["commit", "-q", "-m", "i"]));
        let g = format!("git:file://{}#main", repo.display());
        run_args(e.clone(), &["tamias", "install", &g]).unwrap();
        assert!(run_args(e.clone(), &["tamias", "info", "g"]).is_ok());

        // http: a served tarball → covers the Http resolve + fetch arms.
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("tamias.yaml"), "name: h\nversion: 1.0.0\n").unwrap();
        std::fs::write(src.join("a.txt"), "a").unwrap();
        let reg = tmp.path().join("reg");
        std::fs::create_dir_all(&reg).unwrap();
        tamias_core::pack_tgz(&src, &reg.join("h.tgz")).unwrap();
        let root = reg.clone();
        std::thread::spawn(move || {
            let _ = tamias_registry_local::serve_http("127.0.0.1:8792", &root);
        });
        std::thread::sleep(Duration::from_millis(400));
        run_args(
            e.clone(),
            &["tamias", "install", "http://127.0.0.1:8792/h.tgz"],
        )
        .unwrap();
        assert!(run_args(e.clone(), &["tamias", "info", "h"]).is_ok());
    }

    #[test]
    fn corrupt_cache_entry_never_installs() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        // git ref → prefetch identity → the `hit()` path
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("tamias.yaml"), "name: cg\nversion: 1.0.0\n").unwrap();
        std::fs::write(repo.join("a.txt"), "good").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q", "-b", "main"]));
        assert!(git(&["add", "-A"]));
        assert!(git(&["commit", "-q", "-m", "i"]));
        let g = format!("git:file://{}#main", repo.display());
        run_args(e.clone(), &["tamias", "install", &g]).unwrap();
        // poison the committed cache entry, then reinstall: `hit` must
        // reject it, evict, and refetch — the store gets the good bytes.
        let cache = FetchCache::new(&e.cache_root);
        let entries = cache.entries().unwrap();
        assert_eq!(entries.len(), 1);
        std::fs::write(entries[0].join("a.txt"), "evil").unwrap();
        run_args(e.clone(), &["tamias", "install", &g]).unwrap();
        let entry = Store::new(&e.store_root).find("cg").unwrap();
        assert_eq!(
            std::fs::read_to_string(entry.dir.join("a.txt")).unwrap(),
            "good"
        );
        run_args(e.clone(), &["tamias", "verify", "cg"]).unwrap();

        // same poison, but the source is gone too → hard error, never
        // a silent install of the corrupted entry.
        std::fs::write(entries[0].join("a.txt"), "evil").unwrap();
        std::fs::remove_dir_all(&repo).unwrap();
        assert!(run_args(e.clone(), &["tamias", "install", &g]).is_err());
        assert_eq!(
            std::fs::read_to_string(entry.dir.join("a.txt")).unwrap(),
            "good"
        );
    }

    #[test]
    fn corrupt_cache_entry_replaced_on_refetch() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        // fs refs carry no prefetch identity — every install re-fetches
        // and `commit` must swap the poisoned entry for fresh bytes.
        let dir = pkg(tmp.path());
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        let cache = FetchCache::new(&e.cache_root);
        let entries = cache.entries().unwrap();
        assert_eq!(entries.len(), 1);
        std::fs::write(entries[0].join("a.txt"), "evil").unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        let entry = Store::new(&e.store_root).find("w").unwrap();
        assert_eq!(
            std::fs::read_to_string(entry.dir.join("a.txt")).unwrap(),
            "a"
        );
        run_args(e.clone(), &["tamias", "verify", "w"]).unwrap();
    }

    #[test]
    fn blocking_arms_execute() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = tmp.path().to_path_buf();
        let e1 = e.clone();
        let d1 = dir.clone();
        std::thread::spawn(move || {
            let _ = run_args(
                e1,
                &[
                    "tamias",
                    "registry",
                    "serve",
                    "--addr",
                    "127.0.0.1:0",
                    "--dir",
                    d1.to_str().unwrap(),
                ],
            );
        });
        let e2 = e.clone();
        std::thread::spawn(move || {
            let _ = run_args(e2, &["tamias", "serve"]);
        });
        std::thread::sleep(Duration::from_millis(300));
    }

    #[test]
    fn service_arms() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        // These reach the real systemctl runner (may error in a sandbox) —
        // the dispatch arm executes regardless.
        let _ = run_args(e.clone(), &["tamias", "service", "status", "x"]);
        let _ = run_args(e.clone(), &["tamias", "service", "logs", "x"]);
        let _ = run_args(e.clone(), &["tamias", "service", "disable", "x"]);
        let _ = run_args(e.clone(), &["tamias", "service", "uninstall", "x"]);
        let _ = run_args(e.clone(), &["tamias", "service", "enable", "x"]);
    }

    #[test]
    fn run_main_exit_codes() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let ok = Cli::try_parse_from(["tamias", "version"]).unwrap();
        assert!(matches!(run_main(ok, e.clone()), ExitCode::SUCCESS));
        let bad = Cli::try_parse_from(["tamias", "info", "ghost"]).unwrap();
        assert!(matches!(run_main(bad, e), ExitCode::FAILURE));
    }

    #[test]
    fn env_root_fallbacks() {
        let _g = DRVENV.lock().unwrap();
        for key in [
            "TAMIAS_HOME",
            "TAMIAS_CACHE",
            "TAMIAS_REGISTRY",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "HOME",
        ] {
            std::env::remove_var(key);
        }
        std::env::set_var("TAMIAS_HOME", "/th");
        std::env::set_var("TAMIAS_CACHE", "/tc");
        std::env::set_var("TAMIAS_REGISTRY", "/tr");
        let _ = (store_root(), cache_root(), registry_root());
        std::env::remove_var("TAMIAS_HOME");
        std::env::remove_var("TAMIAS_CACHE");
        std::env::remove_var("TAMIAS_REGISTRY");
        std::env::set_var("XDG_DATA_HOME", "/xd");
        std::env::set_var("XDG_CACHE_HOME", "/xc");
        let _ = (store_root(), cache_root(), registry_root());
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var("XDG_CACHE_HOME");
        std::env::set_var("HOME", "/h");
        let _ = (store_root(), cache_root(), registry_root());
        std::env::remove_var("HOME");
        let _ = (store_root(), cache_root(), registry_root());
    }

    #[test]
    fn cache_hit_fetch_error_and_scoped_info() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            "name: w\nscope: acme\nversion: 1.0.0\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        run_args(e.clone(), &["tamias", "info", "w"]).unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("tamias.yaml"), "name: g\nversion: 1.0.0\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q", "-b", "main"]));
        assert!(git(&["add", "-A"]));
        assert!(git(&["commit", "-q", "-m", "i"]));
        let g = format!("git:file://{}#main", repo.display());
        run_args(e.clone(), &["tamias", "install", &g]).unwrap();
        run_args(e.clone(), &["tamias", "install", &g]).unwrap();
        assert!(run_args(
            e.clone(),
            &["tamias", "install", "http://127.0.0.1:1/x.tgz"]
        )
        .is_err());
        run_args(e.clone(), &["tamias", "cache", "clean", "--all"]).unwrap();
    }

    #[test]
    fn service_install_and_doctor_with_driver() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("cfg"));
        let r = run_args(
            e.clone(),
            &[
                "tamias",
                "service",
                "install",
                "--unit",
                "ufx",
                "--exec",
                "/bin/true",
                "--env",
                "A=1",
            ],
        );
        std::env::remove_var("XDG_CONFIG_HOME");
        r.unwrap();
        assert!(tmp.path().join("cfg/systemd/user/ufx.service").is_file());
        let _ = run_args(e.clone(), &["tamias", "cache", "clean"]);
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        let dd = tmp.path().join("th/drivers");
        std::fs::create_dir_all(&dd).unwrap();
        std::fs::write(dd.join("tamias-driver-x"), "#!/bin/sh\n").unwrap();
        run_args(e.clone(), &["tamias", "doctor"]).unwrap();
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn add_with_existing_manifest_and_service_verbs() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
        {
            let _g = CWD.lock().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();
            std::fs::write(
                tmp.path().join("tamias.yaml"),
                "name: proj\nversion: 1.0.0\n",
            )
            .unwrap();
            let result = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
            std::env::set_current_dir(&prev).unwrap();
            result.unwrap();
        }
        // stub `systemctl` on PATH → service enable/disable print their lines
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("systemctl");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut newp = bin.clone().into_os_string();
        newp.push(":");
        newp.push(&old);
        std::env::set_var("PATH", newp);
        let r1 = run_args(e.clone(), &["tamias", "service", "enable", "foo"]);
        let r2 = run_args(e.clone(), &["tamias", "service", "disable", "foo"]);
        std::env::set_var("PATH", old);
        r1.unwrap();
        r2.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn error_arms_unreadable_and_unwritable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o000)).unwrap();
        let e = Env {
            store_root: store.clone(),
            cache_root: tmp.path().join("cache"),
            registry_root: tmp.path().join("reg"),
        };
        assert!(run_args(e.clone(), &["tamias", "list"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "doctor"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "info", "w"]).is_err());
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ro = tmp.path().join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let e2 = Env {
            store_root: tmp.path().join("store2"),
            cache_root: ro.join("cache"),
            registry_root: tmp.path().join("reg2"),
        };
        let pkgdir = pkg(tmp.path());
        assert!(run_args(e2.clone(), &["tamias", "install", pkgdir.to_str().unwrap()]).is_err());
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();

        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let e3 = env(tmp.path());
        assert!(run_args(e3.clone(), &["tamias", "publish", empty.to_str().unwrap()]).is_err());
        assert!(run_args(e3.clone(), &["tamias", "install", empty.to_str().unwrap()]).is_err());
        assert!(run_args(e3.clone(), &["tamias", "verify", "ghost"]).is_err());
        assert!(run_args(e3.clone(), &["tamias", "remove", "ghost"]).is_err());

        // cache dir exists but is unreadable → cache clean fails
        let cdir = tmp.path().join("cdir");
        std::fs::create_dir_all(cdir.join("k")).unwrap();
        std::fs::set_permissions(&cdir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let e4 = Env {
            store_root: tmp.path().join("s4"),
            cache_root: cdir.clone(),
            registry_root: tmp.path().join("r4"),
        };
        assert!(run_args(e4.clone(), &["tamias", "cache", "clean", "--all"]).is_err());
        assert!(run_args(e4.clone(), &["tamias", "cache", "clean", "--days", "1"]).is_err());
        std::fs::set_permissions(&cdir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn doctor_and_add_save_errors() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = pkg(tmp.path());
        // doctor: unreadable cache
        let cdir = tmp.path().join("c");
        std::fs::create_dir_all(&cdir).unwrap();
        let sd = tmp.path().join("s");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::set_permissions(&cdir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let e = Env {
            store_root: sd,
            cache_root: cdir.clone(),
            registry_root: tmp.path().join("r"),
        };
        assert!(run_args(e.clone(), &["tamias", "doctor"]).is_err());
        std::fs::set_permissions(&cdir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // add: unwritable cwd → manifest save fails
        let ro = tmp.path().join("rodir");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        {
            let _g = CWD.lock().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(&ro).unwrap();
            let r = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
            std::env::set_current_dir(&prev).unwrap();
            assert!(r.is_err());
        }
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn more_error_arms() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
        // unresolvable git ref
        assert!(run_args(
            e.clone(),
            &["tamias", "install", "git:file:///nonexistent-xyz-999#main"]
        )
        .is_err());
        // bad manifest → install + publish
        let badm = tmp.path().join("badm");
        std::fs::create_dir_all(&badm).unwrap();
        std::fs::write(badm.join("tamias.yaml"), "name: \"-bad\"\n").unwrap();
        assert!(run_args(e.clone(), &["tamias", "install", badm.to_str().unwrap()]).is_err());
        assert!(run_args(e.clone(), &["tamias", "publish", badm.to_str().unwrap()]).is_err());
        // publish to a dead http registry
        assert!(run_args(
            e.clone(),
            &[
                "tamias",
                "publish",
                dir.to_str().unwrap(),
                "--registry",
                "http://127.0.0.1:1"
            ]
        )
        .is_err());
        // search a registry with a broken index
        let reg = tmp.path().join("reg");
        std::fs::create_dir_all(&reg).unwrap();
        std::fs::write(reg.join("index.json"), "nope").unwrap();
        assert!(run_args(
            e.clone(),
            &["tamias", "search", "w", "--registry", reg.to_str().unwrap()]
        )
        .is_err());
        // add: pre-existing bad manifest, then a bad lock
        {
            let _g = CWD.lock().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();
            std::fs::write(tmp.path().join("tamias.yaml"), "name: \"-x\"\n").unwrap();
            let r = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
            std::env::set_current_dir(&prev).unwrap();
            assert!(r.is_err());
        }
        {
            let _g = CWD.lock().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();
            std::fs::write(tmp.path().join("tamias.yaml"), "name: proj\n").unwrap();
            std::fs::write(tmp.path().join("tamias.lock"), ": bad : yaml :").unwrap();
            let r = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
            std::env::set_current_dir(&prev).unwrap();
            assert!(r.is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn service_status_logs_uninstall() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("systemctl");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut newp = bin.clone().into_os_string();
        newp.push(":");
        newp.push(&old);
        std::env::set_var("PATH", newp);
        let e = env(tmp.path());
        let r1 = run_args(e.clone(), &["tamias", "service", "status", "foo"]);
        let r2 = run_args(e.clone(), &["tamias", "service", "logs", "foo"]);
        let r3 = run_args(e.clone(), &["tamias", "service", "uninstall", "foo"]);
        std::env::set_var("PATH", old);
        r1.unwrap();
        r2.unwrap();
        r3.unwrap();
    }

    #[test]
    fn update_pin_schema_and_cli_docs() {
        // update/pin consult the cwd `tamias.lock` — hold the cwd lock so
        // a sibling test's set_current_dir can't swap in a foreign lock.
        let _g = CWD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        // list --type filters (match + non-match)
        run_args(e.clone(), &["tamias", "list", "--type", "files"]).unwrap();
        run_args(e.clone(), &["tamias", "list", "--type", "none"]).unwrap();
        // update one / all
        run_args(e.clone(), &["tamias", "update", "w"]).unwrap();
        run_args(e.clone(), &["tamias", "update", "--all"]).unwrap();
        // pin retargets the recorded ref
        run_args(e.clone(), &["tamias", "pin", "w", "git:9aia/x//y#v1"]).unwrap();
        assert_eq!(
            Store::new(&e.store_root)
                .find("w")
                .unwrap()
                .record
                .source_ref,
            "git:9aia/x//y#v1"
        );
        // schema + gen-cli-docs + completions
        run_args(e.clone(), &["tamias", "schema"]).unwrap();
        run_args(e.clone(), &["tamias", "gen-cli-docs"]).unwrap();
        run_args(e.clone(), &["tamias", "completions", "bash"]).unwrap();
        run_args(e.clone(), &["tamias", "completions", "zsh"]).unwrap();
        // update with neither a name nor --all → error
        assert!(run_args(e.clone(), &["tamias", "update"]).is_err());
        // pin a ghost → error
        assert!(run_args(e.clone(), &["tamias", "pin", "ghost", "./x"]).is_err());
    }

    #[test]
    fn config_verb() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        // YAML (the standard) via --file, text + json
        let yml = tmp.path().join("tamias.yml");
        std::fs::write(&yml, "store:\n  kind: local\npublish:\n  default: cloud\n").unwrap();
        run_args(
            e.clone(),
            &["tamias", "config", "--file", yml.to_str().unwrap()],
        )
        .unwrap();
        run_args(
            e.clone(),
            &[
                "tamias",
                "config",
                "--file",
                yml.to_str().unwrap(),
                "--json",
            ],
        )
        .unwrap();
        // TOML (the alternative) via --file
        let toml_ = tmp.path().join("tamias.toml");
        std::fs::write(&toml_, "[store]\nkind = \"local\"\n").unwrap();
        run_args(
            e.clone(),
            &["tamias", "config", "--file", toml_.to_str().unwrap()],
        )
        .unwrap();
        // unclaimed extension → error
        let json = tmp.path().join("x.json");
        std::fs::write(&json, "{}").unwrap();
        assert!(run_args(
            e.clone(),
            &["tamias", "config", "--file", json.to_str().unwrap()]
        )
        .is_err());
        // malformed YAML → error
        let bad = tmp.path().join("bad.yml");
        std::fs::write(&bad, ":\n  - [").unwrap();
        assert!(run_args(
            e.clone(),
            &["tamias", "config", "--file", bad.to_str().unwrap()]
        )
        .is_err());
        // discovery: a dir with a config (Some) and an empty dir (None → default)
        {
            let _g = CWD.lock().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();
            run_args(e.clone(), &["tamias", "config"]).unwrap();
            run_args(e.clone(), &["tamias", "config", "--json"]).unwrap();
            let empty = tmp.path().join("empty");
            std::fs::create_dir_all(&empty).unwrap();
            std::env::set_current_dir(&empty).unwrap();
            run_args(e.clone(), &["tamias", "config"]).unwrap();
            std::env::set_current_dir(&prev).unwrap();
        }
    }

    #[test]
    fn install_gates_bundled_drivers() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let e = env(tmp.path());
        // a package shipping a bundled driver
        let dir = tmp.path().join("pkgd");
        std::fs::create_dir_all(dir.join("drivers")).unwrap();
        std::fs::write(dir.join("tamias.yaml"), "name: d\nversion: 1.0.0\n").unwrap();
        std::fs::write(
            dir.join("drivers/tamias-driver-files"),
            "#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        let r = dir.to_str().unwrap();
        // no consent → fails before the store entry; nothing registered
        assert!(run_args(e.clone(), &["tamias", "install", r]).is_err());
        assert!(Store::new(&e.store_root).find("d").is_err());
        assert!(!tmp.path().join("th/drivers/tamias-driver-files").exists());
        // env consent → proceeds; driver registered + dispatched
        std::env::set_var("TAMIAS_ALLOW_DRIVERS", "1");
        run_args(e.clone(), &["tamias", "install", r]).unwrap();
        assert!(tmp.path().join("th/drivers/tamias-driver-files").is_file());
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        // flag consent → proceeds too
        run_args(e.clone(), &["tamias", "install", r, "--allow-drivers"]).unwrap();
        // a drivers-shipping package also gates `add`
        {
            let _c = CWD.lock().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();
            let res = run_args(e.clone(), &["tamias", "add", r]);
            std::env::set_current_dir(&prev).unwrap();
            assert!(res.is_err());
        }
        // a plain files package (no drivers/) is unaffected — no flag needed
        let plain = pkg(tmp.path());
        run_args(e.clone(), &["tamias", "install", plain.to_str().unwrap()]).unwrap();
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn install_from_cf_registry() {
        use tamias_registry_local::{serve_http, LocalRegistry};
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let src = pkg(tmp.path());
        let reg = tmp.path().join("reg");
        let manifest = Manifest::load(&src).unwrap();
        LocalRegistry::new(&reg).publish(&src, &manifest).unwrap();
        let served = reg.clone();
        std::thread::spawn(move || {
            let _ = serve_http("127.0.0.1:8903", &served);
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect("127.0.0.1:8903").is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        run_args(
            e.clone(),
            &["tamias", "install", "cf:http://127.0.0.1:8903/w@1.0.0"],
        )
        .unwrap();
        assert!(Store::new(&e.store_root).find("w").is_ok());
        // a version the registry doesn't have → error
        assert!(run_args(
            e.clone(),
            &["tamias", "install", "cf:http://127.0.0.1:8903/w@9.9.9"],
        )
        .is_err());
    }

    #[test]
    fn manifest_verbs_cover_every_subcommand() {
        // The generated list must name every top-level verb — including
        // the ones the old hardcoded list dropped.
        assert_eq!(
            cli_verbs(),
            [
                "install",
                "add",
                "remove",
                "list",
                "info",
                "verify",
                "update",
                "pin",
                "cache",
                "version",
                "manifest",
                "schema",
                "gen-cli-docs",
                "completions",
                "doctor",
                "config",
                "serve",
                "publish",
                "search",
                "bundle",
                "registry",
                "deploy-registry",
                "deploy",
                "uninstall",
                "service",
            ]
        );
    }

    #[test]
    fn cache_clean_all_conflicts_with_days() {
        assert!(Cli::try_parse_from(["tamias", "cache", "clean", "--all", "--days", "3"]).is_err());
        assert!(Cli::try_parse_from(["tamias", "cache", "clean", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["tamias", "cache", "clean", "--days", "3"]).is_ok());
    }

    #[test]
    fn sync_lock_only_refreshes_pinned_entries() {
        let _g = CWD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let record = |source_ref: &str, digest: &str| StoreRecord {
            name: "w".into(),
            scope: None,
            version: Some("1.0.0".into()),
            kind: None,
            source_ref: source_ref.into(),
            resolved: "sha256:resolved".into(),
            digest: digest.into(),
            installed_at: 0,
        };
        // no lockfile → left alone, never created
        let r1 = sync_lock(&record("./a", "sha256:one"));
        std::env::set_current_dir(&prev).unwrap();
        r1.unwrap();
        assert!(!tmp.path().join("tamias.lock").exists());

        // lock exists but doesn't pin `w` → untouched
        let lock_path = tmp.path().join("tamias.lock");
        let mut lock = Lock::default();
        lock.upsert(LockEntry {
            name: "other".into(),
            scope: None,
            version: None,
            source_ref: "./o".into(),
            resolved: "r".into(),
            digest: "d".into(),
        });
        lock.save(&lock_path).unwrap();
        let before = std::fs::read_to_string(&lock_path).unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let r2 = sync_lock(&record("./a", "sha256:one"));
        std::env::set_current_dir(&prev).unwrap();
        r2.unwrap();
        assert_eq!(std::fs::read_to_string(&lock_path).unwrap(), before);

        // lock pins `w` → entry refreshed to the new record
        lock.upsert(LockEntry::from_record(&record("./a", "sha256:one")));
        lock.save(&lock_path).unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let r3 = sync_lock(&record("./b", "sha256:two"));
        std::env::set_current_dir(&prev).unwrap();
        r3.unwrap();
        let after = Lock::load(&lock_path).unwrap();
        let w = after
            .packages
            .iter()
            .find(|e| e.name == "w")
            .expect("w entry");
        assert_eq!(w.source_ref, "./b");
        assert_eq!(w.digest, "sha256:two");
    }

    #[test]
    fn update_and_pin_sync_tamias_lock() {
        let _g = CWD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&proj).unwrap();
        let r_add = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
        // corrupt the locked digest → `update` must repair it
        let lock_path = proj.join("tamias.lock");
        let mut lock = Lock::load(&lock_path).unwrap();
        lock.packages[0].digest = "sha256:stale".into();
        lock.save(&lock_path).unwrap();
        let r_update = run_args(e.clone(), &["tamias", "update", "w"]);
        // `pin` retargets the ref → the lock follows
        let r_pin = run_args(e.clone(), &["tamias", "pin", "w", "./other"]);
        std::env::set_current_dir(&prev).unwrap();
        r_add.unwrap();
        r_update.unwrap();
        r_pin.unwrap();
        let lock = Lock::load(&lock_path).unwrap();
        let entry = &lock.packages[0];
        assert_eq!(entry.source_ref, "./other");
        assert!(entry.digest.starts_with("sha256:"));
        assert_ne!(entry.digest, "sha256:stale");
        // and the store record agrees
        assert_eq!(
            Store::new(&e.store_root)
                .find("w")
                .unwrap()
                .record
                .source_ref,
            "./other"
        );
    }

    #[cfg(unix)]
    #[test]
    fn doctor_fails_on_unwritable_roots() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // read+execute but not write → entries() works, the writable
        // probe fails → doctor must exit non-zero.
        let store = tmp.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o500)).unwrap();
        let e = Env {
            store_root: store.clone(),
            cache_root: tmp.path().join("cache"),
            registry_root: tmp.path().join("reg"),
        };
        assert!(run_args(e.clone(), &["tamias", "doctor"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "doctor", "--json"]).is_err());
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
        run_args(e.clone(), &["tamias", "doctor"]).unwrap();
        // a store root that isn't a dir at all → also fails
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let e2 = Env {
            store_root: file,
            cache_root: tmp.path().join("cache2"),
            registry_root: tmp.path().join("reg2"),
        };
        assert!(run_args(e2, &["tamias", "doctor"]).is_err());
    }
}
