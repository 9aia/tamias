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
//! - `pack-self` — release-build `tamias` → self-package `.tgz` (ADR-0003)
//! - `smoke`    — install `tamias` from crates.io + run it (post-publish check)
//! - `site`     — build the docs site (docs/ worktree, VitePress)

use std::path::{Path, PathBuf};
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

/// `tamias-driver-tamias` is excluded from the workspace (it keeps its
/// own `[workspace]` + lockfile and ships as a standalone driver binary),
/// so `--workspace` commands skip it. We still fmt/clippy/test it via its
/// own manifest whenever the crate is present.
const DRIVER_TAMIAS_MANIFEST: &str = "crates/tamias-driver-tamias/Cargo.toml";

fn driver_tamias_available() -> bool {
    std::path::Path::new(DRIVER_TAMIAS_MANIFEST).exists()
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

/// Parsed `pack-self` flags — xtask has no clap dep; the surface is
/// small enough for hand parsing (`--flag` / `--flag value` /
/// `--flag=value`).
#[derive(Default)]
struct PackSelfArgs {
    /// Use `target/release/tamias` as-is — no `cargo build`.
    skip_build: bool,
    /// Package an explicit binary (implies no build) — tests +
    /// cross-compiled trees.
    bin: Option<PathBuf>,
    /// Version override; default reads `crates/tamias-cli/Cargo.toml`.
    version: Option<String>,
    /// Output dir for the stage + `.tgz` (default `target/pack-self/`).
    out: Option<PathBuf>,
}

fn parse_pack_self(rest: &[String]) -> Result<PackSelfArgs, String> {
    let mut args = PackSelfArgs::default();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        let mut take = |name: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            it.next()
                .cloned()
                .ok_or_else(|| format!("pack-self: {name} needs a value"))
        };
        match flag {
            "--skip-build" => args.skip_build = true,
            "--bin" => args.bin = Some(PathBuf::from(take("--bin")?)),
            "--version" => args.version = Some(take("--version")?),
            "--out" => args.out = Some(PathBuf::from(take("--out")?)),
            "--help" | "-h" => {
                println!(
                    "usage: cargo xtask pack-self [--skip-build] [--bin <path>] \
                     [--version <v>] [--out <dir>]\n\
                     \x20 release-builds `tamias` (crates/tamias-cli) unless --skip-build/\
                     --bin, stages a `type: files` package (tamias.yaml + bin/tamias),\n\
                     \x20 and packs it via tamias-publish into <out>/tamias-<version>.tgz \
                     (default out: target/pack-self/)"
                );
                std::process::exit(0);
            }
            other => return Err(format!("pack-self: unknown flag {other:?}")),
        }
    }
    Ok(args)
}

