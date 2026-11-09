//! Driver conformance gate — runs pavo's wire-contract suite
//! ([`pavo_testkit::conformance::check_driver`]) against this crate's
//! own `tamias-driver-tamias serve` binary.
//!
//! The driver is read-only over the tamias install store, fetch cache,
//! registry index, and driver dir. The spawned driver inherits this
//! test's cwd (`crates/tamias-driver-tamias`, inside the tamias repo).

use std::path::{Path, PathBuf};

/// Path to the driver binary under test. `env!("CARGO_BIN_EXE_…")` is
/// baked at compile time — a stale or shared `target/` (repo moved,
/// `CARGO_TARGET_DIR` reuse across checkouts) can leave it pointing at
/// a path that no longer exists. Fall back to the bin next to this
/// test executable: `target/<profile>/deps/<test>` → `target/<profile>/`.
fn driver_bin() -> PathBuf {
    let baked = PathBuf::from(env!("CARGO_BIN_EXE_tamias-driver-tamias"));
    if baked.exists() {
        return baked;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|deps| deps.parent().map(Path::to_path_buf))
        .map(|profile| profile.join("tamias-driver-tamias"))
        .filter(|p| p.exists())
        .unwrap_or(baked)
}

#[test]
fn driver_passes_pavo_conformance() {
    let violations = pavo_testkit::conformance::check_driver(&driver_bin());
    assert!(
        violations.is_empty(),
        "pavo conformance violations:\n{}",
        violations
            .iter()
            .map(|v| format!("  - {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
