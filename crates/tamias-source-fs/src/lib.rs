#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source-fs — the filepath ref kind + source: `./path`, `/path`,
//! `~/path`, `nested/path`. A dir copies its payload (`.git/` excluded); a
//! `.tgz`/`.tar.gz`/`.tar` file unpacks. The content key is the payload's
//! sha256 — a repeat install of identical bytes hits the fetch cache.

use std::any::Any;
use std::fs;
use std::path::{Path, PathBuf};

use tamias_core::{copy_tree, dir_digest, file_digest, untar, untar_gz, Error};
use tamias_source::{expand_path, Fetched, Plan, RefKind, Source, Spec};

/// The fs ref kind — the fallback: any path-shaped ref.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsRefKind;

/// A parsed fs ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsSpec {
    /// The `~`-expanded path.
    pub path: PathBuf,
}

impl RefKind for FsRefKind {
    fn claims(&self, input: &str) -> bool {
        input == "."
            || input == ".."
            || input == "~"
            || input.starts_with("./")
            || input.starts_with("../")
            || input.starts_with("~/")
            || input.starts_with('/')
            || input.contains('/')
    }

    fn parse(&self, input: &str) -> Result<Spec, Error> {
        Ok(Box::new(FsSpec {
            path: expand_path(Path::new(input)),
        }))
    }
}

/// The fs source.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsSource;

impl FsSource {
    /// The fs source — stateless.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

struct FsPlan {
    path: PathBuf,
}

impl Plan for FsPlan {}

impl Source for FsSource {
    fn accepts(&self, spec: &dyn Any) -> bool {
        spec.is::<FsSpec>()
    }

    fn plan(&self, spec: &dyn Any) -> Result<Box<dyn Plan>, Error> {
        let FsSpec { path } = spec
            .downcast_ref::<FsSpec>()
            .ok_or_else(|| Error::Failed("FsSource only plans fs refs".to_string()))?;
        if !path.exists() {
            return Err(Error::Failed(format!("no such path: {}", path.display())));
        }
        Ok(Box::new(FsPlan { path: path.clone() }))
    }

    fn fetch(&self, plan: &dyn Plan, dest_dir: &Path) -> Result<Fetched, Error> {
        let plan = plan
            .downcast_ref::<FsPlan>()
            .ok_or_else(|| Error::Failed("FsSource only fetches fs plans".to_string()))?;
        fs::create_dir_all(dest_dir)
            .map_err(Error::io(format!("create {}", dest_dir.display())))?;
        let digest = if plan.path.is_dir() {
            copy_tree(&plan.path, dest_dir)?;
            dir_digest(dest_dir)?
        } else {
            unpack_tarball(&plan.path, dest_dir)?
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
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::Arc;

    use super::*;
    use tamias_source::{Refs, Sources};

    fn wire() -> (Refs, Sources) {
        (
            Refs::new().with(Arc::new(FsRefKind)),
            Sources::new().with(Arc::new(FsSource)),
        )
    }

    struct ForeignPlan;
    impl Plan for ForeignPlan {}

    #[test]
    fn dir_tarball_and_error_arms() {
        let (refs, sources) = wire();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();

        // a dir copies + digests; no prefetch identity
        let spec = refs.parse(src.path().to_str().unwrap()).unwrap();
        let planned = sources.plan(&*spec).unwrap();
        assert!(planned.prefetch_identity().is_none());
        let dest = tempfile::tempdir().unwrap();
        let fetched = planned.fetch(dest.path()).unwrap();
        assert!(fetched.content_key.starts_with("sha256-"));
        assert!(dest.path().join("a.txt").is_file());

        // a .tgz unpacks
        let out = tempfile::tempdir().unwrap();
        let tgz = out.path().join("x.tgz");
        tamias_core::pack_tgz(src.path(), &tgz).unwrap();
        let spec = refs.parse(tgz.to_str().unwrap()).unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let d2 = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d2.path()).is_ok());

        // a .tar unpacks
        let tar_bytes = {
            let mut b = tar::Builder::new(Vec::new());
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, "ok.txt", &b"x"[..]).unwrap();
            b.into_inner().unwrap()
        };
        let tar_path = out.path().join("x.tar");
        std::fs::write(&tar_path, &tar_bytes).unwrap();
        let spec = refs.parse(tar_path.to_str().unwrap()).unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let d2b = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d2b.path()).is_ok());

        // missing path → plan error
        let spec = refs.parse("./nonexistent-xyz-9").unwrap();
        assert!(sources.plan(&*spec).is_err());

        // an unsupported file → fetch error
        let bad = out.path().join("x.zip");
        std::fs::write(&bad, "x").unwrap();
        let spec = refs.parse(bad.to_str().unwrap()).unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let d3 = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d3.path()).is_err());

        // foreign spec / plan
        let source = FsSource;
        assert!(!source.accepts(&7u8));
        assert!(source.plan(&7u8).is_err());
        assert!(source.fetch(&ForeignPlan, d3.path()).is_err());

        // claims
        assert!(FsRefKind.claims("/abs"));
        assert!(!FsRefKind.claims("git:x"));
    }
}
