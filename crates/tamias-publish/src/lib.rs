#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-publish — author-side packing: a payload dir + manifest → a
//! `.tgz` (the registry stores these; the http source fetches them).

use std::path::{Path, PathBuf};

use tamias_core::{pack_tgz, Error, Manifest};

/// Pack `dir` into a `.tgz` — `<name>-<version>.tgz` by default.
///
/// # Errors
/// Fails when `dir` has no manifest or on I/O error.
pub fn pack(dir: &Path, out: Option<&Path>) -> Result<PathBuf, Error> {
    let manifest = Manifest::load(dir)?;
    let default_name = match &manifest.version {
        Some(v) => format!("{}-{v}.tgz", manifest.name),
        None => format!("{}.tgz", manifest.name),
    };
    let out = out.map_or_else(|| PathBuf::from(&default_name), Path::to_path_buf);
    pack_tgz(dir, &out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Serializes tests that mutate process env/cwd — `cargo test`
    /// runs tests on threads in one process, so mutations must be
    /// serialized *and* restored even on panic.
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
    fn packs_a_dir() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.2.0\n").unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.tgz");
        let packed = pack(src.path(), Some(&out)).unwrap();
        assert!(packed.is_file());
    }

    #[test]
    fn pack_without_version_uses_name() {
        let _g = EnvGuard::new();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: nv\n").unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let out = tempfile::tempdir().unwrap();
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(out.path()).unwrap();
        let packed = pack(src.path(), None).unwrap();
        std::env::set_current_dir(old).unwrap();
        assert_eq!(packed.file_name().unwrap().to_str().unwrap(), "nv.tgz");
    }
}
