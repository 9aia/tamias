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
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

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
}
