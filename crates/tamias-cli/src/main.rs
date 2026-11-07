//! tamias — the CLI. P2b: the distribution verbs over the source port, the
//! content-addressed cache, and the local store.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tamias_cache_local::FetchCache;
use tamias_core::{
    parse_ref, payload_root, Error, Lock, LockEntry, Manifest, StoreEntry, StoreRecord,
};
use tamias_registry::{parse_registry_ref, Registry, RegistryRef};
use tamias_registry_cf::HttpRegistry;
use tamias_registry_local::{serve_http, LocalRegistry};
use tamias_source::{Fetched, Resolved, Source};
use tamias_source_fs::FsSource;
use tamias_source_git::GitSource;
use tamias_source_http::HttpSource;
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
    /// Same as `install` (manifest write lands later).
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
    List,
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
    /// Fetch-cache maintenance.
    Cache {
        #[command(subcommand)]
        cmd: CacheCommand,
    },
    /// Print the tamias version.
    Version,
    /// Print the machine-readable capability manifest.
    Manifest,
    /// Diagnose the host metaservice (store, cache, registry, drivers).
    Doctor {
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

/// Dispatch the parsed ref to its source impl.
fn resolve(reference: &tamias_core::SourceRef) -> Result<Resolved, Error> {
    match reference {
        tamias_core::SourceRef::Fs(_) => FsSource::new().resolve(reference),
        tamias_core::SourceRef::Git(_) => GitSource::new().resolve(reference),
        tamias_core::SourceRef::Http(_) => HttpSource::new().resolve(reference),
    }
}

fn fetch(resolved: &Resolved, dest: &std::path::Path) -> Result<Fetched, Error> {
    match resolved {
        Resolved::Fs { .. } => FsSource::new().fetch(resolved, dest),
        Resolved::Git { .. } => GitSource::new().fetch(resolved, dest),
        Resolved::Http { .. } => HttpSource::new().fetch(resolved, dest),
    }
}

/// resolve → cache-hit or fetch+commit → `(payload_dir, resolved_id)`.
fn fetch_or_cached(env: &Env, resolved: &Resolved) -> Result<(PathBuf, String), Error> {
    let cache = FetchCache::new(&env.cache_root);
    if let Some((key, resolved_id)) = resolved.prefetch_identity() {
        if cache.has(&key) {
            return Ok((cache.entry(&key), resolved_id));
        }
    }
    let scratch = cache.scratch()?;
    let fetched = match fetch(resolved, &scratch) {
        Ok(fetched) => fetched,
        Err(e) => {
            cache.discard(&scratch);
            return Err(e);
        }
    };
    let committed = cache.commit(&scratch, &fetched.content_key)?;
    Ok((committed, fetched.resolved))
}

fn install(env: &Env, reference: &str) -> Result<StoreEntry, Error> {
    let parsed = parse_ref(reference).map_err(|e| Error::Failed(e.to_string()))?;
    let resolved = resolve(&parsed)?;
    let (payload_dir, resolved_id) = fetch_or_cached(env, &resolved)?;
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
        Some(Command::List) => {
            let entries = Store::new(&env.store_root).entries()?;
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
        Some(Command::Manifest) => {
            println!(
                "{{\"name\":\"tamias\",\"version\":\"{}\",\"verbs\":[\"install\",\"add\",\"remove\",\"list\",\"info\",\"verify\",\"cache\"]}}",
                env!("CARGO_PKG_VERSION")
            );
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
    match run(Cli::parse(), Env::from_env()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tamias: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

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
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
        std::env::set_current_dir(prev).unwrap();
        result.unwrap();
        assert!(tmp.path().join("tamias.yaml").is_file());
        assert!(tmp.path().join("tamias.lock").is_file());
    }
}
