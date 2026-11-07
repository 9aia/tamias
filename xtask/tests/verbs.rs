//! xtask verbs are thin process plumbing — run the real binary
//! against PATH stubs so `check`/`test`/`coverage`/`install`/`site`
//! all execute their dispatch, args, and exit-code paths without
//! touching a real toolchain. Stubs are sh scripts (unix repo).

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A throwaway dir with a `bin/` for stubs (and optionally a
/// `docs/` for the site verb).
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-{name}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    dir
}

fn stub(dir: &Path, name: &str, body: &str) {
    let p = dir.join("bin").join(name);
    std::fs::write(&p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The `cargo` stub: `--version` probes for nextest/llvm-cov honor
/// HAVE_NEXTEST/HAVE_LLVM_COV; everything else exits CARGO_EXIT.
const CARGO_STUB: &str = r#"#!/bin/sh
if [ "$2" = "--version" ]; then
  case "$1" in
    nextest) [ "${HAVE_NEXTEST:-0}" = 1 ] || exit 1 ;;
    llvm-cov) [ "${HAVE_LLVM_COV:-0}" = 1 ] || exit 1 ;;
  esac
fi
exit "${CARGO_EXIT:-0}"
"#;

const OK_STUB: &str = "#!/bin/sh\nexit 0\n";

fn xtask(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tamias-xtask"));
    c.args(args)
        .env("PATH", dir.join("bin"))
        .envs(envs.iter().copied())
        .current_dir(dir);
    c.output().unwrap()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

#[test]
fn check_runs_fmt_and_clippy() {
    let dir = scratch("check");
    stub(&dir, "cargo", CARGO_STUB);
    assert_eq!(code(&xtask(&dir, &["check"], &[])), 0);
    let o = xtask(&dir, &["check"], &[("CARGO_EXIT", "1")]);
    assert_eq!(code(&o), 1);
    std::fs::remove_file(dir.join("bin/cargo")).unwrap();
    let o = xtask(&dir, &["check"], &[]);
    assert_eq!(code(&o), 1);
}

#[test]
fn test_prefers_nextest_then_falls_back() {
    let dir = scratch("test");
    stub(&dir, "cargo", CARGO_STUB);
    assert_eq!(code(&xtask(&dir, &["test"], &[("HAVE_NEXTEST", "1")])), 0);
    assert_eq!(code(&xtask(&dir, &["test"], &[])), 0);
    assert_eq!(code(&xtask(&dir, &["test"], &[("CARGO_EXIT", "1")])), 1);
}

#[test]
fn coverage_needs_llvm_cov() {
    let dir = scratch("coverage");
    stub(&dir, "cargo", CARGO_STUB);
    let o = xtask(&dir, &["coverage"], &[]);
    assert_eq!(code(&o), 1);
    let envs = [("HAVE_LLVM_COV", "1"), ("HAVE_NEXTEST", "1")];
    assert_eq!(code(&xtask(&dir, &["coverage"], &envs)), 0);
    assert_eq!(
        code(&xtask(
            &dir,
            &["coverage", "--fail-under-lines", "100"],
            &envs
        )),
        0
    );
    assert_eq!(
        code(&xtask(&dir, &["coverage"], &[("HAVE_LLVM_COV", "1")])),
        0
    );
}

#[test]
fn install_delegates_to_cargo_install() {
    let dir = scratch("install");
    stub(&dir, "cargo", CARGO_STUB);
    assert_eq!(code(&xtask(&dir, &["install"], &[])), 0);
    assert_eq!(code(&xtask(&dir, &["install"], &[("CARGO_EXIT", "1")])), 1);
}

#[test]
fn site_needs_the_docs_worktree() {
    let dir = scratch("site");
    stub(&dir, "cargo", CARGO_STUB);
    let o = xtask(&dir, &["site"], &[]);
    assert_eq!(code(&o), 1);
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/package.json"), "{}").unwrap();
    stub(&dir, "bun", OK_STUB);
    stub(&dir, "npm", OK_STUB);
    assert_eq!(code(&xtask(&dir, &["site"], &[])), 0);
    std::fs::remove_file(dir.join("bin/bun")).unwrap();
    assert_eq!(code(&xtask(&dir, &["site"], &[])), 0);
    std::fs::remove_file(dir.join("bin/npm")).unwrap();
    assert_eq!(code(&xtask(&dir, &["site"], &[])), 1);
}

#[test]
fn unknown_and_empty_verbs_print_usage() {
    let dir = scratch("usage");
    let o = xtask(&dir, &["bogus"], &[]);
    assert_eq!(code(&o), 2);
    let o = xtask(&dir, &[], &[]);
    assert_eq!(code(&o), 2);
}
