#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source-fs — the filepath arm of the ref grammar: `./path`,
//! `/path`, `~/path`, `nested/path`. A dir copies its payload (`.git/`
//! excluded); a `.tgz`/`.tar.gz`/`.tar` file unpacks. The content key is
//! the payload's sha256 — a repeat install of identical bytes hits the
//! fetch cache instead of re-copying.

use std::fs;
use std::path::Path;

use tamias_core::{
    copy_tree, dir_digest, expand_path, file_digest, untar, untar_gz, Error, SourceRef,
};
use tamias_source::{Fetched, Resolved, Source};

/// [`Source`] over local paths.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsSource;

impl FsSource {
    /// The fs source — stateless.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Source for FsSource {
    fn resolve(&self, reference: &SourceRef) -> Result<Resolved, Error> {
        let SourceRef::Fs(path) = reference else {
            return Err(Error::Failed("FsSource only resolves fs refs".to_string()));
        };
        let path = expand_path(path);
        if !path.exists() {
            return Err(Error::Failed(format!("no such path: {}", path.display())));
        }
        Ok(Resolved::Fs { path })
    }

    fn fetch(&self, resolved: &Resolved, dest_dir: &Path) -> Result<Fetched, Error> {
        let Resolved::Fs { path } = resolved else {
            return Err(Error::Failed("FsSource only fetches fs specs".to_string()));
        };
        fs::create_dir_all(dest_dir)
            .map_err(Error::io(format!("create {}", dest_dir.display())))?;
        let digest = if path.is_dir() {
            copy_tree(path, dest_dir)?;
            dir_digest(dest_dir)?
        } else {
            unpack_tarball(path, dest_dir)?
        };
        Ok(Fetched {
            dir: dest_dir.to_path_buf(),
            resolved: digest.clone(),
            content_key: digest.replacen("sha256:", "sha256-", 1),
        })
    }
}

/// A tarball *file* — digest the bytes for the key, then unpack by
/// extension (deliberately case-exact).
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn unpack_tarball(path: &Path, dest_dir: &Path) -> Result<String, Error> {
    let digest = file_digest(path)?;
    let file = fs::File::open(path).map_err(Error::io(format!("open {}", path.display())))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name.ends_with(".tgz") || name.ends_with(".tar.gz") {
        untar_gz(file, dest_dir)?;
    } else if name.ends_with(".tar") {
        untar(file, dest_dir)?;
    } else {
        return Err(Error::Failed(format!(
            "unsupported file '{}' — expected a dir or .tgz/.tar.gz/.tar",
            path.display()
        )));
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_a_dir_and_digests() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let dest = tempfile::tempdir().unwrap();
        let reference = SourceRef::Fs(src.path().to_path_buf());
        let resolved = FsSource::new().resolve(&reference).unwrap();
        let fetched = FsSource::new().fetch(&resolved, dest.path()).unwrap();
        assert!(fetched.resolved.starts_with("sha256:"));
        assert_eq!(
            fetched.content_key,
            fetched.resolved.replacen("sha256:", "sha256-", 1)
        );
        assert!(dest.path().join("a.txt").exists());
        assert!(dest.path().join("tamias.yaml").exists());
    }
}
