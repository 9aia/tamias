#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-driver — the content **driver** host: discovery + dispatch of
//! `tamias-driver-<type>` executables, with **no product knowledge** in
//! tamias.
//!
//! A **driver** is an executable `tamias-driver-<type>` on `PATH` or in
//! `<tamias-home>/drivers/`. tamias reads a package's `type:`, resolves its
//! driver, and runs:
//!
//! ```text
//! tamias-driver-<type> <verb> --dir <dir> --name <name> [--config <file>]
//! ```
//!
//! `verb ∈ {install, deploy, uninstall}` for the lifecycle dispatch;
//! drivers may declare further provider verbs (`health`, `hooks`, …)
//! in `capabilities.verbs`, reached through [`dispatch_with_args`] —
//! `tamias driver exec`. A driver may also answer `capabilities`.
//!
//! **`config-schema` — a reserved verb by convention.** A driver that
//! consumes a `--config` file *may* implement `config-schema`: dispatch
//! it and the driver prints a JSON Schema (draft 2020-12) on stdout
//! describing that file's format, exit 0. Drivers without one exit
//! non-zero with a clear stderr message — callers treat that as
//! "unsupported" and surface it, never a crash. The standard
//! `--dir`/`--name`/`--config` flags are appended like any verb, but
//! the schema should be instance-agnostic. Fetch it with
//! [`config_schema`] or `tamias driver config-schema <pkg>`.
//!
//! Validating a config *against* the schema is deliberately
//! driver-side, not engine-side: tamias ships no JSON Schema validator,
//! so a driver that wants validation implements it as an ordinary
//! provider verb — `tamias driver exec <pkg> config validate
//! --config <file>` is the intended shape.
//! Installing a product drops its driver into
//! `<tamias-home>/drivers/` (from the payload's `drivers/` dir), so tamias
//! finds it. The engine never links a product crate.
//!
//! **Bundled drivers.** A payload's `drivers/` dir ships ready-to-run
//! driver files:
//!
//! - `drivers/<name>` — portable drivers (interpreted scripts like
//!   `#!/usr/bin/env python3` ship as source).
//! - `drivers/bin/<target-triple>/<name>` — compiled drivers, one dir per
//!   rustc target triple (`x86_64-unknown-linux-gnu`, …). At install time
//!   only the dir matching [`target_triple`] is copied; a package that
//!   ships platform dirs but none for this host fails the install with a
//!   clear error rather than silently dropping the driver.
//!
//! Both forms land flat in `<tamias-home>/drivers/` (executable bit set),
//! stay inside the payload digest, and are consent-gated — see
//! [`drivers_allowed`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use tamias_core::{output_with_timeout, Error};

/// `(stdout, stderr)` captured from a dispatched driver verb —
/// [`dispatch_with_args`] returns it for the caller to render.
pub type DriverOutput = (Vec<u8>, Vec<u8>);

/// A driver verb (`install`/`deploy`/`uninstall`) can legitimately take a
/// while — a hung one must not wedge the install or the single-threaded
/// `tamias serve` loop.
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(120);

/// `capabilities` is a discovery probe — seconds, not minutes.
const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);

/// The rustc target triple this build was compiled for
/// (`x86_64-unknown-linux-gnu`) — captured at compile time by `build.rs`
/// (`cargo:rustc-env`), so it always names a real cargo target even where
/// `std::env::consts` can't reconstruct the vendor/os/env spelling.
///
/// This is the selector for a payload's `drivers/bin/<triple>/` dir —
/// see [`register_bundled_drivers`].
#[must_use]
pub fn target_triple() -> &'static str {
    env!("TAMIAS_TARGET_TRIPLE")
}

/// `<tamias-home>` — `$TAMIAS_HOME` → `$XDG_DATA_HOME/tamias` →
/// `~/.local/share/tamias` → `./.tamias`.
///
/// The no-env fallback is `./.tamias` — the same project-local home the
/// CLI's `store_root()`/`cache_root()`/`registry_root()` fall back to
/// (`./.tamias/store`, `./.tamias/fetch`, `./.tamias/registry`), so the
/// drivers dir lands at `./.tamias/drivers` beside them. (`./tamias`
/// would collide with a `tamias` source checkout or package dir.)
#[must_use]
pub fn tamias_home() -> PathBuf {
    std::env::var_os("TAMIAS_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("tamias")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/tamias")))
        .unwrap_or_else(|| PathBuf::from("./.tamias"))
}

