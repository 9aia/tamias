#![cfg_attr(coverage, feature(coverage_attribute))]
//! xtask — repo chores. `cargo xtask <verb>` via the alias in
//! `.cargo/config.toml`.
//!
//! - `check`    — `cargo fmt --check` + `cargo clippy -D warnings`
//! - `test`     — `cargo nextest run` when installed, else `cargo test`
//! - `coverage` — `cargo llvm-cov` (HTML → target/llvm-cov/html);
//!   extra args pass through, e.g. `--fail-under-lines 97`
//! - `install`  — release-install `tamias` into ~/.cargo/bin
//! - `package`  — `cargo package --workspace` (verify: builds each packaged crate)
//! - `smoke`    — install `tamias` from crates.io + run it (post-publish check)
//! - `site`     — build the docs site (docs/ worktree, VitePress)

use std::process::{Command, ExitStatus};

fn run(args: &[&str]) -> ExitStatus {
    let empty: &[&str] = &[];
    let (prog, rest) = args.split_first().unwrap_or((&"true", empty));
    Command::new(prog).args(rest).status().unwrap_or_else(|e| {
        eprintln!("xtask: failed to spawn {prog}: {e}");
        std::process::exit(1)
    })
}

fn run_in(dir: &str, args: &[&str]) -> ExitStatus {
    let empty: &[&str] = &[];
    let (prog, rest) = args.split_first().unwrap_or((&"true", empty));
    Command::new(prog)
        .args(rest)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("xtask: failed to spawn {prog}: {e}");
            std::process::exit(1)
        })
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn have_cargo_sub(sub: &str) -> bool {
    Command::new("cargo")
        .args([sub, "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `tamias-driver-tamias` is excluded from the workspace (its pavo-protocol
/// dep is a sibling-checkout path dep until pavo-protocol is published), so
/// `--workspace` commands skip it. When the pavo checkout is present we still
/// fmt/clippy/test it via its own manifest; on a fresh clone it's skipped.
const DRIVER_TAMIAS_MANIFEST: &str = "crates/tamias-driver-tamias/Cargo.toml";

fn driver_tamias_available() -> bool {
    std::path::Path::new(DRIVER_TAMIAS_MANIFEST).exists()
        && std::path::Path::new("../pavo/crates/pavo-protocol/Cargo.toml").exists()
}

fn check() -> i32 {
    let mut ok = run(&["cargo", "fmt", "--all", "--", "--check"]).success();
    if ok && driver_tamias_available() {
        ok = run(&[
            "cargo",
            "fmt",
            "--manifest-path",
            DRIVER_TAMIAS_MANIFEST,
            "--",
            "--check",
        ])
        .success();
    }
    ok = ok
        && run(&[
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ])
        .success();
    if ok && driver_tamias_available() {
        ok = run(&[
            "cargo",
            "clippy",
            "--manifest-path",
            DRIVER_TAMIAS_MANIFEST,
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ])
        .success();
    }
    i32::from(!ok)
}

fn test() -> i32 {
    let nextest = have_cargo_sub("nextest");
    let mut code = if nextest {
        run(&["cargo", "nextest", "run", "--workspace"]).code()
    } else {
        run(&["cargo", "test", "--workspace"]).code()
    };
    if code == Some(0) && driver_tamias_available() {
        code = if nextest {
            run(&[
                "cargo",
                "nextest",
                "run",
                "--manifest-path",
                DRIVER_TAMIAS_MANIFEST,
            ])
            .code()
        } else {
            run(&["cargo", "test", "--manifest-path", DRIVER_TAMIAS_MANIFEST]).code()
        };
    }
    code.unwrap_or(1)
}

fn coverage(extra: &[String]) -> i32 {
    if !have_cargo_sub("llvm-cov") {
        eprintln!("cargo-llvm-cov not installed — cargo install cargo-llvm-cov");
        return 1;
    }
    let mut args = vec!["cargo", "llvm-cov"];
    if have_cargo_sub("nextest") {
        args.push("nextest");
        args.extend(["--workspace", "--no-tests=pass"]);
    } else {
        args.push("--workspace");
    }
    if extra.is_empty() {
        args.push("--html");
    }
    let owned: Vec<&str> = extra.iter().map(String::as_str).collect();
    args.extend(owned);
    run(&args).code().unwrap_or(1)
}

fn install() -> i32 {
    run(&["cargo", "install", "--path", "crates/tamias-cli", "--force"])
        .code()
        .unwrap_or(1)
}

/// Package + *verify* every crate the way crates.io will see them — catches
/// files that escape the crate (`include_str!("../../sibling/…")`) and any
/// build that only works in-workspace. Run before a release.
fn package() -> i32 {
    run(&["cargo", "package", "--workspace", "--allow-dirty"])
        .code()
        .unwrap_or(1)
}

/// Registry-only smoke test: install `tamias` from crates.io into a temp root
/// and run it. This is the **only** check that catches a *stale already-published
/// dependency* — in-workspace builds hide it, and `cargo package --workspace`
/// resolves workspace members by path, not by registry version.
/// Run after publishing (see `scripts/release.sh`).
fn smoke() -> i32 {
    let root = std::env::temp_dir().join("tamias-smoke");
    let _ = std::fs::remove_dir_all(&root);
    let root_s = root.to_string_lossy().into_owned();
    if run(&["cargo", "install", "tamias", "--root", &root_s, "--force"]).code() != Some(0) {
        return 1;
    }
    let bin = root
        .join("bin")
        .join("tamias")
        .to_string_lossy()
        .into_owned();
    run(&[&bin, "version"]).code().unwrap_or(1)
}

fn site() -> i32 {
    if !std::path::Path::new("docs/package.json").exists() {
        eprintln!("no docs/ worktree — `git worktree add --orphan -b docs docs` first");
        return 1;
    }
    let pkg = if have("bun") { "bun" } else { "npm" };
    let _ = run_in("docs", &[pkg, "install"]).success();
    run_in("docs", &[pkg, "run", "docs:build"])
        .code()
        .unwrap_or(1)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    let code = match cmd.as_str() {
        "check" => check(),
        "test" => test(),
        "coverage" => coverage(&rest),
        "install" => install(),
        "package" => package(),
        "smoke" => smoke(),
        "site" => site(),
        other => {
            eprintln!(
                "usage: cargo xtask <check|test|coverage|install|package|smoke|site>\n  unknown verb: {other:?}"
            );
            2
        }
    };
    std::process::exit(code);
}
