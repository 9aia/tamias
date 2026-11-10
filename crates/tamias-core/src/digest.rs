//! SHA-256 helpers + payload walking — the content addressing the store
//! and `verify` use.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::records::RECORD_FILE;

/// sha256 hex (no prefix) of a byte slice.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// `sha256:<hex>` of a byte slice — the recorded payload identity.
#[must_use]
pub fn content_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// `sha256:<hex>` → its first 12 chars — the display shortening the
/// CLI/TUI surfaces share (`sha256:abcdef…` → `abcdef123456`).
#[must_use]
pub fn short_digest(digest: &str) -> &str {
    let s = digest.strip_prefix("sha256:").unwrap_or(digest);
    s.get(..12).unwrap_or(s)
}

/// `sha256:<hex>` of a file's contents.
///
/// # Errors
/// Propagates I/O failures reading the file.
pub fn file_digest(path: &Path) -> Result<String, Error> {
    let mut hasher = Sha256::new();
    hash_file_into(&mut hasher, path)?;
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

/// Stream `path` through `hasher` in fixed-size chunks — memory stays
/// flat no matter how large the file is (cache hits re-digest on every
/// read, so `fs::read` here would make peak memory = largest blob).
fn hash_file_into(hasher: &mut Sha256, path: &Path) -> Result<(), Error> {
    let mut file = fs::File::open(path).map_err(Error::io(format!("read {}", path.display())))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(Error::io(format!("read {}", path.display())))?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(&buf[..n]);
    }
}

/// Sorted relative paths of payload files under `dir` — `.git/` dirs and
/// the root [`RECORD_FILE`] never count. Symlinks are never payload:
/// `DirEntry::file_type` does not follow them, so links are neither
/// recursed into nor hashed — a `loot -> /` or `a -> .` in a fetched
/// tree can't exfiltrate or loop the walk.
///
/// # Errors
/// Propagates I/O failures while walking the tree.
pub fn payload_files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries = fs::read_dir(dir).map_err(Error::io(format!("read {}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(Error::io(format!("read {}", dir.display())))?;
        let file_type = entry
            .file_type()
            .map_err(Error::io(format!("stat {}", entry.path().display())))?;
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
        if file_type.is_dir() {
            if rel == Path::new(".git") {
                continue;
            }
            walk(&path, base, out)?;
        } else if file_type.is_file() && rel != Path::new(RECORD_FILE) {
            out.push(rel);
        }
    }
    Ok(())
}

/// `sha256:<hex>` over the sorted `relpath\0bytes` of every payload file —
/// order-independent, so reinstalls of identical trees match.
///
/// # Errors
/// Propagates I/O failures while reading the payload.
pub fn dir_digest(dir: &Path) -> Result<String, Error> {
    let mut hasher = Sha256::new();
    for rel in payload_files(dir)? {
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0]);
        hash_file_into(&mut hasher, &dir.join(&rel))?;
        hasher.update([0]);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

/// Total payload bytes — `info` size reporting.
///
/// # Errors
/// Propagates I/O failures while stat-ing the payload.
pub fn dir_size(dir: &Path) -> Result<u64, Error> {
    let mut total = 0;
    for rel in payload_files(dir)? {
        let ctx = format!("stat {}", dir.join(&rel).display());
        total += fs::metadata(dir.join(&rel)).map_err(Error::io(ctx))?.len();
    }
    Ok(total)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            content_digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn dir_digest_order_independent_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"b").unwrap();
        let d1 = dir_digest(dir.path()).unwrap();
        let d2 = dir_digest(dir.path()).unwrap();
        assert_eq!(d1, d2);
        assert!(d1.starts_with("sha256:"));
        assert_eq!(dir_size(dir.path()).unwrap(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_payload() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"loot").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("loot")).unwrap();
        std::os::unix::fs::symlink(".", dir.path().join("self")).unwrap();
        std::os::unix::fs::symlink("a.txt", dir.path().join("alias")).unwrap();
        // completes — no recursion through `self`, no reads through `loot`
        let files = payload_files(dir.path()).unwrap();
        assert_eq!(files, vec![PathBuf::from("a.txt")]);
        assert!(dir_digest(dir.path()).is_ok());
        assert_eq!(dir_size(dir.path()).unwrap(), 1);
    }

    #[test]
    fn io_error_arms() {
        let missing = Path::new("/nonexistent-xyz-123");
        assert!(file_digest(missing).is_err());
        assert!(payload_files(missing).is_err());
        assert!(dir_digest(missing).is_err());
        assert!(dir_size(missing).is_err());
    }
}