/// The `<tamias-home>/drivers` dir.
#[must_use]
pub fn drivers_dir() -> PathBuf {
    tamias_home().join("drivers")
}

/// Find the driver for a content type — `tamias-driver-<kind>` on `PATH`,
/// then `<tamias-home>/drivers/`.
#[must_use]
pub fn find_driver(kind: &str) -> Option<PathBuf> {
    let name = format!("tamias-driver-{kind}");
    if let Some(path) = lookup(&name) {
        return Some(path);
    }
    // Otherwise map via capabilities: the first driver whose `types` include `kind`.
    discovered_drivers()
        .into_iter()
        .find(|(_, path)| driver_handles(path, kind))
        .map(|(_, path)| path)
}

/// `<name>` on `PATH`, then `<tamias-home>/drivers/`.
fn lookup(name: &str) -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    });
    on_path.or_else(|| {
        let candidate = drivers_dir().join(name);
        candidate.is_file().then_some(candidate)
    })
}

/// Whether `driver` reports handling `kind` (its `capabilities` `types`).
fn driver_handles(driver: &Path, kind: &str) -> bool {
    capabilities(driver)
        .ok()
        .and_then(|value| value.get("types").cloned())
        .and_then(|t| t.as_array().cloned())
        .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(kind)))
}

/// Run a driver's `capabilities` probe and return the JSON it prints —
/// the same bounded call [`driver_handles`] makes, exposed for the
/// `tamias drivers` listing. `Ok` only on a zero exit *and* parseable
/// JSON; anything else is an [`Error`] the caller renders as "unreachable".
///
/// # Errors
/// Propagates spawn/timeout/parse failures.
pub fn capabilities(driver: &Path) -> Result<serde_json::Value, Error> {
    let mut cmd = std::process::Command::new(driver);
    cmd.arg("capabilities");
    let out = output_with_timeout(&mut cmd, CAPABILITIES_TIMEOUT)
        .map_err(|e| Error::Failed(format!("{} capabilities: {e}", driver.display())))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "{} capabilities exited {}",
            driver.display(),
            out.status
        )));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| Error::Failed(format!("{} capabilities: bad JSON: {e}", driver.display())))
}

/// Every discovered driver as `(type, path)` — for `doctor` and
/// `tamias drivers`.
#[must_use]
pub fn discovered_drivers() -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut scan = |dir: &Path| {
        let Some(read) = std::fs::read_dir(dir).ok() else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let kind = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("tamias-driver-"));
            // `file_type` never follows symlinks — a linked driver isn't
            // dispatchable (mirrors register_bundled_drivers).
            let is_file = entry.file_type().is_ok_and(|t| t.is_file());
            if let Some(kind) = kind {
                if is_file && !found.iter().any(|(k, _)| k == kind) {
                    found.push((kind.to_string(), path));
                }
            }
        }
    };
    for dir in std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        scan(&dir);
    }
    scan(&drivers_dir());
    found.sort();
    found
}

/// Dispatch a verb to the content type's driver. Returns whether a driver
/// ran (`false` = nothing to do — a `files` package, or no driver present).
///
/// # Errors
/// Fails when a driver is found but exits non-zero.
pub fn dispatch(
    kind: &str,
    verb: &str,
    dir: &Path,
    name: &str,
    config: Option<&Path>,
) -> Result<bool, Error> {
    dispatch_with_timeout(kind, verb, dir, name, config, &[], DISPATCH_TIMEOUT)
        .map(|ran| ran.is_some())
}

/// [`dispatch`] plus arbitrary trailing argv — the `tamias driver exec`
/// passthrough for provider verbs beyond the lifecycle three
/// (`hooks register`, `health`, …). `args` land after the standard
/// `--dir`/`--name`/`--config` flags. On success the driver's captured
/// stdout/stderr come back for the caller to render — provider verbs
/// answer with status JSON the operator needs to see. `Ok(None)` = no
/// driver found (same `false` as [`dispatch`]).
///
/// # Errors
/// Fails when a driver is found but exits non-zero.
pub fn dispatch_with_args(
    kind: &str,
    verb: &str,
    dir: &Path,
    name: &str,
    config: Option<&Path>,
    args: &[String],
) -> Result<Option<DriverOutput>, Error> {
    dispatch_with_timeout(kind, verb, dir, name, config, args, DISPATCH_TIMEOUT)
}

