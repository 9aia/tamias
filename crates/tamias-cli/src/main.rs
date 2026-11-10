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
use tamias_config::{ConfigFormat, CONFIG_ENV, CONFIG_NAMES};
use tamias_config_toml::TomlFormat;
use tamias_config_yml::YmlFormat;
use tamias_core::{short_digest, Error, Lock, Manifest, StoreEntry, StoreRecord};
use tamias_install::{Progress, Surface};
use tamias_registry::Registry;
use tamias_registry_local::{serve_http, serve_http_with_limits};
use tamias_store_local::Store;

/// Default fetch-cache eviction age for `cache clean` — 30 days, the
/// shared [`tamias_cache_local::DEFAULT_MAX_AGE`] window in days.
const CACHE_MAX_AGE_DAYS: u64 = tamias_cache_local::DEFAULT_MAX_AGE.as_secs() / 86_400;

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
        /// Sign the package (`ssh-keygen -Y`, namespace `tamias-package`) —
        /// the signature rides the registry index to `cf:` installs.
        #[arg(long)]
        sign: bool,
        /// Signing key path — overrides `$TAMIAS_SIGNING_KEY`,
        /// `signing.key_env`, `signing.key`, `~/.ssh/id_ed25519`.
        #[arg(long, requires = "sign")]
        key: Option<PathBuf>,
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
    /// Registry operations — named sources in the config file.
    Registry {
        /// Explicit config file for the management verbs (overrides
        /// `TAMIAS_CONFIG` + discovery).
        #[arg(long)]
        file: Option<PathBuf>,
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
    /// Interactive package-manager TUI (ratatui) — installed entries,
    /// registry search, provider lifecycle, manifest deps, authoring.
    #[cfg(feature = "tui")]
    Ui,
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
    /// Fetch the `--config` file's JSON Schema from the package's kind
    /// driver — the reserved `config-schema` verb, printed to stdout.
    /// Drivers without one exit non-zero; tamias surfaces their stderr
    /// ("unsupported" is expected for drivers that don't declare it).
    /// Validate a config driver-side: `tamias driver exec <pkg> config
    /// validate --config <file>` — tamias ships no schema validator.
    ConfigSchema {
        /// `@scope/name` or `name` — the installed package.
        name: String,
        /// Node/config file passed through as the driver's `--config`.
        #[arg(long)]
        config: Option<PathBuf>,
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
        /// Upload cap — bytes or a human size (`256MiB`, `1GiB`).
        /// Overrides the config's global `limits.max_upload`; the
        /// built-in 512 MiB default applies when neither is set.
        #[arg(long)]
        max_upload: Option<String>,
    },
    /// Add a named registry to the config file.
    Add {
        /// Registry name — `publish`/`search --registry <name>` and the
        /// `publish.default`/`search.default` routing resolve it.
        name: String,
        /// Registry kind.
        #[arg(long, value_enum)]
        kind: RegistryKind,
        /// Base URL (http/cf) or root dir (local).
        #[arg(long)]
        url: String,
        /// Env var NAME holding the bearer token (http/cf) — the value
        /// is read from the environment at connect time, never stored.
        #[arg(long)]
        token_env: Option<String>,
    },
    /// Remove a named registry from the config file.
    Remove {
        /// Registry name.
        name: String,
    },
    /// Show one named registry.
    Get {
        /// Registry name.
        name: String,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// List named registries.
    List {
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
}

/// `registry add --kind` — the explicit kind set. `http`/`cf` share
/// the `HttpRegistry` client; `cf` marks a worker-managed registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum RegistryKind {
    /// A local registry dir (`LocalRegistry`, optionally `registry serve`d).
    Local,
    /// Any http(s) blob-store endpoint.
    Http,
    /// A Cloudflare-Worker-managed registry (R2 + bearer token).
    Cf,
}

impl RegistryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Http => "http",
            Self::Cf => "cf",
        }
    }
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
/// `config` verb uses, with `TAMIAS_CONFIG` naming an explicit file
/// (overrides discovery, mirroring `--file`). Parse/read failures warn
/// and yield `None`.
fn cli_config() -> Option<tamias_config::Config> {
    // Resolution + the file precedence live in `tamias-registry-open`
    // (shared with `tamias serve`); the CLI keeps its warn-and-ignore
    // posture on top — a broken config warns and is ignored rather
    // than failing every verb before it starts.
    match tamias_registry_open::load_config(None) {
        Ok(config) => Some(config),
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
        &tamias_install::Env::for_config(
            env.store_root.clone(),
            env.cache_root.clone(),
            cli_config().as_ref(),
        ),
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
        "signed": entry.record.signature.is_some(),
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

/// Keep `tamias.lock` honest after `update`/`pin` — the shared
/// [`Lock::sync_record`] rule (cwd `tamias.lock`, refresh pinned
/// entries only, never create).
fn sync_lock(record: &StoreRecord) -> Result<(), Error> {
    Lock::sync_record(Path::new("tamias.lock"), record)
}

/// Open a registry by name-or-ref — the shared resolution lives in
/// `tamias-registry-open` (same code `tamias serve`'s `search`/`publish`
/// ops use); this wrapper binds the CLI env's `registry_root`.
fn open_registry(
    env: &Env,
    config: Option<&tamias_config::Config>,
    value: Option<&str>,
    package: Option<&str>,
) -> Result<Box<dyn Registry>, Error> {
    tamias_registry_open::open_registry(&env.registry_root, config, value, package)
}

/// `registry add` — validate + write through the shared
/// `tamias-registry-open` path (same code `tamias serve`'s
/// `registry.add` op uses), then report.
fn registry_add(
    file: Option<&Path>,
    name: String,
    kind: RegistryKind,
    url: String,
    token_env: Option<String>,
) -> Result<(), Error> {
    let path = tamias_registry_open::add_registry(file, &name, kind.as_str(), &url, token_env)?;
    println!(
        "added registry {name} ({}) → {}",
        kind.as_str(),
        path.display()
    );
    Ok(())
}

/// `registry remove` — shared write path; unknown names error.
fn registry_remove(file: Option<&Path>, name: &str) -> Result<(), Error> {
    let path = tamias_registry_open::remove_registry(file, name)?;
    println!("removed registry {name} → {}", path.display());
    Ok(())
}

/// `registry get` — one named registry, text or `--json`.
fn registry_get(file: Option<&Path>, name: &str, json: bool) -> Result<(), Error> {
    let reg = tamias_registry_open::get_registry(file, name)?;
    if json {
        print_json(&tamias_registry_open::registry_json(name, &reg))?;
    } else {
        println!("name:      {name}");
        println!("kind:      {}", reg.kind.as_deref().unwrap_or("(inferred)"));
        println!("effective: {}", reg.effective_kind());
        println!("url:       {}", reg.url.as_deref().unwrap_or("-"));
        println!("token_env: {}", reg.token_env.as_deref().unwrap_or("-"));
        if !reg.limits.is_empty() {
            println!(
                "limits:    {}",
                YmlFormat
                    .render(&tamias_config::Config {
                        limits: reg.limits.clone(),
                        ..Default::default()
                    })?
                    .trim()
            );
        }
    }
    Ok(())
}

/// `registry list` — every named registry, text table or `--json`.
fn registry_list(file: Option<&Path>, json: bool) -> Result<(), Error> {
    let (_path, config) = tamias_registry_open::config_target(file)?;
    if json {
        let items: Vec<serde_json::Value> = config
            .registries
            .iter()
            .map(|(name, reg)| tamias_registry_open::registry_json(name, reg))
            .collect();
        print_json(&serde_json::json!({ "registries": items }))?;
    } else if config.registries.is_empty() {
        println!("no registries configured");
    } else {
        for (name, reg) in &config.registries {
            println!(
                "{name}  {}  {}  {}",
                reg.effective_kind(),
                reg.url.as_deref().unwrap_or("-"),
                reg.token_env
                    .as_deref()
                    .map(|t| format!("token: ${t}"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// Human byte size for doctor output — `512MiB`, `1.5GiB`, raw `B`.
fn fmt_bytes(n: u64) -> String {
    for (unit, div) in [
        ("TiB", 1u64 << 40),
        ("GiB", 1u64 << 30),
        ("MiB", 1u64 << 20),
        ("KiB", 1u64 << 10),
    ] {
        if n >= div && n.is_multiple_of(div) {
            return format!("{}{unit}", n / div);
        }
    }
    format!("{n}B")
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
                short_digest(&entry.record.digest)
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
            Lock::upsert_record(Path::new("tamias.lock"), &entry.record)?;
            println!(
                "added {} → tamias.yaml + tamias.lock ({})",
                entry.label(),
                short_digest(&entry.record.digest)
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
                        short_digest(&e.record.digest),
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
            let store = Store::new(&env.store_root);
            let report = store.verify(&name)?;
            // Signature status alongside the digest check — the record's
            // `signature` re-verified against `signing.allowed_signers`
            // (env `TAMIAS_ALLOWED_SIGNERS` → config →
            // ~/.config/tamias/allowed_signers).
            let entry = store.find(&name)?;
            let policy = tamias_sign::SigningPolicy::resolve(cli_config().as_ref());
            let signature = tamias_sign::record_signature_status(
                &entry.record,
                policy.allowed_signers.as_deref(),
                &policy.namespace,
            )?;
            if json {
                print_json(&serde_json::json!({
                    "name": report.name,
                    "recorded": report.recorded,
                    "computed": report.computed,
                    "ok": report.ok,
                    "signature": signature.as_str(),
                }))?;
            }
            if report.ok {
                if !json {
                    println!("{}: ok ({})", report.name, short_digest(&report.recorded));
                    println!("signature: {}", signature.as_str());
                }
            } else {
                return Err(Error::Failed(format!(
                    "{}: digest mismatch — recorded {}, computed {}",
                    report.name,
                    short_digest(&report.recorded),
                    short_digest(&report.computed)
                )));
            }
            // A bad signature is a provenance failure, not a footnote.
            match signature {
                tamias_sign::SignatureStatus::Failed => {
                    return Err(Error::Failed(format!(
                        "{}: signature verification failed",
                        report.name
                    )));
                }
                tamias_sign::SignatureStatus::Unchecked => {
                    eprintln!(
                        "tamias: warning — {} is signed but no allowed_signers is configured",
                        report.name
                    );
                }
                _ => {}
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
                registry_root: env.registry_root.clone(),
                // `None` → the serve ops honor `TAMIAS_CONFIG` then
                // `./tamias.{yml,yaml,toml}` discovery per request, so
                // `registry.add` is visible to a later `search` in the
                // same session.
                config_file: None,
            })?;
        }
        Some(Command::Publish {
            dir,
            registry,
            sign,
            key,
        }) => {
            let config = cli_config();
            // The shared publish resolution (`tamias-registry-open`) —
            // same code `tamias serve`'s `publish` op and `tamias ui`
            // take. The "publishing" line prints post-open, pre-upload.
            let plan = tamias_registry_open::publish_plan(
                &dir,
                &env.registry_root,
                config.as_ref(),
                registry.as_deref(),
                &[config.as_ref().and_then(|c| c.publish.default.as_deref())],
                &mut |_, _| {},
            )?;
            // `--sign` — ssh-keygen -Y over the provenance anchors
            // (name/scope/version/payload digest); the armored signature
            // is stored on the index entry. The key is a *path* resolved
            // by `--key` → env → config — material never touches args.
            let signature = if sign {
                let key_path = tamias_sign::resolve_signing_key(config.as_ref(), key.as_deref())?;
                let policy = tamias_sign::SigningPolicy::resolve(config.as_ref());
                eprintln!("tamias: signing with {}", key_path.display());
                Some(tamias_sign::sign_package(
                    &plan.root,
                    &plan.manifest,
                    &key_path,
                    &policy.namespace,
                )?)
            } else {
                None
            };
            eprintln!("tamias: publishing {}", plan.manifest.name);
            let entry = plan.publish(signature.as_deref())?;
            println!(
                "published {} ({})",
                entry.label(),
                if entry.signature.is_some() {
                    "signed"
                } else {
                    "unsigned"
                }
            );
        }
        Some(Command::Search {
            query,
            registry,
            json,
        }) => {
            let config = cli_config();
            // The shared search routing (`tamias-registry-open`) — same
            // code `tamias serve`'s `search` op and `tamias ui` take.
            let (found, _target) = tamias_registry_open::search(
                &env.registry_root,
                config.as_ref(),
                registry.as_deref(),
                &query,
            )?;
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
                    println!("{}  {}  {}", e.label(), short_digest(&e.digest), e.file);
                }
            }
        }
        Some(Command::Bundle { cmd }) => {
            let BundleCommand::Pack { dir, out } = cmd;
            let packed = tamias_publish::pack(&dir, out.as_deref())?;
            println!("packed → {}", packed.display());
        }
        Some(Command::Registry { file, cmd }) => match cmd {
            RegistryCommand::Serve {
                addr,
                dir,
                max_upload,
            } => {
                let root = dir.unwrap_or_else(|| env.registry_root.clone());
                // Flag > config global `limits.max_upload` > built-in
                // default (serve_http's own fallback).
                let limit = match &max_upload {
                    Some(text) => Some(
                        tamias_config::parse_bytes(text)
                            .map_err(|e| Error::Failed(format!("--max-upload: {e}")))?,
                    ),
                    None => cli_config().and_then(|c| c.limits.max_upload),
                };
                match limit {
                    Some(max) => serve_http_with_limits(&addr, &root, max)?,
                    None => serve_http(&addr, &root)?,
                }
            }
            RegistryCommand::Add {
                name,
                kind,
                url,
                token_env,
            } => registry_add(file.as_deref(), name, kind, url, token_env)?,
            RegistryCommand::Remove { name } => registry_remove(file.as_deref(), &name)?,
            RegistryCommand::Get { name, json } => registry_get(file.as_deref(), &name, json)?,
            RegistryCommand::List { json } => registry_list(file.as_deref(), json)?,
        },
        Some(Command::DeployRegistry { out }) => {
            let out = out.unwrap_or_else(|| PathBuf::from("tamias-registry-worker"));
            // Bake the configured global upload cap into the Worker —
            // per-registry/package rules don't apply to a fresh deploy.
            let deploy = match cli_config() {
                Some(config) => tamias_deploy_cf::CfDeploy::default()
                    .with_max_upload(config.effective_limits(None, None).max_upload),
                None => tamias_deploy_cf::CfDeploy::default(),
            };
            for action in tamias_deploy::Deploy::deploy(&deploy, &out)? {
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
        Some(Command::Driver { cmd }) => match cmd {
            DriverCommand::Exec {
                name,
                verb,
                config,
                args,
            } => driver_exec(&env, &name, &verb, config.as_deref(), &args)?,
            DriverCommand::ConfigSchema { name, config } => {
                driver_config_schema(&env, &name, config.as_deref())?;
            }
        },
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
        #[cfg(feature = "tui")]
        Some(Command::Ui) => {
            // Same roots `tamias serve` gets — `config_file: None`
            // honors TAMIAS_CONFIG then ./tamias.* discovery per op.
            tamias_tui::run(&tamias_tui::Ctx::new(
                env.store_root.clone(),
                env.cache_root.clone(),
                env.registry_root.clone(),
                None,
            ))
            .map_err(Error::Failed)?;
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
            // `--file` → `TAMIAS_CONFIG` → discovery, same precedence as
            // the registry management verbs.
            let explicit = file
                .clone()
                .or_else(|| std::env::var_os(CONFIG_ENV).map(PathBuf::from));
            let (path, config) = match explicit {
                Some(p) => (Some(p.clone()), tamias_config::load_file(&p, &formats)?),
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

/// `deploy <name>` — the installed package's provider deploy step
/// (kind driver `deploy` + declarative `provider.services[]` units),
/// shared with `tamias ui` through `tamias-provider`.
fn deploy_package(env: &Env, name: &str, config: Option<&Path>) -> Result<(), Error> {
    let report = tamias_provider::deploy(&env.store_root, name, config)?;
    for line in &report.lines {
        println!("{line}");
    }
    for warning in &report.warnings {
        eprintln!("tamias: warning — {warning}");
    }
    Ok(())
}

/// `uninstall <name>` — provider teardown + drop the store entry,
/// shared with `tamias ui` through `tamias-provider`.
fn uninstall_package(env: &Env, name: &str) -> Result<(), Error> {
    let report = tamias_provider::uninstall(&env.store_root, name)?;
    for line in &report.lines {
        println!("{line}");
    }
    for warning in &report.warnings {
        eprintln!("tamias: warning — {warning}");
    }
    Ok(())
}

/// `service …` — the per-user service mechanism (systemd units on
/// Unix, Task Scheduler tasks on Windows — [`tamias_provider::ServiceBackend`]).
fn run_service(cmd: ServiceCommand) -> Result<(), Error> {
    use tamias_deploy::ServiceSpec;
    let deploy = tamias_provider::ServiceBackend::new();
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
                exec: tamias_deploy::split_exec(&exec)?,
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

/// `driver config-schema <pkg>` — the reserved `config-schema` verb:
/// resolve the installed package's store entry (dir + name + kind),
/// fetch the JSON Schema its kind driver declares for the `--config`
/// file, and print it. A driver that doesn't implement the verb exits
/// non-zero and the error surfaces its stderr — "unsupported" is a
/// normal outcome. No driver at all → a notice, not an error.
fn driver_config_schema(env: &Env, name: &str, config: Option<&Path>) -> Result<(), Error> {
    let entry = Store::new(&env.store_root).find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    match tamias_driver::config_schema(kind, &entry.dir, &entry.record.name, config)? {
        Some(schema) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&schema).map_err(|e| Error::Failed(e.to_string()))?
            );
        }
        None => {
            if kind != "files" {
                eprintln!("tamias: warning — no driver for type '{kind}'");
            }
            println!(
                "{}: no driver for type {kind} — nothing to fetch",
                entry.label()
            );
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

/// `provider list` — installed packages' `provider:` blocks. Default:
/// only packages that declare one; `--all` adds the rest as
/// provider-less rows. The data comes from `tamias-provider` — the
/// same source `tamias ui`'s Services pane reads.
fn provider_list(env: &Env, all: bool, json: bool) -> Result<(), Error> {
    let packages = tamias_provider::list(&env.store_root)?;
    let mut rows = Vec::new();
    let mut texts = Vec::new();
    for package in packages {
        let Some(view) = package.provider else {
            if all {
                rows.push(serde_json::json!({
                    "package": package.label,
                    "provider": null,
                }));
                texts.push(format!("{}  (no provider block)", package.label));
            }
            continue;
        };
        let mut service_rows = Vec::new();
        let mut service_lines = Vec::new();
        for service in &view.services {
            service_rows.push(serde_json::json!({
                "name": service.service,
                "unit": format!("{}.service", service.unit),
                "exec": service.exec,
                "state": service.state,
            }));
            service_lines.push(format!(
                "  service {} → {}.service ({})\n    exec: {}",
                service.service,
                service.unit,
                service.state.as_deref().unwrap_or("-"),
                service.exec
            ));
        }
        let ingress: Vec<serde_json::Value> = view
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
        rows.push(serde_json::json!({
            "package": package.label,
            "provider": view.base,
            "services": service_rows,
            "ingress": ingress,
            "verbs": view.verbs,
            "health": view.health,
            "dir": package.dir.display().to_string(),
        }));
        let mut block = format!("{} — provider {}", package.label, view.base);
        for line in service_lines {
            block.push('\n');
            block.push_str(&line);
        }
        for i in &view.ingress {
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
        if !view.verbs.is_empty() {
            block.push_str(&format!("\n  verbs: {}", view.verbs.join(", ")));
        }
        if let Some(cmd) = &view.health {
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
/// leak). The spec resolution lives in `tamias-provider` — shared with
/// `tamias ui`'s streamed `run`.
fn run_provider_service(
    env: &Env,
    name: &str,
    service: Option<&str>,
    extra: &[String],
) -> Result<(), Error> {
    let spec = tamias_provider::exec_spec(&env.store_root, name, service, extra)?;
    let status = tamias_shell::exec_with_env(&spec.workdir, &spec.argv, &spec.env)
        .map_err(Error::io("exec provider service"))?;
    if !status.success() {
        return Err(Error::Failed(format!("{} exited {status}", spec.service)));
    }
    Ok(())
}

/// `logs <pkg|unit>` — the host backend's log view over a provider
/// package's units (`journalctl` on systemd, `schtasks /query` on
/// Windows), or a bare unit base name (`tamias service logs` with
/// `--lines`/`--follow`).
fn logs(env: &Env, name: &str, lines: u32, follow: bool) -> Result<(), Error> {
    let deploy = tamias_provider::ServiceBackend::new();
    match Store::new(&env.store_root).find(name) {
        Ok(entry) => {
            let units = tamias_provider::units_of(&entry).map_err(|e| {
                // Keep the CLI's fuller hint for the no-provider case.
                if e.to_string().contains("no provider: block") {
                    Error::Failed(format!(
                        "{}: no provider: block — `tamias logs` needs provider.services (or pass a unit name)",
                        entry.label()
                    ))
                } else {
                    e
                }
            })?;
            deploy.logs_with(&units, lines, follow)
        }
        // Not a store entry → treat the name as a unit base name.
        Err(_) => deploy.logs_with(&[name.to_string()], lines, follow),
    }
}

/// The registry check — probes the *routed* registry exactly as
/// `publish` resolves it (`publish.default` → env/default local root;
/// `entries()` is the only network probe, kept cheap). Every other
/// configured `registries` entry is listed as info — name/kind/url,
/// no probe. A limits summary for the routed registry rides along.
fn doctor_registry(env: &Env) -> (String, bool) {
    let config = cli_config();
    let routed = config.as_ref().and_then(|c| c.publish.default.clone());
    let label = routed.clone().unwrap_or_else(|| "default".to_string());
    let (primary, ok) = match open_registry(env, config.as_ref(), routed.as_deref(), None) {
        Ok(registry) => match registry.entries() {
            Ok(entries) => (format!("{label}: {} entr(ies)", entries.len()), true),
            Err(e) => (format!("{label}: {e}"), false),
        },
        Err(e) => (format!("{label}: {e}"), false),
    };
    let mut notes = vec![primary];
    if let Some(cfg) = &config {
        for (name, reg) in &cfg.registries {
            if routed.as_deref() == Some(name.as_str()) {
                continue;
            }
            notes.push(format!(
                "{name}({}): {}",
                reg.effective_kind(),
                reg.url.as_deref().unwrap_or("-")
            ));
        }
        let eff = cfg.effective_limits(routed.as_deref(), None);
        notes.push(format!(
            "limits: upload={} download={}",
            fmt_bytes(eff.max_upload),
            fmt_bytes(eff.max_download)
        ));
    }
    (notes.join("; "), ok)
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

    use tamias_core::LockEntry;

    use super::*;
    /// Serializes tests that touch process-global state — env vars, the
    /// working directory, and `PATH`-resolved spawns. `cargo test` runs
    /// this binary's tests on threads in a single process (nextest
    /// already isolates per-test), so every test that mutates *or
    /// observes that state holds one [`ProcGuard`] for its whole body.
    static PROC: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard for [`PROC`]: snapshots the process environment and
    /// working directory on acquisition and restores both on drop, so
    /// a panicking assert can't leak `TAMIAS_*`/`PATH`/cwd mutations
    /// into sibling tests. Mutex poisoning is ignored — an unwinding
    /// sibling already restored everything via its own `Drop`.
    struct ProcGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        cwd: std::path::PathBuf,
    }

    impl ProcGuard {
        fn new() -> Self {
            let lock = PROC.lock().unwrap_or_else(|e| e.into_inner());
            let env = std::env::vars_os().collect();
            let cwd = std::env::current_dir().unwrap();
            Self {
                _lock: lock,
                env,
                cwd,
            }
        }
    }

    impl Drop for ProcGuard {
        fn drop(&mut self) {
            // Drop vars added since the snapshot, then restore the rest.
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
    fn driver_config_schema_dispatches_and_surfaces_unsupported() {
        let _pg = ProcGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        let dd = tmp.path().join("th/drivers");
        std::fs::create_dir_all(&dd).unwrap();
        let driver = dd.join("tamias-driver-csx");
        std::fs::write(
            &driver,
            "#!/bin/sh\nif [ \"$1\" = config-schema ]; then\n  echo '{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"}}}'\n  exit 0\nfi\necho '{\"types\":[\"csx\"]}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A driver that doesn't implement the verb → non-zero → Err
        // carrying its stderr.
        let bad = dd.join("tamias-driver-csbad");
        std::fs::write(
            &bad,
            "#!/bin/sh\nif [ \"$1\" = config-schema ]; then\n  echo 'config-schema: unsupported' >&2\n  exit 1\nfi\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            "name: cs\nversion: 1.0.0\ntype: csx\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
        // Schema fetch works end-to-end (stdout is the schema JSON).
        run_args(e.clone(), &["tamias", "driver", "config-schema", "cs"]).unwrap();
        // Unknown package → error.
        assert!(run_args(e.clone(), &["tamias", "driver", "config-schema", "ghost"]).is_err());
        // Unsupported verb → surfaced error, not a panic.
        let bad_dir = tmp.path().join("badpkg");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join("tamias.yaml"),
            "name: cb\nversion: 1.0.0\ntype: csbad\n",
        )
        .unwrap();
        std::fs::write(bad_dir.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", bad_dir.to_str().unwrap()]).unwrap();
        let err = run_args(e.clone(), &["tamias", "driver", "config-schema", "cb"]).unwrap_err();
        assert!(err.to_string().contains("unsupported"), "{err}");
        // No driver at all → notice, still Ok.
        let nodir = tmp.path().join("nodrv");
        std::fs::create_dir_all(&nodir).unwrap();
        std::fs::write(nodir.join("tamias.yaml"), "name: nd\nversion: 1.0.0\n").unwrap();
        std::fs::write(nodir.join("a.txt"), "a").unwrap();
        run_args(e.clone(), &["tamias", "install", nodir.to_str().unwrap()]).unwrap();
        run_args(e.clone(), &["tamias", "driver", "config-schema", "nd"]).unwrap();
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn install_rejects_empty_name() {
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let ok = Cli::try_parse_from(["tamias", "version"]).unwrap();
        assert!(matches!(run_main(ok, e.clone()), ExitCode::SUCCESS));
        let bad = Cli::try_parse_from(["tamias", "info", "ghost"]).unwrap();
        assert!(matches!(run_main(bad, e), ExitCode::FAILURE));
    }

    #[test]
    fn env_root_fallbacks() {
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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

    /// Write a `files` package dir whose manifest declares `provider:`
    /// services — `services` is the YAML list body.
    fn provider_pkg(root: &std::path::Path, name: &str, services: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            format!(
                "name: {name}\nversion: 1.0.0\nprovider:\n  name: shared\n  services:\n{services}"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        dir
    }

    #[test]
    fn uninstall_keeps_unit_still_declared_by_a_survivor() {
        let _pg = ProcGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("cfg"));
        // Two packages share `provider.name: shared` — their `web`
        // services render ONE unit (`shared-web`); `a` also declares a
        // private `solo` service (`shared-solo`).
        let a = provider_pkg(
            tmp.path(),
            "a",
            "    - name: web\n      exec: /bin/serve --shared\n    - name: solo\n      exec: /bin/solo\n",
        );
        let b = provider_pkg(
            tmp.path(),
            "b",
            "    - name: web\n      exec: /bin/serve --shared\n",
        );
        for dir in [&a, &b] {
            run_args(e.clone(), &["tamias", "install", dir.to_str().unwrap()]).unwrap();
            run_args(
                e.clone(),
                &[
                    "tamias",
                    "deploy",
                    &dir.file_name().unwrap().to_string_lossy(),
                ],
            )
            .unwrap();
        }
        let shared = tmp.path().join("cfg/systemd/user/shared-web.service");
        let solo = tmp.path().join("cfg/systemd/user/shared-solo.service");
        assert!(shared.is_file() && solo.is_file());
        // Uninstalling `a` must not drop the unit `b` still declares —
        // it is re-rendered from b's spec — while `a`'s private unit goes.
        run_args(e.clone(), &["tamias", "uninstall", "a"]).unwrap();
        assert!(shared.is_file(), "shared unit must survive");
        assert!(std::fs::read_to_string(&shared)
            .unwrap()
            .contains("ExecStart=/bin/serve --shared"));
        assert!(!solo.exists(), "unshared unit must be removed");
        // Uninstalling the last declarer removes the shared unit.
        run_args(e.clone(), &["tamias", "uninstall", "b"]).unwrap();
        assert!(!shared.exists());
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn add_with_existing_manifest_and_service_verbs() {
        let _pg = ProcGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let dir = pkg(tmp.path());
        {
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();
            std::fs::write(tmp.path().join("tamias.yaml"), "name: \"-x\"\n").unwrap();
            let r = run_args(e.clone(), &["tamias", "add", dir.to_str().unwrap()]);
            std::env::set_current_dir(&prev).unwrap();
            assert!(r.is_err());
        }
        {
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
        // update/pin consult the cwd `tamias.lock` — ProcGuard serializes
        // cwd mutations so a sibling can't swap in a foreign lock.
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
        use tamias_registry_local::{serve_http, LocalRegistry};
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let src = pkg(tmp.path());
        let reg = tmp.path().join("reg");
        let manifest = Manifest::load(&src).unwrap();
        LocalRegistry::new(&reg)
            .publish(&src, &manifest, None)
            .unwrap();
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
        let mut expected = vec![
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
        ];
        // `ui` exists only under the default `tui` feature.
        #[cfg(feature = "tui")]
        expected.push("ui");
        assert_eq!(cli_verbs(), expected);
    }

    #[test]
    fn cache_clean_all_conflicts_with_days() {
        assert!(Cli::try_parse_from(["tamias", "cache", "clean", "--all", "--days", "3"]).is_err());
        assert!(Cli::try_parse_from(["tamias", "cache", "clean", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["tamias", "cache", "clean", "--days", "3"]).is_ok());
    }

    #[test]
    fn sync_lock_only_refreshes_pinned_entries() {
        let _pg = ProcGuard::new();
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
            signature: None,
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
        let _pg = ProcGuard::new();
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
        let _pg = ProcGuard::new();
        use std::os::unix::fs::PermissionsExt;
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
        let _pg = ProcGuard::new();
        // tamias-config is wired for store/cache roots: a project
        // `tamias.yml` `store.root`/`cache.root` become the Env roots
        // when the env vars are unset; `kind: cf` is reserved → ignored.
        // ProcGuard holds the single process-state lock — no ordering.
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
        assert_eq!(tamias_provider::unquote_env("\"a b\""), "a b");
        assert_eq!(tamias_provider::unquote_env("'a b'"), "a b");
        assert_eq!(tamias_provider::unquote_env("plain"), "plain");
        assert_eq!(tamias_provider::unquote_env("\"unclosed"), "\"unclosed");
        assert_eq!(tamias_provider::unquote_env("\"\""), "");
    }

    #[cfg(unix)]
    #[test]
    fn doctor_fails_on_unwritable_roots() {
        let _pg = ProcGuard::new();
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

    #[test]
    fn registry_management_verbs() {
        let _pg = ProcGuard::new();
        // ambient env must not redirect discovery/`--file` resolution
        let saved_cfg = std::env::var_os(CONFIG_ENV);
        let saved_reg = std::env::var_os("TAMIAS_REGISTRY");
        std::env::remove_var(CONFIG_ENV);
        std::env::remove_var("TAMIAS_REGISTRY");
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let cfg = tmp.path().join("tamias.yml");
        let regdir = tmp.path().join("reg1");
        let f = cfg.to_str().unwrap();
        // add creates the file when none exists
        run_args(
            e.clone(),
            &[
                "tamias",
                "registry",
                "--file",
                f,
                "add",
                "mine",
                "--kind",
                "local",
                "--url",
                regdir.to_str().unwrap(),
            ],
        )
        .unwrap();
        assert!(cfg.is_file());
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("mine:"), "{text}");
        assert!(text.contains("kind: local"), "{text}");
        // a second registry, http + token_env
        run_args(
            e.clone(),
            &[
                "tamias",
                "registry",
                "--file",
                f,
                "add",
                "web",
                "--kind",
                "http",
                "--url",
                "https://reg.example.com",
                "--token-env",
                "MY_REG_TOKEN",
            ],
        )
        .unwrap();
        // duplicate name → error; http kind with a non-http url → error
        assert!(run_args(
            e.clone(),
            &["tamias", "registry", "--file", f, "add", "mine", "--kind", "local", "--url", "/x"]
        )
        .is_err());
        assert!(run_args(
            e.clone(),
            &[
                "tamias",
                "registry",
                "--file",
                f,
                "add",
                "bad",
                "--kind",
                "http",
                "--url",
                "/not-a-url"
            ]
        )
        .is_err());
        // list (text + json), get (text + json)
        run_args(e.clone(), &["tamias", "registry", "--file", f, "list"]).unwrap();
        run_args(
            e.clone(),
            &["tamias", "registry", "--file", f, "list", "--json"],
        )
        .unwrap();
        run_args(
            e.clone(),
            &["tamias", "registry", "--file", f, "get", "web"],
        )
        .unwrap();
        run_args(
            e.clone(),
            &["tamias", "registry", "--file", f, "get", "web", "--json"],
        )
        .unwrap();
        assert!(run_args(
            e.clone(),
            &["tamias", "registry", "--file", f, "get", "ghost"]
        )
        .is_err());
        // remove → get errors; removing again errors
        run_args(
            e.clone(),
            &["tamias", "registry", "--file", f, "remove", "web"],
        )
        .unwrap();
        assert!(run_args(
            e.clone(),
            &["tamias", "registry", "--file", f, "get", "web"]
        )
        .is_err());
        assert!(run_args(
            e.clone(),
            &["tamias", "registry", "--file", f, "remove", "web"]
        )
        .is_err());
        // discovery: an empty dir gets a fresh tamias.yml on add
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        {
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(&proj).unwrap();
            let r = run_args(
                e.clone(),
                &[
                    "tamias",
                    "registry",
                    "add",
                    "disc",
                    "--kind",
                    "cf",
                    "--url",
                    "https://cf.example.com",
                ],
            );
            std::env::set_current_dir(&prev).unwrap();
            r.unwrap();
        }
        assert!(proj.join("tamias.yml").is_file());
        // TOML target renders TOML
        let toml_path = tmp.path().join("tamias.toml");
        run_args(
            e.clone(),
            &[
                "tamias",
                "registry",
                "--file",
                toml_path.to_str().unwrap(),
                "add",
                "t",
                "--kind",
                "local",
                "--url",
                "/tmp/r",
            ],
        )
        .unwrap();
        assert!(std::fs::read_to_string(&toml_path)
            .unwrap()
            .contains("kind = \"local\""));
        match saved_cfg {
            Some(v) => std::env::set_var(CONFIG_ENV, v),
            None => std::env::remove_var(CONFIG_ENV),
        }
        match saved_reg {
            Some(v) => std::env::set_var("TAMIAS_REGISTRY", v),
            None => std::env::remove_var("TAMIAS_REGISTRY"),
        }
    }

    #[test]
    fn publish_search_route_through_named_registries() {
        let _pg = ProcGuard::new();
        let saved_cfg = std::env::var_os(CONFIG_ENV);
        let saved_reg = std::env::var_os("TAMIAS_REGISTRY");
        std::env::remove_var("TAMIAS_REGISTRY");
        std::env::remove_var(CONFIG_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        // a named local registry + an http-served one
        let local_reg = tmp.path().join("local-reg");
        let http_reg = tmp.path().join("http-reg");
        let http_root = http_reg.clone();
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        std::thread::spawn(move || {
            let _ = tamias_registry_local::serve_http(&format!("127.0.0.1:{port}"), &http_root);
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::env::set_var("MY_REG_TOKEN", "sekret");
        std::fs::write(
            proj.join("tamias.yml"),
            format!(
                "registries:\n  mine:\n    kind: local\n    url: {}\n  web:\n    kind: http\n    url: http://127.0.0.1:{port}\n    token_env: MY_REG_TOKEN\npublish:\n  default: mine\nsearch:\n  default: web\n",
                local_reg.display()
            ),
        )
        .unwrap();
        let dir = pkg(tmp.path());
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&proj).unwrap();
        // bare publish → publish.default (named local registry)
        let r1 = run_args(e.clone(), &["tamias", "publish", dir.to_str().unwrap()]);
        // bare search → search.default (named http registry)
        let r2 = run_args(e.clone(), &["tamias", "search", "w"]);
        // --registry by name on both verbs
        let r3 = run_args(e.clone(), &["tamias", "search", "w", "--registry", "mine"]);
        std::env::set_current_dir(&prev).unwrap();
        std::env::remove_var("MY_REG_TOKEN");
        for (k, v) in [(CONFIG_ENV, saved_cfg), ("TAMIAS_REGISTRY", saved_reg)] {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        r1.unwrap();
        r2.unwrap();
        r3.unwrap();
        // the publish landed in the named local registry, not env.registry_root
        assert!(local_reg.join("index.json").is_file());
        assert!(!e.registry_root.join("index.json").exists());
        // and the http registry received nothing (search only reads)
        assert!(!http_reg.join("index.json").exists());
    }

    #[test]
    fn registry_env_wins_over_config_default() {
        let _pg = ProcGuard::new();
        let saved_cfg = std::env::var_os(CONFIG_ENV);
        std::env::remove_var(CONFIG_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let named = tmp.path().join("named-reg");
        let env_reg = tmp.path().join("env-reg");
        std::fs::write(
            proj.join("tamias.yml"),
            format!(
                "registries:\n  mine:\n    kind: local\n    url: {}\npublish:\n  default: mine\n",
                named.display()
            ),
        )
        .unwrap();
        let saved = std::env::var_os("TAMIAS_REGISTRY");
        std::env::set_var("TAMIAS_REGISTRY", &env_reg);
        let dir = pkg(tmp.path());
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&proj).unwrap();
        let r = run_args(e.clone(), &["tamias", "publish", dir.to_str().unwrap()]);
        std::env::set_current_dir(&prev).unwrap();
        match saved {
            Some(v) => std::env::set_var("TAMIAS_REGISTRY", v),
            None => std::env::remove_var("TAMIAS_REGISTRY"),
        }
        match saved_cfg {
            Some(v) => std::env::set_var(CONFIG_ENV, v),
            None => std::env::remove_var(CONFIG_ENV),
        }
        r.unwrap();
        // env var won: payload went to the env dir, not the config name
        assert!(env_reg.join("index.json").is_file());
        assert!(!named.join("index.json").exists());
    }

    #[test]
    fn registry_target_resolution_order() {
        let _pg = ProcGuard::new();
        let saved = std::env::var_os("TAMIAS_REGISTRY");
        std::env::set_var("TAMIAS_REGISTRY", "envreg");
        // flag > env > config defaults (first set wins)
        assert_eq!(
            tamias_registry_open::registry_target(Some("flag"), &[Some("cfg")]),
            Some("flag".to_string())
        );
        assert_eq!(
            tamias_registry_open::registry_target(None, &[Some("cfg")]),
            Some("envreg".to_string())
        );
        std::env::remove_var("TAMIAS_REGISTRY");
        assert_eq!(
            tamias_registry_open::registry_target(None, &[Some("cfg")]),
            Some("cfg".to_string())
        );
        assert_eq!(
            tamias_registry_open::registry_target(None, &[None, Some("pub")]),
            Some("pub".to_string())
        );
        assert_eq!(tamias_registry_open::registry_target(None, &[None]), None);
        match saved {
            Some(v) => std::env::set_var("TAMIAS_REGISTRY", v),
            None => std::env::remove_var("TAMIAS_REGISTRY"),
        }
    }

    #[test]
    fn fmt_bytes_renders_human_sizes() {
        assert_eq!(fmt_bytes(0), "0B");
        assert_eq!(fmt_bytes(512 << 20), "512MiB");
        assert_eq!(fmt_bytes(1 << 30), "1GiB");
        assert_eq!(fmt_bytes(1536), "1536B");
    }

    /// A second payload dir with a different package name — for
    /// per-package limit overrides.
    fn pkg_named(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            format!("name: {name}\nversion: 1.0.0\n"),
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        dir
    }

    #[test]
    fn publish_enforces_configured_upload_limits() {
        let _pg = ProcGuard::new();
        let saved_cfg = std::env::var_os(CONFIG_ENV);
        let saved_reg = std::env::var_os("TAMIAS_REGISTRY");
        std::env::remove_var(CONFIG_ENV);
        std::env::remove_var("TAMIAS_REGISTRY");
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        // global cap of 10 bytes rejects every publish; the `w`
        // package override (1 MiB) and the `mine` registry's own cap
        // exercise the resolution chain.
        let named_reg = tmp.path().join("named-reg");
        std::fs::write(
            proj.join("tamias.yml"),
            format!(
                "limits:\n  max_upload: 10\n  packages:\n    w:\n      max_upload: 1048576\nregistries:\n  mine:\n    kind: local\n    url: {}\n    limits:\n      max_upload: 10\n",
                named_reg.display()
            ),
        )
        .unwrap();
        let w = pkg(tmp.path());
        let other = pkg_named(tmp.path(), "other");
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&proj).unwrap();
        // `w` matches the package override → publishes to the default
        // (env) registry root.
        let ok = run_args(e.clone(), &["tamias", "publish", w.to_str().unwrap()]);
        // `other` has no override → the 10-byte global cap rejects.
        let err = run_args(e.clone(), &["tamias", "publish", other.to_str().unwrap()]);
        // `--registry mine`: the package override still wins over the
        // registry's own 10-byte cap (package rules resolve first), so
        // `w` publishes; `other` falls through to the registry cap.
        let named_w = run_args(
            e.clone(),
            &[
                "tamias",
                "publish",
                w.to_str().unwrap(),
                "--registry",
                "mine",
            ],
        );
        let named_other = run_args(
            e.clone(),
            &[
                "tamias",
                "publish",
                other.to_str().unwrap(),
                "--registry",
                "mine",
            ],
        );
        std::env::set_current_dir(&prev).unwrap();
        match saved_cfg {
            Some(v) => std::env::set_var(CONFIG_ENV, v),
            None => std::env::remove_var(CONFIG_ENV),
        }
        match saved_reg {
            Some(v) => std::env::set_var("TAMIAS_REGISTRY", v),
            None => std::env::remove_var("TAMIAS_REGISTRY"),
        }
        ok.unwrap();
        named_w.unwrap();
        let err = err.unwrap_err().to_string();
        assert!(err.contains("upload limit"), "{err}");
        assert!(named_other
            .unwrap_err()
            .to_string()
            .contains("upload limit"));
        // both successful publishes landed in their respective roots
        assert!(e.registry_root.join("index.json").is_file());
        assert!(named_reg.join("index.json").is_file());
    }

    #[test]
    fn deploy_registry_bakes_configured_upload_limit() {
        let _pg = ProcGuard::new();
        let saved_cfg = std::env::var_os(CONFIG_ENV);
        std::env::remove_var(CONFIG_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let e = env(tmp.path());
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("tamias.yml"), "limits:\n  max_upload: 4096\n").unwrap();
        let out = tmp.path().join("worker");
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&proj).unwrap();
        let r = run_args(
            e.clone(),
            &["tamias", "deploy-registry", "--out", out.to_str().unwrap()],
        );
        // …and with no config at all → the built-in default is baked.
        let bare = tmp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        std::env::set_current_dir(&bare).unwrap();
        let out2 = tmp.path().join("worker2");
        let r2 = run_args(
            e.clone(),
            &["tamias", "deploy-registry", "--out", out2.to_str().unwrap()],
        );
        std::env::set_current_dir(&prev).unwrap();
        match saved_cfg {
            Some(v) => std::env::set_var(CONFIG_ENV, v),
            None => std::env::remove_var(CONFIG_ENV),
        }
        r.unwrap();
        r2.unwrap();
        let toml = std::fs::read_to_string(out.join("wrangler.toml")).unwrap();
        assert!(toml.contains("MAX_UPLOAD_BYTES = \"4096\""), "{toml}");
        let toml2 = std::fs::read_to_string(out2.join("wrangler.toml")).unwrap();
        assert!(
            toml2.contains("MAX_UPLOAD_BYTES = \"536870912\""),
            "{toml2}"
        );
        // the worker itself reads the var (server-side enforcement)
        let js = std::fs::read_to_string(out.join("index.js")).unwrap();
        assert!(js.contains("env.MAX_UPLOAD_BYTES"));
        assert!(js.contains("status: 413"));
    }
}