/// `version` (+ `description`) from a crate manifest's `[package]`
/// section — a line scan, not a TOML parse: xtask stays dep-light and
/// the fields it needs are plain `key = "value"` lines.
fn package_fields(manifest: &Path) -> Result<(String, String), String> {
    let text = std::fs::read_to_string(manifest)
        .map_err(|e| format!("read {}: {e}", manifest.display()))?;
    let mut in_package = false;
    let (mut version, mut description) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        // `key = "value"` only — `version.workspace = true` and the
        // like must not masquerade as a literal.
        for (key, slot) in [("version", &mut version), ("description", &mut description)] {
            if let Some(rest) = line.strip_prefix(key) {
                if let Some(v) = rest.trim_start().strip_prefix('=') {
                    *slot = Some(v.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    let version = version
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("no version in {}", manifest.display()))?;
    let description = description.unwrap_or_else(|| "tamias".to_string());
    Ok((version, description))
}

/// The cli crate manifest — cwd-relative first (the usual
/// `cargo xtask` invocation), then the compile-time xtask dir so the
/// verb still resolves when run from a scratch cwd.
fn cli_manifest() -> PathBuf {
    let rel = Path::new("crates/tamias-cli/Cargo.toml");
    if rel.exists() {
        return rel.to_path_buf();
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/tamias-cli/Cargo.toml")
}

/// Stage the self-package payload: `tamias.yaml` manifest + `bin/tamias`
/// ← the release binary. Returns the stage dir (caller owns cleanup —
/// it lives under `--out` for inspection).
fn stage_package(
    stage: &Path,
    bin_src: &Path,
    version: &str,
    description: &str,
) -> Result<(), String> {
    let bin_dir = stage.join("bin");
    std::fs::create_dir_all(&bin_dir).map_err(|e| format!("create {}: {e}", bin_dir.display()))?;
    // `type: files` is the built-in kind — no driver needed; `tamias bin`
    // links `bin/` payloads into the bin dir. TODO: driver binaries
    // (e.g. tamias-driver-tamias, publish=false today) could ride under
    // `drivers/` later — see tamias-driver's bundled-driver discovery.
    // `description` is free text (the cli crate's has a colon) — quote
    // it so the manifest stays valid YAML.
    let desc = description.replace('\\', "\\\\").replace('"', "\\\"");
    let manifest =
        format!("name: tamias\nversion: {version}\ndescription: \"{desc}\"\ntype: files\n");
    std::fs::write(stage.join("tamias.yaml"), manifest)
        .map_err(|e| format!("write manifest: {e}"))?;
    let dest = bin_dir.join("tamias");
    std::fs::copy(bin_src, &dest).map_err(|e| format!("copy {}: {e}", bin_src.display()))?;
    // fs::copy keeps the mode on unix; pin the exec bit anyway — the
    // payload's whole point is a runnable binary (pack/untar preserve
    // modes end to end).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", dest.display()))?;
    }
    Ok(())
}

/// ADR-0003 self-distribution: release-build the `tamias` binary, stage
/// it as a `type: files` package (`bin/tamias`), and pack it through
/// `tamias-publish` — the same path `tamias bundle pack`/`publish` use.
fn pack_self(rest: &[String]) -> i32 {
    let args = match parse_pack_self(rest) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let out_dir = args
        .out
        .unwrap_or_else(|| PathBuf::from("target/pack-self"));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("pack-self: create {}: {e}", out_dir.display());
        return 1;
    }
    let (version, description) = match args.version {
        Some(v) => (v, "tamias".to_string()),
        None => match package_fields(&cli_manifest()) {
            Ok(fields) => fields,
            Err(e) => {
                eprintln!("pack-self: {e}");
                return 1;
            }
        },
    };
    // Binary: explicit --bin wins; otherwise build unless --skip-build.
    let bin = match &args.bin {
        Some(b) => b.clone(),
        None => {
            let rel = if cfg!(windows) {
                "target/release/tamias.exe"
            } else {
                "target/release/tamias"
            };
            if !args.skip_build
                && run(&["cargo", "build", "--release", "-p", "tamias"]).code() != Some(0)
            {
                return 1;
            }
            PathBuf::from(rel)
        }
    };
    if !bin.is_file() {
        eprintln!(
            "pack-self: no binary at {} — drop --skip-build or pass --bin <path>",
            bin.display()
        );
        return 1;
    }
    let stage = out_dir.join("stage");
    if let Err(e) = std::fs::remove_dir_all(&stage) {
        if stage.exists() {
            eprintln!("pack-self: clear {}: {e}", stage.display());
            return 1;
        }
    }
    if let Err(e) = stage_package(&stage, &bin, &version, &description) {
        eprintln!("pack-self: {e}");
        return 1;
    }
    // No `include:` in the manifest → `Includes` never engages; Keep is
    // the honest choice (ship the raw payload). store_root is unused.
    let tgz = out_dir.join(format!("tamias-{version}.tgz"));
    let store_root = out_dir.join("store");
    let packed = match tamias_publish::pack(
        &stage,
        Some(&tgz),
        tamias_publish::Includes::Keep,
        &store_root,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pack-self: pack failed: {e}");
            return 1;
        }
    };
    let tgz_digest =
        tamias_core::file_digest(&packed).unwrap_or_else(|e| format!("sha256:<unreadable: {e}>"));
    let payload_digest =
        tamias_core::dir_digest(&stage).unwrap_or_else(|e| format!("sha256:<unreadable: {e}>"));
    let name = packed
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tamias.tgz".to_string());
    println!("packed: {}", packed.display());
    println!("tgz sha256:      {tgz_digest}");
    println!("payload digest:  {payload_digest}");
    println!();
    println!("install (pinned tarball):");
    println!("  tamias install \"https://tamias.9aia.com/{name}#{tgz_digest}\"");
    println!("  tamias bin tamias   # link bin/tamias into the bin dir");
    println!();
    println!("or publish to a registry:");
    println!("  cp {} <registry-payload-dir>/{}", packed.display(), name);
    println!("  tamias publish --registry <name>");
    0
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
        "pack-self" => pack_self(&rest),
        "smoke" => smoke(),
        "site" => site(),
        other => {
            eprintln!(
                "usage: cargo xtask <check|test|coverage|install|package|pack-self|smoke|site>\n  unknown verb: {other:?}"
            );
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn package_fields_reads_version_and_description() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"x\"\nversion = \"1.2.3\"\ndescription = \"desc here\"\n\n[dependencies]\nversion = \"9.9\"\n",
        )
        .unwrap();
        let (v, d) = package_fields(&manifest).unwrap();
        assert_eq!(v, "1.2.3");
        assert_eq!(d, "desc here");
    }

    #[test]
    fn package_fields_rejects_workspace_version() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"x\"\nversion.workspace = true\n",
        )
        .unwrap();
        assert!(package_fields(&manifest).is_err());
    }

    #[test]
    fn stage_package_writes_manifest_and_bin() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        let bin = dir.path().join("fake-tamias");
        std::fs::write(&bin, "#!/bin/sh\necho hi\n").unwrap();
        stage_package(&stage, &bin, "0.1.4", "test desc").unwrap();
        let manifest = std::fs::read_to_string(stage.join("tamias.yaml")).unwrap();
        assert!(manifest.contains("name: tamias"));
        assert!(manifest.contains("version: 0.1.4"));
        assert!(manifest.contains("type: files"));
        assert!(stage.join("bin/tamias").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(stage.join("bin/tamias"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }
}