/// Fetch a kind driver's `config-schema` — the reserved verb that
/// prints a JSON Schema (draft 2020-12) describing the `--config` file
/// the driver consumes. Bounded like `capabilities` (a probe, not a
/// lifecycle verb). `Ok(None)` = no driver found; a driver that doesn't
/// implement the verb exits non-zero and surfaces as [`Error`] with its
/// stderr — "unsupported" is a normal outcome, not a crash.
///
/// # Errors
/// Fails when a driver is found but exits non-zero or prints non-JSON.
pub fn config_schema(
    kind: &str,
    dir: &Path,
    name: &str,
    config: Option<&Path>,
) -> Result<Option<serde_json::Value>, Error> {
    let Some((stdout, _)) = dispatch_with_timeout(
        kind,
        "config-schema",
        dir,
        name,
        config,
        &[],
        CAPABILITIES_TIMEOUT,
    )?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&stdout)
        .map(Some)
        .map_err(|e| Error::Failed(format!("config-schema: bad JSON: {e}")))
}

/// [`dispatch`] with an explicit bound — tests shrink it; production
/// callers get [`DISPATCH_TIMEOUT`]. `args` is the trailing argv
/// [`dispatch_with_args`] appends.
fn dispatch_with_timeout(
    kind: &str,
    verb: &str,
    dir: &Path,
    name: &str,
    config: Option<&Path>,
    args: &[String],
    timeout: Duration,
) -> Result<Option<DriverOutput>, Error> {
    let Some(driver) = find_driver(kind) else {
        return Ok(None);
    };
    let mut cmd = std::process::Command::new(&driver);
    cmd.arg(verb).arg("--dir").arg(dir).arg("--name").arg(name);
    if let Some(config) = config {
        cmd.arg("--config").arg(config);
    }
    cmd.args(args);
    let out = output_with_timeout(&mut cmd, timeout)
        .map_err(|e| Error::Failed(format!("run driver {}: {e}", driver.display())))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "driver {} {verb} failed: {}",
            driver.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(Some((out.stdout, out.stderr)))
}

/// Consent for registering + running payload-shipped drivers: the caller's
/// `--allow-drivers` flag, or `TAMIAS_ALLOW_DRIVERS=1` in the environment.
#[must_use]
pub fn drivers_allowed(flag: bool) -> bool {
    flag || std::env::var("TAMIAS_ALLOW_DRIVERS").is_ok_and(|v| v == "1")
}

/// Names of the driver files a payload ships under `drivers/` — regular
/// files only, mirroring `register_bundled_drivers`'s filter, plus the
/// platform bins under `drivers/bin/<triple>/` (reported as
/// `bin/<triple>/<name>`). Lets callers detect (and gate) bundled drivers
/// *before* copying or dispatching them — including binaries for other
/// platforms, which are still bundled executable content.
///
/// # Errors
/// Propagates read failures.
pub fn bundled_drivers(payload_root: &Path) -> Result<Vec<String>, Error> {
    let src = payload_root.join("drivers");
    if !src.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&src).map_err(Error::io(format!("read {}", src.display())))? {
        let entry = entry.map_err(Error::io("read driver entry"))?;
        let file_type = entry
            .file_type()
            .map_err(Error::io(format!("stat {}", entry.path().display())))?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        // `file_type` never follows symlinks — a linked `bin/` dir can't
        // smuggle outside paths into the listing (mirrors registration).
        if file_type.is_file() {
            names.push(name.to_string());
        } else if name == "bin" && file_type.is_dir() {
            // `drivers/bin/<triple>/<name>` — compiled driver payloads.
            names.extend(platform_bin_names(&entry.path())?);
        }
    }
    names.sort();
    Ok(names)
}

