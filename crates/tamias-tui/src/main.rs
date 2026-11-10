//! tamias-tui — standalone entry; `tamias ui` embeds the same
//! `tamias_tui::run`. Roots come from `TAMIAS_HOME`/`TAMIAS_CACHE`/
//! `TAMIAS_REGISTRY` (or the XDG/`~/.local` defaults) — no project
//! config layer here; `tamias ui` is the full-fidelity entry.

fn main() {
    if let Err(e) = tamias_tui::run(&tamias_tui::Ctx::from_env()) {
        eprintln!("tamias-tui: error: {e}");
        std::process::exit(1);
    }
}
