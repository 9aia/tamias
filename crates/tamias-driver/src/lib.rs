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
//! `verb ∈ {install, deploy, uninstall}`; a driver may also answer
//! `capabilities`. Installing a product drops its driver into
//! `<tamias-home>/drivers/` (from the payload's `drivers/` dir), so tamias
//! finds it. The engine never links a product crate.

use std::path::{Path, PathBuf};

use tamias_core::Error;

/// `<tamias-home>` — `$TAMIAS_HOME` → `$XDG_DATA_HOME/tamias` →
/// `~/.local/share/tamias` → `./tamias`.
#[must_use]
pub fn tamias_home() -> PathBuf {
    std::env::var_os("TAMIAS_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("tamias")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/tamias")))
        .unwrap_or_else(|| PathBuf::from("./tamias"))
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
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let candidate = drivers_dir().join(name);
    candidate.is_file().then_some(candidate)
}

/// Whether `driver` reports handling `kind` (its `capabilities` `types`).
fn driver_handles(driver: &Path, kind: &str) -> bool {
    let Ok(out) = std::process::Command::new(driver)
        .arg("capabilities")
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return false;
    };
    value
        .get("types")
        .and_then(|t| t.as_array())
        .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(kind)))
}

/// Every discovered driver as `(type, path)` — for `doctor`.
#[must_use]
pub fn discovered_drivers() -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut scan = |dir: &Path| {
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if let Some(kind) = name.strip_prefix("tamias-driver-") {
                        if path.is_file() && !found.iter().any(|(k, _)| k == kind) {
                            found.push((kind.to_string(), path));
                        }
                    }
                }
            }
        }
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            scan(&dir);
        }
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
    let Some(driver) = find_driver(kind) else {
        return Ok(false);
    };
    let mut cmd = std::process::Command::new(&driver);
    cmd.arg(verb).arg("--dir").arg(dir).arg("--name").arg(name);
    if let Some(config) = config {
        cmd.arg("--config").arg(config);
    }
    let out = cmd
        .output()
        .map_err(Error::io(format!("run driver {}", driver.display())))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "driver {} {verb} failed: {}",
            driver.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(true)
}

/// Copy any bundled drivers a payload ships (`<payload>/drivers/*`) into
/// `<tamias-home>/drivers/` so tamias discovers them. Returns action lines.
///
/// # Errors
/// Propagates read/copy failures.
pub fn register_bundled_drivers(payload_root: &Path) -> Result<Vec<String>, Error> {
    let src = payload_root.join("drivers");
    if !src.is_dir() {
        return Ok(Vec::new());
    }
    let dest = drivers_dir();
    std::fs::create_dir_all(&dest).map_err(Error::io(format!("create {}", dest.display())))?;
    let mut actions = Vec::new();
    for entry in std::fs::read_dir(&src).map_err(Error::io(format!("read {}", src.display())))? {
        let path = entry.map_err(Error::io("read driver entry"))?.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap_or_default();
        let to = dest.join(name);
        std::fs::copy(&path, &to).map_err(Error::io(format!("copy {}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755));
        }
        actions.push(format!("driver: {}", to.display()));
    }
    Ok(actions)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn dispatch_none_and_driver() {
        let _guard = ENV.lock().unwrap();
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
        let _guard = ENV.lock().unwrap();
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
    fn home_defaults() {
        let _guard = ENV.lock().unwrap();
        std::env::remove_var("TAMIAS_HOME");
        let _ = tamias_home();
        assert!(drivers_dir().ends_with("drivers"));
    }
}