/// `bin/<triple>/<name>` labels for every regular file one level under
/// each `drivers/bin/<triple>/` dir — all platforms, not just this host's.
fn platform_bin_names(bin: &Path) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    let Ok(triples) = std::fs::read_dir(bin) else {
        return Ok(names);
    };
    for triple in triples {
        let triple = triple.map_err(Error::io("read driver bin entry"))?;
        if !triple
            .file_type()
            .map_err(Error::io(format!("stat {}", triple.path().display())))?
            .is_dir()
        {
            continue;
        }
        let Some(triple_name) = triple.file_name().to_str().map(str::to_string) else {
            continue;
        };
        for file in std::fs::read_dir(triple.path())
            .map_err(Error::io(format!("read {}", triple.path().display())))?
        {
            let file = file.map_err(Error::io("read driver bin file"))?;
            let is_file = file
                .file_type()
                .map_err(Error::io(format!("stat {}", file.path().display())))?
                .is_file();
            if is_file {
                if let Some(name) = file.file_name().to_str() {
                    names.push(format!("bin/{triple_name}/{name}"));
                }
            }
        }
    }
    Ok(names)
}

/// Copy any bundled drivers a payload ships into `<tamias-home>/drivers/`
/// so tamias discovers them: top-level `drivers/<name>` files as-is, plus
/// the `drivers/bin/<target-triple>/` dir matching this host (see
/// [`target_triple`]). Platform bins for other triples are skipped; a
/// payload that ships platform dirs but none for this host is an error —
/// the package intended a runnable compiled driver and can't provide one.
/// Returns action lines.
///
/// # Errors
/// Propagates read/copy failures, and the no-matching-platform error.
pub fn register_bundled_drivers(payload_root: &Path) -> Result<Vec<String>, Error> {
    let src = payload_root.join("drivers");
    if !src.is_dir() {
        return Ok(Vec::new());
    }
    let dest = drivers_dir();
    std::fs::create_dir_all(&dest).map_err(Error::io(format!("create {}", dest.display())))?;
    let mut actions = Vec::new();
    let mut bin = None;
    for entry in std::fs::read_dir(&src).map_err(Error::io(format!("read {}", src.display())))? {
        let entry = entry.map_err(Error::io("read driver entry"))?;
        // `file_type` never follows symlinks — a payload can't install a
        // linked "driver" that resolves outside the payload.
        let file_type = entry
            .file_type()
            .map_err(Error::io(format!("stat {}", entry.path().display())))?;
        if file_type.is_dir() && entry.file_name() == "bin" {
            bin = Some(entry.path());
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().unwrap_or_default();
        let to = dest.join(name);
        std::fs::copy(&path, &to).map_err(Error::io(format!("copy {}", path.display())))?;
        set_executable(&to);
        actions.push(format!("driver: {}", to.display()));
    }
    if let Some(bin) = bin {
        actions.extend(register_platform_bins(&bin, &dest)?);
    }
    Ok(actions)
}

/// The `drivers/bin/<target-triple>/` payload: copy the dir matching this
/// host into `dest` (flat, executable). Errors when platform dirs exist
/// but none match — listing what the package does ship.
fn register_platform_bins(bin: &Path, dest: &Path) -> Result<Vec<String>, Error> {
    let mut triples = Vec::new();
    for entry in std::fs::read_dir(bin).map_err(Error::io(format!("read {}", bin.display())))? {
        let entry = entry.map_err(Error::io("read driver bin entry"))?;
        if entry
            .file_type()
            .map_err(Error::io(format!("stat {}", entry.path().display())))?
            .is_dir()
        {
            if let Some(name) = entry.file_name().to_str() {
                triples.push(name.to_string());
            }
        }
    }
    triples.sort();
    if triples.is_empty() {
        return Ok(Vec::new());
    }
    let current = target_triple();
    if !triples.iter().any(|t| t == current) {
        return Err(Error::Failed(format!(
            "package ships driver binaries for {} but not this platform ({current})",
            triples.join(", ")
        )));
    }
    let mut actions = Vec::new();
    let platform = bin.join(current);
    for entry in
        std::fs::read_dir(&platform).map_err(Error::io(format!("read {}", platform.display())))?
    {
        let entry = entry.map_err(Error::io("read driver bin file"))?;
        if !entry
            .file_type()
            .map_err(Error::io(format!("stat {}", entry.path().display())))?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().unwrap_or_default();
        let to = dest.join(name);
        std::fs::copy(&path, &to).map_err(Error::io(format!("copy {}", path.display())))?;
        set_executable(&to);
        actions.push(format!("driver: {}", to.display()));
    }
    Ok(actions)
}

/// Best-effort exec bit on unix — mirrors the previous inline block.
fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Serializes tests that mutate process env (`TAMIAS_HOME`,
    /// `TAMIAS_ALLOW_DRIVERS`, `PATH`). `cargo test` runs tests on
    /// threads in one process, so mutations must be serialized *and*
    /// restored even on panic.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard for [`ENV`]: restores the process environment and
    /// working directory on drop — panic-safe. Poison is ignored: an
    /// unwinding sibling already restored everything via its `Drop`.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        cwd: std::path::PathBuf,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
            let env = std::env::vars_os().collect();
            let cwd = std::env::current_dir().unwrap();
            Self {
                _lock: lock,
                env,
                cwd,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
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

    #[test]
    fn dispatch_none_and_driver() {
        let _guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        assert!(!dispatch("no-such-type", "install", dir.path(), "n", None).unwrap());
        std::env::set_var("TAMIAS_HOME", dir.path());
        std::fs::create_dir_all(drivers_dir()).unwrap();
        let driver = drivers_dir().join("tamias-driver-fx");
        std::fs::write(&driver, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(find_driver("fx").is_some());
        assert!(dispatch("fx", "install", dir.path(), "n", None).unwrap());
        assert!(discovered_drivers().iter().any(|(k, _)| k == "fx"));
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn register_bundled_drivers_copies() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        let payload = tmp.path().join("payload");
        std::fs::create_dir_all(payload.join("drivers")).unwrap();
        std::fs::write(payload.join("drivers/tamias-driver-x"), "#!/bin/sh\n").unwrap();
        assert_eq!(register_bundled_drivers(&payload).unwrap().len(), 1);
        assert!(drivers_dir().join("tamias-driver-x").is_file());
        let empty = tempfile::tempdir().unwrap();
        assert!(register_bundled_drivers(empty.path()).unwrap().is_empty());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn bundled_drivers_detects_and_consent() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("payload");
        std::fs::create_dir_all(payload.join("drivers")).unwrap();
        std::fs::write(payload.join("drivers/tamias-driver-x"), "#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(payload.join("drivers/sub")).unwrap();
        assert_eq!(bundled_drivers(&payload).unwrap(), ["tamias-driver-x"]);
        assert!(bundled_drivers(tmp.path()).unwrap().is_empty());
        // consent: flag or env
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        assert!(!drivers_allowed(false));
        assert!(drivers_allowed(true));
        std::env::set_var("TAMIAS_ALLOW_DRIVERS", "1");
        assert!(drivers_allowed(false));
        std::env::set_var("TAMIAS_ALLOW_DRIVERS", "0");
        assert!(!drivers_allowed(false));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
    }

    #[test]
    fn home_defaults() {
        let _guard = EnvGuard::new();
        std::env::remove_var("TAMIAS_HOME");
        let _ = tamias_home();
        assert!(drivers_dir().ends_with("drivers"));
    }

    #[test]
    fn home_fallback_is_dot_tamias() {
        let _guard = EnvGuard::new();
        // The no-env fallback must match the CLI's `./.tamias/*` roots —
        // `./tamias` would collide with a source checkout/package dir.
        let saved = ["TAMIAS_HOME", "XDG_DATA_HOME", "HOME"].map(|k| (k, std::env::var_os(k)));
        for (k, _) in &saved {
            std::env::remove_var(*k);
        }
        assert_eq!(tamias_home(), PathBuf::from("./.tamias"));
        assert_eq!(drivers_dir(), PathBuf::from("./.tamias/drivers"));
        for (k, v) in saved {
            if let Some(v) = v {
                std::env::set_var(k, v);
            }
        }
    }

    fn write_driver(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn config_schema_fetch_and_unsupported() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        let dd = drivers_dir();
        // No driver → Ok(None).
        assert!(config_schema("cs-none", tmp.path(), "n", None)
            .unwrap()
            .is_none());
        // Driver answers `config-schema` with JSON.
        write_driver(
            &dd,
            "tamias-driver-cs",
            "#!/bin/sh\nif [ \"$1\" = config-schema ]; then\n  echo '{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\"}'\n  exit 0\nfi\necho 'unsupported' >&2\nexit 1\n",
        );
        let schema = config_schema("cs", tmp.path(), "n", None).unwrap().unwrap();
        assert_eq!(schema["type"], "object");
        // Driver exits non-zero → Err carrying its stderr ("unsupported").
        write_driver(&dd, "tamias-driver-csbad", "#!/bin/sh\nexit 3\n");
        let err = config_schema("csbad", tmp.path(), "n", None).unwrap_err();
        assert!(err.to_string().contains("config-schema failed"), "{err}");
        // Driver prints non-JSON → Err.
        write_driver(&dd, "tamias-driver-csjunk", "#!/bin/sh\necho nope\n");
        assert!(config_schema("csjunk", tmp.path(), "n", None).is_err());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn capability_and_error_paths() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        let dd = drivers_dir();
        // capabilities exits 1 → not matched
        write_driver(&dd, "tamias-driver-failcap", "#!/bin/sh\nexit 1\n");
        assert!(find_driver("nomatch").is_none());
        // capabilities returns non-json → not matched
        write_driver(&dd, "tamias-driver-junk", "#!/bin/sh\necho nope\n");
        assert!(find_driver("nomatch2").is_none());
        // a capability match maps a type to a differently-named driver
        write_driver(
            &dd,
            "tamias-driver-ok",
            "#!/bin/sh\nif [ \"$1\" = capabilities ]; then echo '{\"types\":[\"thetype\"]}'; fi\n",
        );
        assert!(find_driver("thetype").is_some());
        // dispatch: driver exits 1 → Err
        write_driver(&dd, "tamias-driver-bad", "#!/bin/sh\nexit 1\n");
        assert!(dispatch("bad", "install", tmp.path(), "n", None).is_err());
        // the public capabilities probe: json on success, Err otherwise
        let caps = capabilities(&dd.join("tamias-driver-ok")).unwrap();
        assert_eq!(caps["types"], serde_json::json!(["thetype"]));
        assert!(capabilities(&dd.join("tamias-driver-junk")).is_err());
        assert!(capabilities(&dd.join("tamias-driver-failcap")).is_err());
        assert!(capabilities(&dd.join("tamias-driver-bad")).is_err());
        write_driver(&dd, "tamias-driver-boom", "#!/bin/sh\nexit 1\n");
        assert!(dispatch("boom", "install", tmp.path(), "n", None).is_err());
        assert!(discovered_drivers().iter().any(|(k, _)| k == "ok"));
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn find_via_path() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        let bin = tmp.path().join("bin");
        write_driver(&bin, "tamias-driver-onthepath", "#!/bin/sh\nexit 0\n");
        let old = std::env::var_os("PATH");
        std::env::set_var("PATH", &bin);
        assert!(find_driver("onthepath").is_some());
        if let Some(old) = old {
            std::env::set_var("PATH", old);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn hung_driver_is_killed() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        write_driver(
            &drivers_dir(),
            "tamias-driver-hang",
            "#!/bin/sh\nsleep 30\n",
        );
        let start = std::time::Instant::now();
        let err = dispatch_with_timeout(
            "hang",
            "install",
            tmp.path(),
            "n",
            None,
            &[],
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(10));
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn dispatch_with_args_passes_argv_and_captures_output() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        // Echo the trailing args + the standard flags so the test can
        // assert both landed, in order.
        write_driver(
            &drivers_dir(),
            "tamias-driver-ops",
            "#!/bin/sh\nprintf '%s\\n' \"$@\"\n",
        );
        let out = dispatch_with_args(
            "ops",
            "hooks",
            tmp.path(),
            "pkg",
            None,
            &["register".into(), "--dry-run".into()],
        )
        .unwrap()
        .unwrap();
        let stdout = String::from_utf8(out.0).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[0], "hooks");
        assert!(lines.contains(&"--dir"));
        assert!(lines.contains(&"--name"));
        assert!(lines.contains(&"register"));
        assert!(lines.contains(&"--dry-run"));
        // no driver → None, not an error
        assert!(
            dispatch_with_args("nope", "health", tmp.path(), "n", None, &[])
                .unwrap()
                .is_none()
        );
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn platform_bins_select_current_triple() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        let payload = tmp.path().join("payload");
        let current = target_triple();
        // a triple that can never be this host's (rustc has no such target)
        let foreign = "not-a-real-target-triple";
        assert_ne!(current, foreign);
        let cur = payload.join("drivers/bin").join(current);
        let oth = payload.join("drivers/bin").join(foreign);
        std::fs::create_dir_all(&cur).unwrap();
        std::fs::create_dir_all(&oth).unwrap();
        std::fs::write(cur.join("tamias-driver-cur"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(oth.join("tamias-driver-oth"), "#!/bin/sh\nexit 0\n").unwrap();
        // interpreted driver alongside the bins still registers
        std::fs::create_dir_all(payload.join("drivers")).unwrap();
        std::fs::write(payload.join("drivers/tamias-driver-script"), "#!/bin/sh\n").unwrap();
        let actions = register_bundled_drivers(&payload).unwrap();
        assert_eq!(actions.len(), 2);
        assert!(drivers_dir().join("tamias-driver-cur").is_file());
        assert!(drivers_dir().join("tamias-driver-script").is_file());
        // the foreign-platform binary is never copied
        assert!(!drivers_dir().join("tamias-driver-oth").exists());
        // exec bit set on the registered bin
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(drivers_dir().join("tamias-driver-cur"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        // consent detection sees both forms, labeled by platform
        let shipped = bundled_drivers(&payload).unwrap();
        assert!(shipped.contains(&"tamias-driver-script".to_string()));
        assert!(shipped.contains(&format!("bin/{current}/tamias-driver-cur")));
        assert!(shipped.contains(&format!("bin/{foreign}/tamias-driver-oth")));
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn platform_bins_missing_platform_errors() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        let payload = tmp.path().join("payload");
        let foreign = "aarch64-pc-windows-gnullvm";
        assert_ne!(target_triple(), foreign);
        let dir = payload.join("drivers/bin").join(foreign);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tamias-driver-x"), "bin").unwrap();
        let err = register_bundled_drivers(&payload).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(foreign), "{msg}");
        assert!(msg.contains(target_triple()), "{msg}");
        assert!(msg.contains("not this platform"), "{msg}");
        // nothing landed in the drivers dir
        assert!(!drivers_dir().join("tamias-driver-x").exists());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn platform_bins_empty_and_nested_dirs_ignored() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        let payload = tmp.path().join("payload");
        // drivers/bin exists but holds no platform dirs → no-op
        std::fs::create_dir_all(payload.join("drivers/bin")).unwrap();
        assert!(register_bundled_drivers(&payload).unwrap().is_empty());
        // a file directly under bin/ is not a platform dir → still no-op
        std::fs::write(payload.join("drivers/bin/loose"), "x").unwrap();
        assert!(register_bundled_drivers(&payload).unwrap().is_empty());
        // deeper nesting under a triple dir is ignored
        let cur = payload.join("drivers/bin").join(target_triple());
        std::fs::create_dir_all(cur.join("nested")).unwrap();
        std::fs::write(cur.join("nested/deep"), "x").unwrap();
        assert!(register_bundled_drivers(&payload).unwrap().is_empty());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn dispatch_with_config_and_non_file_entry() {
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("home"));
        let dd = drivers_dir();
        write_driver(&dd, "tamias-driver-cfg", "#!/bin/sh\nexit 0\n");
        let cfg = tmp.path().join("cfg.yaml");
        std::fs::write(&cfg, "x").unwrap();
        assert!(dispatch("cfg", "deploy", tmp.path(), "n", Some(&cfg)).unwrap());
        // a subdir under drivers/ is skipped when registering bundled drivers
        let payload = tmp.path().join("payload");
        std::fs::create_dir_all(payload.join("drivers/sub")).unwrap();
        std::fs::write(payload.join("drivers/d1"), "#!/bin/sh\n").unwrap();
        assert_eq!(register_bundled_drivers(&payload).unwrap().len(), 1);
        std::env::remove_var("TAMIAS_HOME");
    }
}
