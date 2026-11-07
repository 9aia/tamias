//! Tar archives — `bundle pack` writes `.tgz`; the http/fs sources unpack
//! tarballs here. Paths sanitize on unpack (no absolute/`..` escapes),
//! the executable bit keeps.

use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::{Archive, Builder, EntryType};

use crate::digest::payload_files;
use crate::Error;

/// `dir` → a gzipped tarball at `out` — payload files at the archive
/// root, sorted for deterministic entry order.
///
/// # Errors
/// Propagates I/O failures while packing.
pub fn pack_tgz(src_dir: &Path, out: &Path) -> Result<PathBuf, Error> {
    let file = fs::File::create(out).map_err(Error::io(format!("create {}", out.display())))?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = Builder::new(encoder);
    for rel in payload_files(src_dir)? {
        builder
            .append_path_with_name(src_dir.join(&rel), &rel)
            .map_err(Error::io(format!("archive {}", rel.display())))?;
    }
    let encoder = builder.into_inner().map_err(Error::io("finish tar"))?;
    encoder.finish().map_err(Error::io("finish gzip"))?;
    Ok(out.to_path_buf())
}

/// Unpack a plain `.tar` stream.
///
/// # Errors
/// Propagates read/write failures.
pub fn untar(reader: impl Read, dest: &Path) -> Result<(), Error> {
    unpack(Archive::new(reader), dest)
}

/// Unpack a gzipped tarball stream (`.tgz`/`.tar.gz`).
///
/// # Errors
/// Propagates read/write failures.
pub fn untar_gz(reader: impl Read, dest: &Path) -> Result<(), Error> {
    unpack(Archive::new(GzDecoder::new(reader)), dest)
}

fn unpack<R: Read>(mut archive: Archive<R>, dest: &Path) -> Result<(), Error> {
    fs::create_dir_all(dest).map_err(Error::io(format!("create {}", dest.display())))?;
    let entries = archive.entries().map_err(Error::io("read tar entries"))?;
    for entry in entries {
        let mut entry = entry.map_err(Error::io("read tar entry"))?;
        let path = entry.path().map_err(Error::io("read tar entry path"))?;
        let Some(rel) = sanitize(&path)? else {
            continue;
        };
        let target = dest.join(&rel);
        match entry.header().entry_type() {
            EntryType::Directory => {
                fs::create_dir_all(&target)
                    .map_err(Error::io(format!("create {}", target.display())))?;
            }
            EntryType::Regular => {
                let parent = target.parent().unwrap_or(dest);
                fs::create_dir_all(parent)
                    .map_err(Error::io(format!("create {}", parent.display())))?;
                let mut file = fs::File::create(&target)
                    .map_err(Error::io(format!("create {}", target.display())))?;
                io::copy(&mut entry, &mut file)
                    .map_err(Error::io(format!("write {}", target.display())))?;
                set_mode(&entry, &target)?;
            }
            EntryType::Symlink => {
                write_symlink(&entry, &target)?;
            }
            other => {
                return Err(Error::Failed(format!(
                    "unsupported tar entry type {other:?} at {}",
                    rel.display()
                )));
            }
        }
    }
    Ok(())
}

/// `./`-stripping + tar-slip rejection: absolute paths and `..`
/// components are a hard error, root entries skip.
fn sanitize(path: &Path) -> Result<Option<PathBuf>, Error> {
    let mut rel = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => rel.push(part),
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(Error::Failed(format!(
                    "unsafe path in archive: {}",
                    path.display()
                )));
            }
        }
    }
    if rel.as_os_str().is_empty() {
        Ok(None)
    } else {
        Ok(Some(rel))
    }
}

#[cfg(unix)]
fn set_mode<R: Read>(entry: &tar::Entry<'_, R>, target: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let mode = entry.header().mode().map_err(Error::io("read tar mode"))?;
    fs::set_permissions(target, fs::Permissions::from_mode(mode))
        .map_err(Error::io(format!("chmod {}", target.display())))
}

