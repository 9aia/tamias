//! tamias-serve — standalone ndjson JSON-RPC server binary. Same wire
//! as `tamias serve` (the CLI subcommand embeds this crate's lib), but
//! installs without the CLI's extra surface — notably no TUI stack.
//! Roots resolve exactly like the CLI: `TAMIAS_*`/XDG env vars win,
//! then `./tamias.{yml,yaml,toml}` config fills what env leaves unset.

use std::path::PathBuf;

use tamias_config::Location;
use tamias_serve::Env;

fn main() -> std::process::ExitCode {
    let _telemetry = tamias_telemetry::init();
    let config = cli_config();
    let env = Env {
        store_root: store_root(config.as_ref()),
        cache_root: cache_root(config.as_ref()),
        registry_root: registry_root(),
        // `None` → ops honor `TAMIAS_CONFIG` then `./tamias.{yml,yaml,toml}`
        // discovery per request, so `registry.add` is visible to a later
        // `search` in the same session.
        config_file: None,
    };
    match tamias_serve::serve(env) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tamias-serve: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The project config in the cwd, if any — warn-and-ignore like the
/// CLI: a broken config warns and yields `None`.
fn cli_config() -> Option<tamias_config::Config> {
    match tamias_registry_open::load_config(None) {
        Ok(config) => Some(config),
        Err(e) => {
            eprintln!("tamias-serve: warning — ignoring config file: {e}");
            None
        }
    }
}

/// A config `Location` → its `root` as a path. Only `local` (or unset)
/// kinds resolve — `cf` and anything else warn and yield `None`.
fn location_root(loc: &Location, what: &str) -> Option<String> {
    match loc.kind.as_deref() {
        None | Some("local") => loc.root.clone(),
        Some(kind) => {
            eprintln!(
                "tamias-serve: warning — {what}.kind '{kind}' is not wired yet; ignoring {what}.root"
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
