//! tamias — the CLI. P2b: the distribution verbs over the source port, the
//! content-addressed cache, and the local store.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tamias_cache_local::FetchCache;
use tamias_content::{self, InstallCtx};
use tamias_core::{parse_ref, payload_root, Error, Manifest, StoreEntry, StoreRecord};
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

/// Store + cache roots — `TAMIAS_HOME`/`TAMIAS_CACHE`/XDG, else the
/// `~/.local/share` + `~/.cache` defaults.
struct Env {
    store_root: PathBuf,
    cache_root: PathBuf,
}

impl Env {
    fn from_env() -> Self {
        Self {
            store_root: store_root(),
            cache_root: cache_root(),
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
            if let Some(content) = tamias_content::by_name(kind) {
                let report = content.install(&InstallCtx {
                    payload_root: &root,
                    reference,
                    entry: &entry,
                })?;
                for action in report.actions {
                    println!("  {action}");
                }
            }
            Ok(entry)
        })
}

fn short(digest: &str) -> &str {
    let s = digest.strip_prefix("sha256:").unwrap_or(digest);
    s.get(..12).unwrap_or(s)
}

fn run(cli: Cli) -> Result<(), Error> {
    let env = Env::from_env();
    match cli.command {
        Some(Command::Install { reference }) | Some(Command::Add { reference }) => {
            let entry = install(&env, &reference)?;
            println!(
                "installed {} ({})",
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
        Some(Command::Version) | None => {
            println!("tamias {}", env!("CARGO_PKG_VERSION"));
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

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tamias: {e}");
            ExitCode::FAILURE
        }
    }
}
