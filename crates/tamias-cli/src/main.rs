#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias — the CLI. P2b: the distribution verbs over the source port, the
//! content-addressed cache, and the local store. PL-008 added the
//! operator surface for the provider model: `drivers` (discovery +
//! capabilities), `provider list` (installed `provider:` blocks),
//! `run`/`logs` (foreground exec + journal over provider services), and
//! `service`/`deploy`/`uninstall` for the systemd units they render.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tamias_cache_local::FetchCache;
use tamias_config::{ConfigFormat, CONFIG_NAMES};
use tamias_config_toml::TomlFormat;
use tamias_config_yml::YmlFormat;
use tamias_core::{payload_root, Error, Lock, LockEntry, Manifest, StoreEntry, StoreRecord};
use tamias_install::{Progress, Surface};
use tamias_registry::{parse_registry_ref, Registry, RegistryRef};
use tamias_registry_cf::HttpRegistry;
use tamias_registry_local::{serve_http, LocalRegistry};
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
    /// List discovered content drivers (`tamias-driver-<type>` on PATH +
    /// `<tamias-home>/drivers/`).
    Drivers {
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Provider-verb dispatch — run a driver verb beyond the lifecycle
    /// three (`install`/`deploy`/`uninstall`).
    Driver {
        #[command(subcommand)]
        cmd: DriverCommand,
    },
    /// Provider packages — the declarative `provider:` runtime spec.
    Provider {
        #[command(subcommand)]
        cmd: ProviderCommand,
    },
    /// Run an installed provider service's exec in the foreground —
    /// the unit's argv + env, without systemd. `tamias run <pkg>
    /// [service] [-- extra-args]`.
    Run {
        /// `@scope/name` or `name` — the package must declare `provider:`.
        name: String,
        /// Service name — required when the provider declares several.
        service: Option<String>,
        /// Extra argv appended to the service exec (after `--`).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Follow a package's provider units' journals — `tamias logs
    /// <pkg|unit>`. A package name resolves to all its provider units;
    /// anything else is treated as a unit base name.
    Logs {
        /// Package name or unit base name.
        name: String,
        /// Journal lines to show.
        #[arg(long, default_value_t = 200)]
        lines: u32,
        /// Follow the journal (`journalctl -f`).
        #[arg(long, short)]
        follow: bool,
    },
}

#[derive(Subcommand)]
enum DriverCommand {
    /// Dispatch a provider verb to an installed package's kind driver —
    /// `tamias driver exec <pkg> <verb> [args…]`. The verb and any args
    /// pass through verbatim after the standard `--dir`/`--name`
    /// flags; the driver's stdout/stderr stream back. `tamias drivers`
    /// lists each driver's declared verbs; `tamias run` stays the
    /// local service-exec surface — provider-specific ops (hook
    /// registration, health probes, unit status) live here.
    Exec {
        /// `@scope/name` or `name` — the installed package.
        name: String,
        /// The driver verb (`hooks`, `health`, `status`, …).
        verb: String,
        /// Node/config file passed through as the driver's `--config`.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Everything after the verb — driver flags/sub-verbs
        /// (`register --dry-run`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ProviderCommand {
    /// List installed packages' `provider:` blocks — services (with
    /// unit state), ingress URLs, verbs, health probe.
    List {
        /// Also list packages without a `provider:` block.
        #[arg(long)]
        all: bool,
        /// JSON output.
        #[arg(long)]
        json: bool,
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
        // The project config (`./tamias.{yml,yaml,toml}`) fills roots the
        // env vars leave unset — explicit `TAMIAS_*`/XDG vars win. A
        // broken config warns and is ignored rather than failing every
        // verb before it starts.
        let config = cli_config();
        Self {
            store_root: store_root(config.as_ref()),
            cache_root: cache_root(config.as_ref()),
            registry_root: registry_root(),
        }
    }
}

/// The effective config file in the cwd, if any — the same discovery the
/// `config` verb uses. Parse/read failures warn and yield `None`.
fn cli_config() -> Option<tamias_config::Config> {
    let formats: [&dyn ConfigFormat; 2] = [&YmlFormat, &TomlFormat];
    match tamias_config::load_in(Path::new("."), &formats) {
        Ok(found) => found.map(|(_, c)| c),
        Err(e) => {
            eprintln!("tamias: warning — ignoring config file: {e}");
            None
        }
    }
}

/// A config `Location` → its `root` as a path. Only `local` (or unset)
/// kinds resolve — `cf` and anything else are reserved (T-015): warn and
/// yield `None` so the caller falls through to env/defaults instead of
/// silently pointing at a backend that isn't wired.
fn location_root(loc: &tamias_config::Location, what: &str) -> Option<String> {
    match loc.kind.as_deref() {
        None | Some("local") => loc.root.clone(),
        Some(kind) => {
            eprintln!(
                "tamias: warning — {what}.kind '{kind}' is not wired yet; ignoring {what}.root"
            );
            None
        }
    }
}

fn store_root(config: Option<&tamias_config::Config>) -> PathBuf {
    if let Some(root) = std::env::var_os("TAMIAS_HOME") {
        return PathBuf::from(root).join("store");
    }
    if let Some(root) = config.and_then(|c| location_root(&c.store, "store")) {
        return PathBuf::from(root);
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(|d| PathBuf::from(d).join("tamias/store"))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/tamias/store"))
        })
        .unwrap_or_else(|| PathBuf::from(".tamias/store"))
}

fn cache_root(config: Option<&tamias_config::Config>) -> PathBuf {
    if let Some(root) = std::env::var_os("TAMIAS_CACHE") {
        return PathBuf::from(root);
    }
    if let Some(root) = config.and_then(|c| location_root(&c.cache, "cache")) {
        return PathBuf::from(root);
    }
    std::env::var_os("XDG_CACHE_HOME")
        .map(|d| PathBuf::from(d).join("tamias/fetch"))
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

/// `install`/`add`/`update` — the shared pipeline in `tamias-install`,
/// rendered with the CLI's progress lines (`tamias:` → stderr, driver
/// actions → stdout).
fn install(env: &Env, reference: &str, allow_drivers: bool) -> Result<StoreEntry, Error> {
    tamias_install::install_with_progress(
        &tamias_install::Env {
            store_root: env.store_root.clone(),
            cache_root: env.cache_root.clone(),
        },
        reference,
        allow_drivers,
        Surface::Cli,
        &mut |progress| match progress {
            Progress::Fetching { reference } => eprintln!("tamias: fetching {reference}"),
            Progress::Installing { label } => eprintln!("tamias: installing {label}"),
            Progress::DriverAction { action } => println!("  {action}"),
            Progress::Done { label } => eprintln!("tamias: done — {label}"),
        },
    )
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
        Some(Command::Drivers { json }) => {
            drivers_list(json)?;
        }
        Some(Command::Driver { cmd }) => {
            let DriverCommand::Exec {
                name,
                verb,
                config,
                args,
            } = cmd;
            driver_exec(&env, &name, &verb, config.as_deref(), &args)?;
        }
        Some(Command::Provider { cmd }) => {
            let ProviderCommand::List { all, json } = cmd;
            provider_list(&env, all, json)?;
        }
        Some(Command::Run {
            name,
            service,
            args,
        }) => {
            run_provider_service(&env, &name, service.as_deref(), &args)?;
        }
        Some(Command::Logs {
            name,
            lines,
            follow,
        }) => {
            logs(&env, &name, lines, follow)?;
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
        /// `EnvironmentFile=` path (`~` expands; rendered optional `-`).
        #[arg(long)]
        env_file: Option<String>,
        /// `Restart=` policy (default `on-failure`).
        #[arg(long)]
        restart: Option<String>,
        /// Working directory (`~` expands to `$HOME`).
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
    Logs {
        unit: String,
        /// Journal lines to show.
        #[arg(long, default_value_t = 200)]
        lines: u32,
        /// Follow the journal (`journalctl -f`).
        #[arg(long, short)]
        follow: bool,
    },
    /// Disable + remove the unit file.
    Uninstall { unit: String },
}

/// The provider block of an installed package's manifest — `None` when
/// the payload carries no manifest or no `provider:` (a plain package
/// has nothing declarative to deploy).
fn provider_of(entry: &StoreEntry) -> Result<Option<Manifest>, Error> {
    if Manifest::path_in(&entry.dir).is_none() {
        return Ok(None);
    }
    let manifest = entry.manifest()?;
    Ok(manifest.provider.is_some().then_some(manifest))
}

/// The unit base name a provider service renders to —
/// `<provider-or-package>-<service>`. `provider.name` wins; otherwise
/// the package name (scope-qualified when scoped, so `@s/foo` and `foo`
/// don't collide). Deterministic: two packages claiming the same unit
/// name resolve last-deploy-wins on the unit file.
fn provider_unit_name(manifest: &Manifest, service: &str) -> String {
    format!("{}-{service}", provider_base(manifest))
}

/// Render a provider service as a [`ServiceSpec`] — exec string → argv
/// via [`tamias_deploy_local::split_exec`], `~` expansion deferred to
/// the renderer.
fn provider_service_spec(
    entry: &StoreEntry,
    manifest: &Manifest,
    service: &tamias_core::ProviderService,
) -> Result<tamias_deploy_local::ServiceSpec, Error> {
    Ok(tamias_deploy_local::ServiceSpec {
        name: provider_unit_name(manifest, &service.name),
        description: format!("{} — {}", entry.label(), service.name),
        exec: tamias_deploy_local::split_exec(&service.exec)?,
        env: service
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        env_file: service.env_file.clone(),
        restart: service.restart.clone(),
        restart_sec: None,
        workdir: service.workdir.clone(),
        after: vec![],
    })
}

/// `deploy <name>` — run the installed package's provider deploy step:
/// the kind driver's `deploy` verb (kind-specific setup/teardown) *and*,
/// independently, every `provider.services[]` entry rendered as a
/// per-user systemd unit. Units are written + daemon-reloaded but not
/// enabled — `tamias service enable <unit>` is the operator's step.
fn deploy_package(env: &Env, name: &str, config: Option<&Path>) -> Result<(), Error> {
    let entry = Store::new(&env.store_root).find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    let driver_ran =
        tamias_driver::dispatch(kind, "deploy", &entry.dir, &entry.record.name, config)?;
    if driver_ran {
        println!("deployed {}", entry.label());
    }
    let mut deployed = false;
    if let Some(manifest) = provider_of(&entry)? {
        let deploy = tamias_deploy_local::LocalDeploy::new();
        if let Some(provider) = &manifest.provider {
            for service in &provider.services {
                let spec = provider_service_spec(&entry, &manifest, service)?;
                let path = deploy.install(&spec)?;
                deployed = true;
                println!(
                    "unit {} → {} (enable: tamias service enable {})",
                    spec.name,
                    path.display(),
                    spec.name
                );
            }
        }
        if deployed {
            // Best-effort: a missing user bus must not fail the deploy —
            // the unit files are already on disk.
            let _ = deploy.daemon_reload();
        }
        // Ingress hooks stay manual for v1: `public_url` fronts a tunnel
        // the operator confirms first — auto-registering forge hooks at
        // deploy time would point repos at whatever URL happens to be
        // declared. Point at the explicit verb instead.
        let hooks: usize = manifest
            .provider
            .as_ref()
            .map(|p| p.ingress.iter().map(|i| i.hooks.len()).sum())
            .unwrap_or(0);
        if hooks > 0 {
            eprintln!(
                "tamias: {} ingress hook(s) declared — review with `tamias driver exec {} hooks register --dry-run`, then drop --dry-run to register",
                hooks,
                entry.label()
            );
        }
    }
    if !driver_ran && !deployed {
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
    // Declarative provider services are tamias-native — remove their
    // units too. Best-effort per unit: a unit that can't be removed
    // warns but doesn't block dropping the store entry.
    if let Some(manifest) = provider_of(&entry)? {
        let deploy = tamias_deploy_local::LocalDeploy::new();
        if let Some(provider) = &manifest.provider {
            for service in &provider.services {
                let unit = provider_unit_name(&manifest, &service.name);
                if let Err(e) = deploy.uninstall(&unit) {
                    eprintln!("tamias: warning — could not remove unit {unit}: {e}");
                }
            }
        }
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
            env_file,
            restart,
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
                exec: tamias_deploy_local::split_exec(&exec)?,
                env,
                env_file,
                restart,
                restart_sec: None,
                workdir,
                after: vec![],
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
        ServiceCommand::Logs {
            unit,
            lines,
            follow,
        } => deploy.logs_with(&[unit], lines, follow)?,
        ServiceCommand::Uninstall { unit } => {
            deploy.uninstall(&unit)?;
            println!("removed unit {unit}");
        }
    }
    Ok(())
}

/// `driver exec <pkg> <verb> [args…]` — the provider-verb passthrough:
/// resolves the installed package's store entry (dir + name + kind),
/// dispatches `<verb>` to its kind driver with `args` trailing the
/// standard flags, and renders the driver's captured output. Warns when
/// the driver's declared `capabilities.verbs` doesn't list the verb —
/// advisory only, the driver stays the authority.
fn driver_exec(
    env: &Env,
    name: &str,
    verb: &str,
    config: Option<&Path>,
    args: &[String],
) -> Result<(), Error> {
    let entry = Store::new(&env.store_root).find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    if let Some(driver) = tamias_driver::find_driver(kind) {
        if let Ok(caps) = tamias_driver::capabilities(&driver) {
            let verbs = caps.get("verbs").and_then(|v| v.as_array());
            let declared =
                verbs.is_some_and(|a| !a.is_empty() && a.iter().any(|v| v.as_str() == Some(verb)));
            if verbs.is_some_and(|a| !a.is_empty()) && !declared {
                eprintln!(
                    "tamias: warning — '{}' does not declare verb '{verb}' in capabilities (driver decides)",
                    driver.display()
                );
            }
        }
    }
    match tamias_driver::dispatch_with_args(
        kind,
        verb,
        &entry.dir,
        &entry.record.name,
        config,
        args,
    )? {
        Some((stdout, stderr)) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
        }
        None => {
            if kind != "files" {
                eprintln!("tamias: warning — no driver for type '{kind}'");
            }
            println!("{}: no driver for type {kind} — nothing ran", entry.label());
        }
    }
    Ok(())
}

/// `drivers` — every discovered `tamias-driver-<type>` (PATH first, then
/// `<tamias-home>/drivers/`, deduped by name — the same scan dispatch
/// uses) with its `capabilities` probe. A driver that won't answer
/// reports `capabilities: null` instead of failing the listing.
fn drivers_list(json: bool) -> Result<(), Error> {
    let installed = tamias_driver::drivers_dir();
    let mut rows = Vec::new();
    for (kind, path) in tamias_driver::discovered_drivers() {
        let caps = tamias_driver::capabilities(&path).ok();
        let strings = |key: &str| -> Vec<String> {
            caps.as_ref()
                .and_then(|c| c.get(key))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let types = strings("types");
        let types = if types.is_empty() {
            vec![kind.clone()]
        } else {
            types
        };
        let verbs = strings("verbs");
        let location = if path.starts_with(&installed) {
            "installed"
        } else {
            "path"
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        rows.push(serde_json::json!({
            "name": name,
            "type": kind,
            "types": types,
            "verbs": verbs,
            "location": location,
            "path": path.display().to_string(),
            "capabilities": caps,
        }));
    }
    if json {
        return print_json(&serde_json::json!({ "drivers": rows }));
    }
    if rows.is_empty() {
        println!("no drivers discovered");
        return Ok(());
    }
    let join = |row: &serde_json::Value, key: &str| -> String {
        row[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".to_string())
    };
    for row in &rows {
        let caps = if row["capabilities"].is_null() {
            "unreachable".to_string()
        } else {
            row["capabilities"].to_string()
        };
        println!(
            "{}  types: {}  verbs: {}  [{}] {}\n  capabilities: {caps}",
            row["name"].as_str().unwrap_or("?"),
            join(row, "types"),
            join(row, "verbs"),
            row["location"].as_str().unwrap_or("?"),
            row["path"].as_str().unwrap_or("?"),
        );
    }
    Ok(())
}

/// Best-effort `systemctl --user is-enabled <unit>` — bounded at 3s;
/// `None` when systemd can't answer (no user bus, timeout). `is-enabled`
/// exits non-zero for `disabled`/`masked`/`not-found`, so the state is
/// read from stdout (stderr as fallback), not the exit status.
fn unit_state(unit: &str) -> Option<String> {
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(["--user", "is-enabled", unit]);
    let out = tamias_core::output_with_timeout(&mut cmd, Duration::from_secs(3)).ok()?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if state.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return (!err.is_empty()).then_some(err);
    }
    Some(state)
}

/// The provider display/identity base — `provider.name`, else the
/// scope-qualified package name (shared by [`provider_unit_name`] and
/// `provider list`).
fn provider_base(manifest: &Manifest) -> String {
    manifest
        .provider
        .as_ref()
        .and_then(|p| p.name.clone())
        .unwrap_or_else(|| match &manifest.scope {
            Some(scope) => format!("{scope}-{}", manifest.name),
            None => manifest.name.clone(),
        })
}

/// `provider list` — installed packages' `provider:` blocks. Default:
/// only packages that declare one; `--all` adds the rest as
/// provider-less rows.
fn provider_list(env: &Env, all: bool, json: bool) -> Result<(), Error> {
    let mut rows = Vec::new();
    let mut texts = Vec::new();
    for entry in Store::new(&env.store_root).entries()? {
        let manifest = entry.manifest().ok();
        let Some(manifest) = manifest.filter(|m| m.provider.is_some()) else {
            if all {
                rows.push(serde_json::json!({
                    "package": entry.label(),
                    "provider": null,
                }));
                texts.push(format!("{}  (no provider block)", entry.label()));
            }
            continue;
        };
        // `filter` above guarantees `provider` is `Some` here.
        let Some(provider) = manifest.provider.as_ref() else {
            continue;
        };
        let base = provider_base(&manifest);
        let mut service_rows = Vec::new();
        let mut service_lines = Vec::new();
        for service in &provider.services {
            let unit = provider_unit_name(&manifest, &service.name);
            let state = unit_state(&unit);
            service_rows.push(serde_json::json!({
                "name": service.name,
                "unit": format!("{unit}.service"),
                "exec": service.exec,
                "state": state,
            }));
            service_lines.push(format!(
                "  service {} → {}.service ({})\n    exec: {}",
                service.name,
                unit,
                state.as_deref().unwrap_or("-"),
                service.exec
            ));
        }
        let ingress: Vec<serde_json::Value> = provider
            .ingress
            .iter()
            .map(|i| {
                serde_json::json!({
                    "public_url": i.public_url,
                    "listen": i.listen,
                    "path": i.path,
                })
            })
            .collect();
        let health = provider.health.as_ref().map(|h| h.cmd.clone());
        rows.push(serde_json::json!({
            "package": entry.label(),
            "provider": base,
            "services": service_rows,
            "ingress": ingress,
            "verbs": provider.verbs,
            "health": health,
            "dir": entry.dir.display().to_string(),
        }));
        let mut block = format!("{} — provider {}", entry.label(), base);
        for line in service_lines {
            block.push('\n');
            block.push_str(&line);
        }
        for i in &provider.ingress {
            let mut line = format!("  ingress {}", i.public_url);
            if let Some(listen) = &i.listen {
                line.push_str(&format!(" (listen {listen}"));
                if let Some(path) = &i.path {
                    line.push_str(path);
                }
                line.push(')');
            } else if let Some(path) = &i.path {
                line.push_str(&format!(" (path {path})"));
            }
            block.push('\n');
            block.push_str(&line);
        }
        if !provider.verbs.is_empty() {
            block.push_str(&format!("\n  verbs: {}", provider.verbs.join(", ")));
        }
        if let Some(cmd) = &health {
            block.push_str(&format!("\n  health: {cmd}"));
        }
        texts.push(block);
    }
    if json {
        return print_json(&serde_json::json!({ "providers": rows }));
    }
    if texts.is_empty() {
        println!("no provider packages installed");
    } else {
        println!("{}", texts.join("\n"));
    }
    Ok(())
}

/// `run <pkg> [service] [-- args…]` — exec a provider service's `exec`
/// argv in the foreground via `tamias-shell`: the unit's env
/// (`env_file` parsed as `K=V` lines, inline `env` overriding — the same
/// order systemd applies), `workdir` (`~` expanded) or the payload dir
/// as cwd, and a scrubbed child environment (parent credentials never
/// leak). This is the no-systemd counterpart of the rendered unit —
/// `provider.health.cmd` and richer verbs are T8-5's job.
fn run_provider_service(
    env: &Env,
    name: &str,
    service: Option<&str>,
    extra: &[String],
) -> Result<(), Error> {
    let entry = Store::new(&env.store_root).find(name)?;
    let manifest = provider_of(&entry)?.ok_or_else(|| {
        Error::Failed(format!(
            "{}: no provider: block — `tamias run` needs provider.services",
            entry.label()
        ))
    })?;
    let provider = manifest
        .provider
        .as_ref()
        .ok_or_else(|| Error::Failed(format!("{}: no provider: block", entry.label())))?;
    let names = || {
        provider
            .services
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let svc = match service {
        Some(s) => provider
            .services
            .iter()
            .find(|x| x.name == s)
            .ok_or_else(|| {
                Error::Failed(format!(
                    "{}: no provider service '{s}' (have: {})",
                    entry.label(),
                    names()
                ))
            })?,
        None => match provider.services.as_slice() {
            [] => {
                return Err(Error::Failed(format!(
                    "{}: provider declares no services",
                    entry.label()
                )));
            }
            [one] => one,
            many => {
                return Err(Error::Failed(format!(
                    "{}: provider declares {} services — pass one: {}",
                    entry.label(),
                    many.len(),
                    names()
                )));
            }
        },
    };
    let mut argv = tamias_deploy_local::split_exec(&svc.exec)?;
    argv.extend(extra.iter().cloned());
    // env_file first, inline env overrides — systemd's order.
    let mut vars: Vec<(String, String)> = Vec::new();
    if let Some(env_file) = &svc.env_file {
        let path = PathBuf::from(tamias_deploy_local::expand_home(env_file));
        // The unit renders `EnvironmentFile=-` (optional) — a missing
        // file is not an error here either.
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(Error::io(format!("read {}", path.display())))?;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                match line.split_once('=') {
                    Some((k, v)) => vars.push((k.trim().to_string(), unquote_env(v.trim()))),
                    None => eprintln!(
                        "tamias: warning — ignoring malformed env_file line in {}",
                        path.display()
                    ),
                }
            }
        }
    }
    vars.extend(svc.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let workdir = svc
        .workdir
        .as_deref()
        .map(tamias_deploy_local::expand_home)
        .map(PathBuf::from)
        .unwrap_or_else(|| entry.dir.clone());
    let status = tamias_shell::exec_with_env(&workdir, &argv, &vars)
        .map_err(Error::io("exec provider service"))?;
    if !status.success() {
        return Err(Error::Failed(format!("{} exited {status}", svc.name)));
    }
    Ok(())
}

/// Strip one layer of matching surrounding quotes from an env_file
/// value — systemd unquotes `KEY="a b"`/`KEY='a b'` the same way.
fn unquote_env(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

/// `logs <pkg|unit>` — journalctl over a provider package's units, or a
/// bare unit base name (`tamias service logs` with `--lines`/`--follow`).
fn logs(env: &Env, name: &str, lines: u32, follow: bool) -> Result<(), Error> {
    let deploy = tamias_deploy_local::LocalDeploy::new();
    match Store::new(&env.store_root).find(name) {
        Ok(entry) => {
            let Some(manifest) = provider_of(&entry)? else {
                return Err(Error::Failed(format!(
                    "{}: no provider: block — `tamias logs` needs provider.services (or pass a unit name)",
                    entry.label()
                )));
            };
            let units: Vec<String> = manifest
                .provider
                .as_ref()
                .map(|p| {
                    p.services
                        .iter()
                        .map(|s| provider_unit_name(&manifest, &s.name))
                        .collect()
                })
                .unwrap_or_default();
            if units.is_empty() {
                return Err(Error::Failed(format!(
                    "{}: provider declares no services",
                    entry.label()
                )));
            }
            deploy.logs_with(&units, lines, follow)
        }
        // Not a store entry → treat the name as a unit base name.
        Err(_) => deploy.logs_with(&[name.to_string()], lines, follow),
    }
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
    /// Serializes tests that prepend fake binaries to `PATH`.
    static PATHL: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn driver_exec_dispatches_verb_and_args() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        let dd = tmp.path().join("th/drivers");
        std::fs::create_dir_all(&dd).unwrap();
        let driver = dd.join("tamias-driver-opsx");
        std::fs::write(
            &driver,
            "#!/bin/sh\nif [ \"$1\" = capabilities ]; then\n  echo '{\"types\":[\"opsx\"],\"verbs\":[\"health\",\"hooks\"]}'\n  exit 0\nfi\necho \"$@\" >> \"$TAMIAS_HOME/seen\"\n",
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
            "name: op\nversion: 1.0.0\ntype: opsx\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        // trailing args land after the standard flags, verbatim
        run_args(
            e.clone(),
            &[
                "tamias",
                "driver",
                "exec",
                "op",
                "hooks",
                "register",
                "--dry-run",
            ],
        )
        .unwrap();
        let seen = std::fs::read_to_string(tmp.path().join("th/seen")).unwrap();
        assert!(seen.contains("hooks"), "{seen}");
        assert!(seen.contains("--dir"), "{seen}");
        assert!(seen.contains("--name"), "{seen}");
        assert!(seen.contains("register"), "{seen}");
        assert!(seen.contains("--dry-run"), "{seen}");
        // unknown package → error; undeclared verb warns but still runs
        assert!(run_args(e.clone(), &["tamias", "driver", "exec", "ghost", "health"]).is_err());
        run_args(e.clone(), &["tamias", "driver", "exec", "op", "bogus"]).unwrap();
        let seen = std::fs::read_to_string(tmp.path().join("th/seen")).unwrap();
        assert!(seen.contains("bogus"), "{seen}");
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn driver_exec_parses_trailing_args() {
        let cli = Cli::try_parse_from([
            "tamias",
            "driver",
            "exec",
            "pkg",
            "hooks",
            "register",
            "--dry-run",
            "--api-base",
            "http://x",
        ])
        .unwrap();
        let Some(Command::Driver {
            cmd: DriverCommand::Exec {
                name, verb, args, ..
            },
        }) = cli.command
        else {
            panic!("expected driver exec");
        };
        assert_eq!(name, "pkg");
        assert_eq!(verb, "hooks");
        assert_eq!(args, ["register", "--dry-run", "--api-base", "http://x"]);
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
        for _ in 0..100 {
            if std::net::TcpStream::connect("127.0.0.1:8792").is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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
        let _ = (store_root(None), cache_root(None), registry_root());
        std::env::remove_var("TAMIAS_HOME");
        std::env::remove_var("TAMIAS_CACHE");
        std::env::remove_var("TAMIAS_REGISTRY");
        std::env::set_var("XDG_DATA_HOME", "/xd");
        std::env::set_var("XDG_CACHE_HOME", "/xc");
        let _ = (store_root(None), cache_root(None), registry_root());
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var("XDG_CACHE_HOME");
        std::env::set_var("HOME", "/h");
        let _ = (store_root(None), cache_root(None), registry_root());
        std::env::remove_var("HOME");
        let _ = (store_root(None), cache_root(None), registry_root());
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
    fn deploy_renders_provider_services_and_uninstall_removes() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("cfg"));
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            "name: gate\nversion: 1.0.0\nprovider:\n  services:\n    - name: web\n      exec: \"/usr/bin/gate serve --port \\\"2917 3\\\"\"\n      env_file: \"~/.config/gate/env\"\n      env: {PORT: \"29173\"}\n      workdir: \"~/srv/gate\"\n      restart: always\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        run_args(e.clone(), &["tamias", "deploy", "gate"]).unwrap();
        let unit = tmp.path().join("cfg/systemd/user/gate-web.service");
        let text = std::fs::read_to_string(&unit).unwrap();
        assert!(text.contains("ExecStart=/usr/bin/gate serve --port \"2917 3\""));
        assert!(text.contains("EnvironmentFile=-"));
        assert!(text.contains(".config/gate/env"));
        assert!(text.contains("Environment=PORT=29173"));
        assert!(text.contains("/srv/gate"));
        assert!(text.contains("Restart=always"));
        assert!(text.contains("After=network-online.target"));
        // uninstall drops the declarative unit too
        run_args(e.clone(), &["tamias", "uninstall", "gate"]).unwrap();
        assert!(!unit.exists());
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn deploy_provider_name_scopes_unit_name() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("cfg"));
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            "name: gate\nscope: ci\nversion: 1.0.0\nprovider:\n  name: gateprov\n  services:\n    - name: api\n      exec: /bin/true\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        run_args(e.clone(), &["tamias", "deploy", "gate"]).unwrap();
        // provider.name wins over the scope-qualified package name
        assert!(tmp
            .path()
            .join("cfg/systemd/user/gateprov-api.service")
            .is_file());
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn add_with_existing_manifest_and_service_verbs() {
        let _p = PATHL.lock().unwrap();
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
        let _p = PATHL.lock().unwrap();
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
    fn schema_carries_the_provider_block() {
        // `tamias schema` is `schemars::schema_for!(Manifest)` — the
        // `provider:` block must surface with its nested fields.
        let schema = serde_json::to_value(schemars::schema_for!(Manifest)).unwrap();
        let text = schema.to_string();
        for needle in [
            "\"provider\"",
            "\"services\"",
            "\"ingress\"",
            "\"hooks\"",
            "\"env_file\"",
            "\"public_url\"",
            "\"workdir\"",
            "\"restart\"",
            "\"verbs\"",
            "\"health\"",
        ] {
            assert!(text.contains(needle), "schema missing {needle}");
        }
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
    fn install_registers_platform_driver_bins() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let e = env(tmp.path());
        // a package shipping a *compiled*-style driver under
        // drivers/bin/<triple>/ (a shell script stands in for the binary —
        // registration is bytes + exec bit, platform selection is the path)
        let dir = tmp.path().join("pkgb");
        let triple = tamias_driver::target_triple();
        std::fs::create_dir_all(dir.join("drivers/bin").join(triple)).unwrap();
        std::fs::write(dir.join("tamias.yaml"), "name: b\nversion: 1.0.0\n").unwrap();
        std::fs::write(
            dir.join("drivers/bin")
                .join(triple)
                .join("tamias-driver-binfx"),
            "#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        // a foreign-triple dir rides along — must not be registered
        std::fs::create_dir_all(dir.join("drivers/bin/not-a-real-triple")).unwrap();
        std::fs::write(
            dir.join("drivers/bin/not-a-real-triple/tamias-driver-foreign"),
            "bin",
        )
        .unwrap();
        let r = dir.to_str().unwrap();
        // consent gate sees the bin payload too
        assert!(run_args(e.clone(), &["tamias", "install", r]).is_err());
        assert!(!tmp.path().join("th/drivers/tamias-driver-binfx").exists());
        run_args(e.clone(), &["tamias", "install", r, "--allow-drivers"]).unwrap();
        assert!(tmp.path().join("th/drivers/tamias-driver-binfx").is_file());
        assert!(!tmp.path().join("th/drivers/tamias-driver-foreign").exists());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn install_platform_bins_missing_platform_fails() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        let e = env(tmp.path());
        let dir = tmp.path().join("pkgc");
        std::fs::create_dir_all(dir.join("drivers/bin/wasm32-unknown-unknown")).unwrap();
        std::fs::write(dir.join("tamias.yaml"), "name: c\nversion: 1.0.0\n").unwrap();
        std::fs::write(
            dir.join("drivers/bin/wasm32-unknown-unknown/tamias-driver-w"),
            "bin",
        )
        .unwrap();
        let res = run_args(
            e.clone(),
            &[
                "tamias",
                "install",
                dir.to_str().unwrap(),
                "--allow-drivers",
            ],
        );
        let err = res.unwrap_err().to_string();
        assert!(err.contains("not this platform"), "{err}");
        assert!(err.contains("wasm32-unknown-unknown"), "{err}");
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
                "drivers",
                "driver",
                "provider",
                "run",
                "logs",
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
    fn drivers_provider_run_logs() {
        use std::os::unix::fs::PermissionsExt;
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        // a driver installed under <tamias-home>/drivers — capabilities answers
        let dd = tmp.path().join("th/drivers");
        std::fs::create_dir_all(&dd).unwrap();
        let driver = dd.join("tamias-driver-fx");
        std::fs::write(
            &driver,
            "#!/bin/sh\nif [ \"$1\" = capabilities ]; then echo '{\"types\":[\"fx\"],\"verbs\":[\"install\",\"deploy\"]}'; fi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        run_args(e.clone(), &["tamias", "drivers"]).unwrap();
        run_args(e.clone(), &["tamias", "drivers", "--json"]).unwrap();

        // a provider package + a plain one
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let env_file = tmp.path().join("svc.env");
        std::fs::write(
            &env_file,
            "# comment\nFILE_VAR=\"quoted value\"\nBADLINE\nOTHER=1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            format!(
                "name: gate\nversion: 1.0.0\nprovider:\n  name: gateprov\n  services:\n    - name: web\n      exec: \"printenv RUN_MARK\"\n      env: {{RUN_MARK: ok}}\n    - name: api\n      exec: \"/bin/echo api-argv\"\n    - name: envtest\n      exec: >-\n        sh -c \"test \\\"$FILE_VAR\\\" = 'quoted value' && test \\\"$INLINE\\\" = 'inline'\"\n      env_file: {}\n      env: {{INLINE: inline}}\n  ingress:\n    - public_url: https://ci.example.com\n      listen: 127.0.0.1:29173\n      path: /hook\n  verbs: [run, logs]\n  health: {{cmd: \"curl -sf localhost:29173/health\"}}\n",
                env_file.display()
            ),
        )
        .unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        // a plain package (no provider:) for --all / run / logs negatives
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("tamias.yaml"), "name: w\nversion: 1.0.0\n").unwrap();
        std::fs::write(plain.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", plain.to_str().unwrap()]).unwrap();

        // provider list — default (provider only), --all, --json
        run_args(e.clone(), &["tamias", "provider", "list"]).unwrap();
        run_args(e.clone(), &["tamias", "provider", "list", "--all"]).unwrap();
        run_args(e.clone(), &["tamias", "provider", "list", "--json"]).unwrap();
        run_args(
            e.clone(),
            &["tamias", "provider", "list", "--all", "--json"],
        )
        .unwrap();

        // run — sole-service pick needs the service arg here (3 services);
        // env_file parsing + quoting + inline env override exercised by envtest
        run_args(e.clone(), &["tamias", "run", "gate", "web"]).unwrap();
        run_args(e.clone(), &["tamias", "run", "gate", "api", "--", "hello"]).unwrap();
        run_args(e.clone(), &["tamias", "run", "gate", "envtest"]).unwrap();
        assert!(run_args(e.clone(), &["tamias", "run", "gate"]).is_err()); // ambiguous
        assert!(run_args(e.clone(), &["tamias", "run", "gate", "nope"]).is_err());
        assert!(run_args(e.clone(), &["tamias", "run", "w"]).is_err()); // no provider
        assert!(run_args(e.clone(), &["tamias", "run", "ghost"]).is_err());

        // logs — fake journalctl records argv; package → all provider units
        let _p = PATHL.lock().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let jlog = tmp.path().join("journal.log");
        let fake = bin.join("journalctl");
        std::fs::write(&fake, "#!/bin/sh\necho \"$@\" >> \"$JLOG_PATH\"\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut newp = bin.clone().into_os_string();
        newp.push(":");
        newp.push(&old_path);
        std::env::set_var("PATH", newp);
        std::env::set_var("JLOG_PATH", &jlog);
        let r1 = run_args(e.clone(), &["tamias", "logs", "gate", "--lines", "50"]);
        let r2 = run_args(e.clone(), &["tamias", "logs", "otherunit", "-f"]);
        let r3 = run_args(
            e.clone(),
            &["tamias", "service", "logs", "u", "--lines", "5"],
        );
        let r4 = run_args(e.clone(), &["tamias", "logs", "w"]); // no provider → err
        std::env::set_var("PATH", old_path);
        std::env::remove_var("JLOG_PATH");
        r1.unwrap();
        r2.unwrap();
        r3.unwrap();
        assert!(r4.is_err());
        let calls = std::fs::read_to_string(&jlog).unwrap();
        assert!(calls.contains("-u gateprov-web.service"), "{calls}");
        assert!(calls.contains("-u gateprov-api.service"), "{calls}");
        assert!(calls.contains("-u gateprov-envtest.service"), "{calls}");
        assert!(calls.contains("-n 50"), "{calls}");
        assert!(calls.contains("-u otherunit.service"), "{calls}");
        assert!(calls.contains("-f"), "{calls}");
        assert!(calls.contains("-u u.service -n 5"), "{calls}");

        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn config_locations_resolve_store_and_cache() {
        // tamias-config is wired for store/cache roots: a project
        // `tamias.yml` `store.root`/`cache.root` become the Env roots
        // when the env vars are unset; `kind: cf` is reserved → ignored.
        // Lock order matters: DRVENV before CWD — sibling tests take
        // DRVENV→PATHL→CWD, so acquiring out of order deadlocks.
        let _g2 = DRVENV.lock().unwrap();
        let _g = CWD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("tamias.yml"),
            "store:\n  root: /cfg-store\ncache:\n  kind: local\n  root: /cfg-cache\n",
        )
        .unwrap();
        let prev = std::env::current_dir().unwrap();
        let keys = [
            "TAMIAS_HOME",
            "TAMIAS_CACHE",
            "TAMIAS_REGISTRY",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "HOME",
        ];
        let saved: Vec<(String, Option<std::ffi::OsString>)> = keys
            .iter()
            .map(|k| (k.to_string(), std::env::var_os(k)))
            .collect();
        for k in &keys {
            std::env::remove_var(k);
        }
        std::env::set_current_dir(&proj).unwrap();
        let e = Env::from_env();
        std::env::set_current_dir(&prev).unwrap();
        for (k, v) in saved {
            if let Some(v) = v {
                std::env::set_var(k, v);
            }
        }
        assert_eq!(e.store_root, PathBuf::from("/cfg-store"));
        assert_eq!(e.cache_root, PathBuf::from("/cfg-cache"));
        // env vars still win over the config file
        std::env::set_var("TAMIAS_HOME", "/env-home");
        let cfg = tamias_config::Config {
            store: tamias_config::Location {
                kind: None,
                root: Some("/cfg-store".into()),
            },
            ..Default::default()
        };
        assert_eq!(
            store_root(Some(&cfg)),
            PathBuf::from("/env-home").join("store")
        );
        std::env::remove_var("TAMIAS_HOME");
        // a reserved kind warns + falls through to the default chain —
        // with every root env var cleared that's `./.tamias/store`.
        let cf = tamias_config::Config {
            store: tamias_config::Location {
                kind: Some("cf".into()),
                root: Some("https://x".into()),
            },
            ..Default::default()
        };
        let saved2: Vec<(String, Option<std::ffi::OsString>)> = keys
            .iter()
            .map(|k| (k.to_string(), std::env::var_os(k)))
            .collect();
        for k in &keys {
            std::env::remove_var(k);
        }
        let root = store_root(Some(&cf));
        for (k, v) in saved2 {
            if let Some(v) = v {
                std::env::set_var(k, v);
            }
        }
        assert_eq!(root, PathBuf::from(".tamias/store"));
    }

    #[test]
    fn unquote_env_strips_matching_quotes() {
        assert_eq!(unquote_env("\"a b\""), "a b");
        assert_eq!(unquote_env("'a b'"), "a b");
        assert_eq!(unquote_env("plain"), "plain");
        assert_eq!(unquote_env("\"unclosed"), "\"unclosed");
        assert_eq!(unquote_env("\"\""), "");
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
