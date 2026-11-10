#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-tui — the interactive package-manager surface behind
//! `tamias ui` (ratatui/crossterm, PL-0004). Phase 1 is a read-only
//! skeleton: the Installed pane lists store entries with a
//! client-side `/` filter; the Registry/Services/Manifest/Author
//! panes are placeholders until their phases land. Terminal
//! discipline mirrors `myco-tui`: alternate screen, raw mode, a
//! drop guard, a panic hook, mouse capture, and a `notify` watcher
//! on the store root that reloads the entry list when installs land
//! from another `tamias` invocation.
//!
//! The crate stays engine-free: it reads the store through
//! `tamias-store-local` directly and takes its roots from [`Ctx`] —
//! `tamias ui` builds one from the CLI env; the standalone
//! `tamias-tui` bin reads `TAMIAS_HOME`/`TAMIAS_CACHE`/`TAMIAS_REGISTRY`.
//! Later phases call the shared ops (`tamias-install`,
//! `tamias-registry-open`, `tamias-serve`) on worker threads — the
//! Ctx fields are already the serve `Env` shape so nothing has to
//! be re-plumbed.

mod app;
mod render;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Where the TUI reads/writes — the same roots `tamias-serve`'s
/// `Env` carries, so CLI, serve, and TUI resolve identically.
/// `config_file: None` honors `TAMIAS_CONFIG` then `./tamias.*`
/// discovery (per-op, like serve).
#[derive(Clone, Debug)]
pub struct Ctx {
    /// The local store root (`<TAMIAS_HOME>/store` by default).
    pub store_root: PathBuf,
    /// The fetch-cache root.
    pub cache_root: PathBuf,
    /// The default local-registry root.
    pub registry_root: PathBuf,
    /// Explicit config file for registry-aware ops — `None` honors
    /// `TAMIAS_CONFIG` then `./tamias.{yml,yaml,toml}` discovery.
    pub config_file: Option<PathBuf>,
}

impl Ctx {
    /// All four roots explicit.
    #[must_use]
    pub fn new(
        store_root: impl Into<PathBuf>,
        cache_root: impl Into<PathBuf>,
        registry_root: impl Into<PathBuf>,
        config_file: Option<PathBuf>,
    ) -> Self {
        Self {
            store_root: store_root.into(),
            cache_root: cache_root.into(),
            registry_root: registry_root.into(),
            config_file,
        }
    }

    /// Roots from the `TAMIAS_*` env vars with the same
    /// `~/.local/share`/`~/.cache` fallbacks the CLI uses — the
    /// standalone bin's whole config story (no project config layer;
    /// `tamias ui` is the full-fidelity entry).
    #[must_use]
    pub fn from_env() -> Self {
        let data = || {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .unwrap_or_else(|| PathBuf::from(".tamias"))
        };
        let cache = || {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
                .unwrap_or_else(|| PathBuf::from(".tamias"))
        };
        let store_root = std::env::var_os("TAMIAS_HOME")
            .map(|h| PathBuf::from(h).join("store"))
            .or_else(|| Some(data().join("tamias/store")))
            .unwrap_or_else(|| PathBuf::from(".tamias/store"));
        let cache_root = std::env::var_os("TAMIAS_CACHE")
            .map(PathBuf::from)
            .or_else(|| Some(cache().join("tamias/fetch")))
            .unwrap_or_else(|| PathBuf::from(".tamias/fetch"));
        let registry_root = std::env::var_os("TAMIAS_REGISTRY")
            .map(PathBuf::from)
            .or_else(|| Some(data().join("tamias/registry")))
            .unwrap_or_else(|| PathBuf::from(".tamias/registry"));
        Self::new(store_root, cache_root, registry_root, None)
    }
}

/// Restore the terminal on any exit path.
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

/// Panic hook — restore the terminal *before* the default hook
/// prints, otherwise the panic message is erased by the alt-screen
/// exit and the shell is left in raw mode.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        prev(info);
    }));
}

/// Launch the TUI against `ctx`'s roots.
pub fn run(ctx: &Ctx) -> Result<(), String> {
    let mut model = app::Model::open(ctx)?;
    enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    // Guard first — raw mode is on from here, so every fallible
    // step below (alt screen, terminal init) must restore on `?`.
    let _guard = Guard;
    install_panic_hook();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|e| format!("alt screen: {e}"))?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(stdout)).map_err(|e| format!("terminal: {e}"))?;

    loop {
        terminal
            .draw(|f| render::draw(f, &mut model))
            .map_err(|e| format!("draw: {e}"))?;
        if event::poll(Duration::from_millis(250)).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                    if model.key(k) {
                        break;
                    }
                }
                Event::Mouse(m) => model.mouse(m),
                Event::Resize(..) => {}
                _ => {}
            }
        }
        // auto-refresh — pick up installs/removes that landed while
        // we were idle (watcher dirty flag + slow fallback scan)
        model.tick();
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The hook body itself — a caught panic still runs it, which
    /// exercises the terminal-restore path (a no-op without a tty).
    #[test]
    fn panic_hook_restores_the_terminal() {
        install_panic_hook();
        let r = std::panic::catch_unwind(|| panic!("boom"));
        assert!(r.is_err());
    }

    #[test]
    fn ctx_from_env_resolves_roots() {
        // never panics; roots may point anywhere — this only pins
        // the shape for the standalone bin
        let ctx = Ctx::from_env();
        assert!(!ctx.store_root.as_os_str().is_empty());
        assert!(ctx.config_file.is_none());
    }
}
