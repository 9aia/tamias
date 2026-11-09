//! Atomic file writes — a same-dir tmp file, fsync, then `rename(2)`.
//! A crash can leave a stray `*.tmp` sibling but never a torn file at
//! `path`. Mirrors `myco-store-md`'s `write_atomic`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Error;

/// Sibling tmp path for `path` — same dir, `<name>.<pid>.tmp`
/// extension swap (`index.json` → `index.<pid>.tmp`,
/// `0.2.0.tgz` → `0.2.0.<pid>.tmp`). Same filesystem as `path`, so the
/// final `rename` is atomic.
#[must_use]
pub fn tmp_path(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.tmp", std::process::id()))
}

/// Write `contents` to `path` atomically: tmp sibling + fsync +
/// rename. Parent dirs are created; a failed write cleans its tmp.
///
/// # Errors
/// Propagates create/write/sync/rename failures.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), Error> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(Error::io(format!("create {}", parent.display())))?;
    let tmp = tmp_path(path);
    let write = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if write.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write.map_err(Error::io(format!("write {}", path.display())))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn write_atomic_creates_dirs_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/deep/f.json");
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
        // no tmp litter
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn write_atomic_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_cleans_tmp_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("ro");
        fs::create_dir_all(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
        let target = sub.join("f.json");
        assert!(write_atomic(&target, b"hi").is_err());
        // tmp file cleaned up
        let left: Vec<_> = fs::read_dir(&sub).unwrap().collect();
        assert!(left.is_empty());
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn tmp_path_is_sibling_with_pid_extension() {
        let p = Path::new("/x/index.json");
        let tmp = tmp_path(p);
        assert_eq!(tmp.parent(), Some(Path::new("/x")));
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("index."), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
        // extension swap keeps multi-dot names intact
        let tgz = tmp_path(Path::new("/x/a/0.2.0.tgz"));
        assert!(tgz
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("0.2.0."));
    }
}
