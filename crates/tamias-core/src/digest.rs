//! SHA-256 helpers + payload walking — the content addressing the store
//! and `verify` use.

use std::fs;
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

/// `sha256:<hex>` of a file's contents.
///
/// # Errors
/// Propagates I/O failures reading the file.
pub fn file_digest(path: &Path) -> Result<String, Error> {
    let bytes = fs::read(path).map_err(Error::io(format!("read {}", path.display())))?;
    Ok(content_digest(&bytes))
}

/// Sorted relative paths of payload files under `dir` — `.git/` dirs and
/// the root [`RECORD_FILE`] never count.
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
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
        if path.is_dir() {
            if rel == Path::new(".git") {
                continue;
            }
            walk(&path, base, out)?;
        } else if rel != Path::new(RECORD_FILE) {
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
        let bytes = fs::read(dir.join(&rel))
            .map_err(Error::io(format!("read {}", dir.join(&rel).display())))?;
        hasher.update(bytes);
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
        total += fs::metadata(dir.join(&rel))
            .map_err(Error::io(format!("stat {}", dir.join(&rel).display())))?
            .len();
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
}
