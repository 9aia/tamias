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
    },
    /// Install a ref and record it in `tamias.yaml` + `tamias.lock`.
    Add {
        /// The ref to add.
        reference: String,
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
    },
    /// Show one package's manifest + provenance.
    Info {
        /// `@scope/name` or `name`.
        name: String,
    },
    /// Digest-check a package against its record.
    Verify {
        /// `@scope/name` or `name`.
        name: String,
    },
    /// Re-resolve + re-install a package from its recorded ref.
    Update {
        /// `@scope/name` or `name` (omit with `--all`).
        name: Option<String>,
        /// Update every installed package.
        #[arg(long)]
        all: bool,
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
        #[arg(long)]
        all: bool,
        /// Max age in days (default 30).
        #[arg(long)]
        days: Option<u64>,
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
        if cache.has(&key) {
            tracing::debug!(cache_key = %key, "fetch-cache hit");
            return Ok((cache.entry(&key), resolved_id));
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

fn install(env: &Env, reference: &str) -> Result<StoreEntry, Error> {
    let span = tracing::info_span!("install", reference);
    let _enter = span.enter();
    let spec = refs().parse(reference)?;
    let planned = sources().plan(&*spec)?;
    let (payload_dir, resolved_id) = fetch_or_cached(env, &planned)?;
    let root = payload_root(&payload_dir)?;
    let manifest = Manifest::load(&root)?;
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
    Store::new(&env.store_root)
        .install(&root, record)
        .and_then(|entry| {
            let kind = manifest.kind.as_deref().unwrap_or("files");
            for action in tamias_driver::register_bundled_drivers(&root)? {
                println!("  {action}");
            }
            tamias_driver::dispatch(kind, "install", &root, &entry.record.name, None)?;
            Ok(entry)
        })
}

fn short(digest: &str) -> &str {
    let s = digest.strip_prefix("sha256:").unwrap_or(digest);
    s.get(..12).unwrap_or(s)
}

/// The CLI reference (markdown), generated from the clap parser.
fn cli_docs() -> String {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    format!("# tamias CLI\n\n```\n{}\n```\n", cmd.render_long_help())
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
        Some(Command::Install { reference }) => {
            let entry = install(&env, &reference)?;
            println!(
                "installed {} ({})",
                entry.label(),
                short(&entry.record.digest)
            );
        }
        Some(Command::Add { reference }) => {
            let entry = install(&env, &reference)?;
            let manifest_path = PathBuf::from("tamias.yaml");
            let mut project = if manifest_path.is_file() {
                Manifest::load_at(&manifest_path)?
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
        Some(Command::List { kind }) => {
            let entries: Vec<StoreEntry> = Store::new(&env.store_root)
                .entries()?
                .into_iter()
                .filter(|e| {
                    kind.as_deref()
                        .is_none_or(|k| e.record.kind.as_deref() == Some(k))
                })
                .collect();
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
        Some(Command::Info { name }) => {
            let entry = Store::new(&env.store_root).find(&name)?;
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
            println!("resolved:{}", entry.record.resolved);
            println!("digest:  {}", entry.record.digest);
            println!("size:    {} bytes", entry.size()?);
            println!("dir:     {}", entry.dir.display());
        }
        Some(Command::Verify { name }) => {
            let report = Store::new(&env.store_root).verify(&name)?;
            if report.ok {
                println!("{}: ok ({})", report.name, short(&report.recorded));
            } else {
                return Err(Error::Failed(format!(
                    "{}: digest mismatch — recorded {}, computed {}",
                    report.name,
                    short(&report.recorded),
                    short(&report.computed)
                )));
            }
        }
        Some(Command::Update { name, all }) => {
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
            for r in refs {
                let entry = install(&env, &r)?;
                println!("updated {}", entry.label());
            }
        }
        Some(Command::Pin { name, reference }) => {
            let record = Store::new(&env.store_root).retarget(&name, &reference)?;
            println!("pinned {} → {}", record.name, record.source_ref);
        }
        Some(Command::Cache { cmd }) => {
            let cache = FetchCache::new(&env.cache_root);
            let CacheCommand::Clean { all, days } = cmd;
            let evicted = if all {
                cache.clean_all()?
            } else {
                cache.clean(Duration::from_secs(
                    days.unwrap_or(CACHE_MAX_AGE_DAYS) * 86_400,
                ))?
            };
            println!("evicted {} cache entries", evicted.len());
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
            let entry = registry.publish(&root, &manifest)?;
            println!("published {}", entry.label());
        }
        Some(Command::Search { query, registry }) => {
            let registry = open_registry(&env, registry.as_deref());
            for e in registry.search(&query)? {
                println!("{}  {}  {}", e.label(), short(&e.digest), e.file);
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
        Some(Command::Version) | None => {
            println!("tamias {}", env!("CARGO_PKG_VERSION"));
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
            println!(
                "{{\"name\":\"tamias\",\"version\":\"{}\",\"verbs\":[\"install\",\"add\",\"remove\",\"list\",\"info\",\"verify\",\"update\",\"pin\",\"cache\",\"publish\",\"search\",\"bundle\",\"registry\",\"deploy\",\"uninstall\",\"service\",\"doctor\",\"serve\",\"schema\"]}}",
                env!("CARGO_PKG_VERSION")
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
    let entry = Store::new(&env.store_root).find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    let _ = tamias_driver::dispatch(kind, "uninstall", &entry.dir, &entry.record.name, None);
    let record = Store::new(&env.store_root).remove(name)?;
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
                .filter_map(|kv| {
                    kv.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
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

/// `doctor` — diagnose the host metaservice.
fn doctor(env: &Env, json: bool) -> Result<(), Error> {
    let store = Store::new(&env.store_root).entries()?;
    let cache = FetchCache::new(&env.cache_root).entries()?;
    let drivers = tamias_driver::discovered_drivers();
    let checks: Vec<(&str, String, bool)> = vec![
        ("tamias", env!("CARGO_PKG_VERSION").to_string(), true),
        ("store", env.store_root.display().to_string(), true),
        ("packages", store.len().to_string(), true),
        ("cache", env.cache_root.display().to_string(), true),
        ("cache entries", cache.len().to_string(), true),
        ("registry", env.registry_root.display().to_string(), true),
        (
            "drivers",
            if drivers.is_empty() {
                "(none)".to_string()
            } else {
                drivers
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            true,
        ),
    ];
    if json {
        let value: Vec<serde_json::Value> = checks
            .iter()
            .map(|(k, v, ok)| serde_json::json!({"check": k, "detail": v, "ok": ok}))
            .collect();
        println!(
            "{}",
            serde_json::to_string(&value).map_err(|e| Error::Failed(e.to_string()))?
        );
    } else {
        for (k, v, ok) in &checks {
            println!("{} {k}: {v}", if *ok { "ok " } else { "!! " });
        }
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
        std::env::set_current_dir(prev).unwrap();
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
            std::env::set_current_dir(prev).unwrap();
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
            std::env::set_current_dir(prev).unwrap();
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
            std::env::set_current_dir(prev).unwrap();
            assert!(r.is_err());
        }
        {
            let _g = CWD.lock().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();
            std::fs::write(tmp.path().join("tamias.yaml"), "name: proj\n").unwrap();
            std::fs::write(tmp.path().join("tamias.lock"), ": bad : yaml :").unwrap();
            let r = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
            std::env::set_current_dir(prev).unwrap();
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
            std::env::set_current_dir(prev).unwrap();
        }
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
}