#[cfg(unix)]
fn write_symlink<R: Read>(entry: &tar::Entry<'_, R>, target: &Path) -> Result<(), Error> {
    let link = entry
        .link_name()
        .map_err(Error::io("read tar symlink"))?
        .ok_or_else(|| Error::Failed("symlink entry without a target".to_string()))?;
    if link.is_absolute() || link.components().any(|c| c == Component::ParentDir) {
        return Err(Error::Failed(format!(
            "unsafe symlink in archive: {}",
            link.display()
        )));
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("/"));
    fs::create_dir_all(parent).map_err(Error::io(format!("create {}", parent.display())))?;
    std::os::unix::fs::symlink(&link, target)
        .map_err(Error::io(format!("symlink {}", target.display())))
}

#[cfg(not(unix))]
fn set_mode<R: Read>(_entry: &tar::Entry<'_, R>, _target: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(not(unix))]
fn write_symlink<R: Read>(_entry: &tar::Entry<'_, R>, target: &Path) -> Result<(), Error> {
    Err(Error::Failed(format!(
        "symlinks unsupported on this platform: {}",
        target.display()
    )))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod archive_tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn pack_and_unpack_tgz_roundtrip() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/b.txt"), "b").unwrap();
        let out = tempfile::tempdir().unwrap();
        let tgz = out.path().join("x.tgz");
        pack_tgz(src.path(), &tgz).unwrap();
        let dest = tempfile::tempdir().unwrap();
        untar_gz(std::fs::File::open(&tgz).unwrap(), dest.path()).unwrap();
        assert!(dest.path().join("a.txt").is_file());
        assert!(dest.path().join("sub/b.txt").is_file());
    }

    #[test]
    fn untar_plain_stream() {
        let mut builder = Builder::new(Vec::new());
        let data = b"hello";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "f.txt", &data[..])
            .unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let dest = tempfile::tempdir().unwrap();
        untar(Cursor::new(tar_bytes), dest.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join("f.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn sanitize_rejects_and_skips() {
        assert!(sanitize(Path::new("../x")).is_err());
        assert!(sanitize(Path::new("/abs")).is_err());
        assert!(sanitize(Path::new(".")).unwrap().is_none());
        assert_eq!(
            sanitize(Path::new("./a/b")).unwrap().unwrap(),
            PathBuf::from("a/b")
        );
    }

    #[test]
    fn untar_dirs_and_symlinks() {
        let mut builder = Builder::new(Vec::new());
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_mode(0o755);
        dir.set_cksum();
        builder
            .append_data(&mut dir, "d/", std::io::empty())
            .unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_size(1);
        file.set_mode(0o644);
        file.set_cksum();
        builder
            .append_data(&mut file, "d/f.txt", &b"x"[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        link.set_cksum();
        builder.append_link(&mut link, "link", "d/f.txt").unwrap();
        let bytes = builder.into_inner().unwrap();
        let dest = tempfile::tempdir().unwrap();
        untar(Cursor::new(bytes), dest.path()).unwrap();
        assert!(dest.path().join("d/f.txt").is_file());
        assert!(dest.path().join("link").exists());
    }

    #[test]
    fn untar_unsupported_entry_errors() {
        let mut builder = Builder::new(Vec::new());
        let mut fifo = tar::Header::new_gnu();
        fifo.set_entry_type(tar::EntryType::Fifo);
        fifo.set_size(0);
        fifo.set_mode(0o644);
        fifo.set_cksum();
        builder
            .append_data(&mut fifo, "p", std::io::empty())
            .unwrap();
        let bytes = builder.into_inner().unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(untar(Cursor::new(bytes), dest.path()).is_err());
    }

    #[test]
    fn unsafe_symlink_errors() {
        let mut builder = Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        link.set_cksum();
        builder.append_link(&mut link, "link", "../escape").unwrap();
        let bytes = builder.into_inner().unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(untar(Cursor::new(bytes), dest.path()).is_err());
    }
}
