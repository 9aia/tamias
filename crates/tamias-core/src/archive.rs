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
    // Never emit GNUSparse entries: the unpacker only accepts
    // Regular/Directory/Symlink, and a release binary with holes
    // (kuda/bupha drivers) would pack sparse and fail to install.
    // Sparse detection is a tar-Builder default — opt out.
    builder.sparse(false);
    for rel in payload_files(src_dir)? {
        builder
            .append_path_with_name(src_dir.join(&rel), &rel)
            .map_err(Error::io(format!("archive {}", rel.display())))?;
    }
    let encoder = builder.into_inner().map_err(Error::io("finish tar"))?;
    encoder.finish().map_err(Error::io("finish gzip"))?;
    Ok(out.to_path_buf())
}

/// [`pack_tgz`] to a sibling tmp file, fsync, then atomic rename —
/// a crash mid-pack leaves a stray `*.tmp`, never a truncated blob at
/// `out`.
///
/// # Errors
/// Propagates pack/write/rename failures; the tmp file is cleaned up.
pub fn pack_tgz_atomic(src_dir: &Path, out: &Path) -> Result<PathBuf, Error> {
    let tmp = crate::atomic::tmp_path(out);
    let result = (|| -> Result<(), Error> {
        pack_tgz(src_dir, &tmp)?;
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
            .map_err(Error::io(format!("open {}", tmp.display())))?;
        f.sync_all()
            .map_err(Error::io(format!("sync {}", tmp.display())))?;
        fs::rename(&tmp, out).map_err(Error::io(format!(
            "rename {} → {}",
            tmp.display(),
            out.display()
        )))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map(|()| out.to_path_buf())
}

/// Default cap on total *unpacked* bytes — 2 GiB. `max_download`
/// bounds the compressed blob only; a small `.tgz` can expand without
/// bound, so every untar path enforces this ceiling.
pub const DEFAULT_MAX_UNPACK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Default cap on tar entry count — 100k entries bounds inode/dir spam
/// from a tiny archive.
pub const DEFAULT_MAX_ENTRIES: u64 = 100_000;

/// Unpack a plain `.tar` stream under the default size/entry caps.
///
/// # Errors
/// Propagates read/write failures; fails when the unpacked size or
/// entry count exceeds [`DEFAULT_MAX_UNPACK_BYTES`] /
/// [`DEFAULT_MAX_ENTRIES`].
pub fn untar(reader: impl Read, dest: &Path) -> Result<(), Error> {
    untar_limited(reader, dest, DEFAULT_MAX_UNPACK_BYTES, DEFAULT_MAX_ENTRIES)
}

/// Unpack a plain `.tar` stream with explicit caps — `max_bytes` bounds
/// total bytes written for regular files; `max_entries` bounds tar
/// entries (dirs/symlinks included).
///
/// # Errors
/// Propagates read/write failures; fails past either cap.
pub fn untar_limited(
    reader: impl Read,
    dest: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<(), Error> {
    unpack(Archive::new(reader), dest, max_bytes, max_entries)
}

/// Unpack a gzipped tarball stream (`.tgz`/`.tar.gz`) under the default
/// size/entry caps.
///
/// # Errors
/// Propagates read/write failures; fails when the unpacked size or
/// entry count exceeds the defaults.
pub fn untar_gz(reader: impl Read, dest: &Path) -> Result<(), Error> {
    untar_gz_limited(reader, dest, DEFAULT_MAX_UNPACK_BYTES, DEFAULT_MAX_ENTRIES)
}

/// Unpack a gzipped tarball stream with explicit caps — see
/// [`untar_limited`].
///
/// # Errors
/// Propagates read/write failures; fails past either cap.
pub fn untar_gz_limited(
    reader: impl Read,
    dest: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<(), Error> {
    unpack(
        Archive::new(GzDecoder::new(reader)),
        dest,
        max_bytes,
        max_entries,
    )
}

fn unpack<R: Read>(
    mut archive: Archive<R>,
    dest: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<(), Error> {
    fs::create_dir_all(dest).map_err(Error::io(format!("create {}", dest.display())))?;
    let entries = archive.entries().map_err(Error::io("read tar entries"))?;
    let mut unpacked: u64 = 0;
    let mut seen: u64 = 0;
    for entry in entries {
        seen += 1;
        if seen > max_entries {
            return Err(Error::Failed(format!(
                "tarball exceeds entry limit ({max_entries} entries)"
            )));
        }
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
                // Read at most `remaining + 1` bytes so an oversized
                // file trips the cap without buffering it whole.
                let remaining = max_bytes.saturating_sub(unpacked);
                let written = io::copy(
                    &mut (&mut entry).take(remaining.saturating_add(1)),
                    &mut file,
                )
                .map_err(Error::io(format!("write {}", target.display())))?;
                unpacked = unpacked.saturating_add(written);
                if unpacked > max_bytes {
                    return Err(Error::Failed(format!(
                        "tarball exceeds unpacked size limit ({max_bytes} bytes)"
                    )));
                }
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
    // Absolute paths, `..` escapes, and self-references (`.`/`./`/empty —
    // they resolve to the link's own dir and self-cycle any walk) are
    // all unsafe.
    let self_ref = link.components().all(|c| matches!(c, Component::CurDir));
    if self_ref || link.is_absolute() || link.components().any(|c| c == Component::ParentDir) {
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
    fn pack_tgz_atomic_roundtrips_and_leaves_no_tmp() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let out = tempfile::tempdir().unwrap();
        let tgz = out.path().join("x.tgz");
        pack_tgz_atomic(src.path(), &tgz).unwrap();
        // valid blob at the canonical path, no tmp litter
        let dest = tempfile::tempdir().unwrap();
        untar_gz(std::fs::File::open(&tgz).unwrap(), dest.path()).unwrap();
        assert_eq!(std::fs::read(dest.path().join("a.txt")).unwrap(), b"a");
        let entries: Vec<_> = std::fs::read_dir(out.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
        // repack over the existing blob replaces it
        pack_tgz_atomic(src.path(), &tgz).unwrap();
        assert!(tgz.is_file());
    }

    #[test]
    fn pack_tgz_sparse_file_roundtrips_as_regular_entries() {
        let src = tempfile::tempdir().unwrap();
        // A file with a hole — SEEK_HOLE reports it sparse on
        // supporting filesystems; tar's Builder would emit a
        // GNUSparse entry the unpacker rejects. pack_tgz must write
        // it as a plain Regular entry instead.
        let holey = src.path().join("holey.bin");
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::File::create(&holey).unwrap();
            f.write_all(b"head").unwrap();
            f.seek(SeekFrom::Start(1 << 20)).unwrap();
            f.write_all(b"tail").unwrap();
        }
        let out = tempfile::tempdir().unwrap();
        let tgz = out.path().join("x.tgz");
        pack_tgz(src.path(), &tgz).unwrap();
        let dest = tempfile::tempdir().unwrap();
        untar_gz(std::fs::File::open(&tgz).unwrap(), dest.path()).unwrap();
        let got = std::fs::read(dest.path().join("holey.bin")).unwrap();
        assert_eq!(&got[..4], b"head");
        assert_eq!(&got[got.len() - 4..], b"tail");
        assert_eq!(got.len(), (1 << 20) + 4);
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

    #[test]
    fn self_referential_symlink_errors() {
        // `a -> .` resolves to the link's own dir — it self-cycles any
        // walk that follows links, so untar refuses it outright.
        let mut builder = Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        link.set_cksum();
        builder.append_link(&mut link, "a", ".").unwrap();
        let bytes = builder.into_inner().unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(untar(Cursor::new(bytes), dest.path()).is_err());
    }

    #[test]
    fn untar_skips_rootless_entry() {
        let mut builder = Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_mode(0o644);
        h.set_cksum();
        builder.append_data(&mut h, ".", std::io::empty()).unwrap();
        let mut f = tar::Header::new_gnu();
        f.set_size(1);
        f.set_mode(0o644);
        f.set_cksum();
        builder.append_data(&mut f, "ok.txt", &b"x"[..]).unwrap();
        let bytes = builder.into_inner().unwrap();
        let dest = tempfile::tempdir().unwrap();
        untar(Cursor::new(bytes), dest.path()).unwrap();
        assert!(dest.path().join("ok.txt").is_file());
    }

    /// A tar stream with `files` regular entries of `size` bytes each.
    fn tar_with_files(files: usize, size: usize) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        let data = vec![b'x'; size];
        for i in 0..files {
            let mut h = tar::Header::new_gnu();
            h.set_size(size as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder
                .append_data(&mut h, format!("f{i}.txt"), &data[..])
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn untar_limited_byte_cap() {
        let bytes = tar_with_files(1, 100);
        let dest = tempfile::tempdir().unwrap();
        // under the cap → ok
        untar_limited(Cursor::new(&bytes), dest.path(), 100, 10).unwrap();
        assert_eq!(
            std::fs::metadata(dest.path().join("f0.txt")).unwrap().len(),
            100
        );
        // over the cap → error
        let dest2 = tempfile::tempdir().unwrap();
        assert!(untar_limited(Cursor::new(&bytes), dest2.path(), 99, 10).is_err());
        // zero cap rejects any non-empty file
        let dest3 = tempfile::tempdir().unwrap();
        assert!(untar_limited(Cursor::new(&bytes), dest3.path(), 0, 10).is_err());
    }

    #[test]
    fn untar_limited_byte_cap_counts_across_files() {
        // two 60-byte files = 120 unpacked; a 100-byte cap trips on the
        // second file even though each fits alone.
        let bytes = tar_with_files(2, 60);
        let dest = tempfile::tempdir().unwrap();
        assert!(untar_limited(Cursor::new(&bytes), dest.path(), 100, 10).is_err());
        let dest2 = tempfile::tempdir().unwrap();
        untar_limited(Cursor::new(&bytes), dest2.path(), 120, 10).unwrap();
    }

    #[test]
    fn untar_limited_entry_cap() {
        let bytes = tar_with_files(3, 1);
        let dest = tempfile::tempdir().unwrap();
        assert!(untar_limited(Cursor::new(&bytes), dest.path(), 1 << 20, 3).is_ok());
        let dest2 = tempfile::tempdir().unwrap();
        assert!(untar_limited(Cursor::new(&bytes), dest2.path(), 1 << 20, 2).is_err());
    }

    #[test]
    fn untar_gz_limited_caps_apply() {
        // gzip a small tar, then unpack with a tiny byte cap.
        let tar_bytes = tar_with_files(1, 50);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &tar_bytes).unwrap();
        let gz = enc.finish().unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(untar_gz_limited(Cursor::new(&gz), dest.path(), 10, 100).is_err());
        let dest2 = tempfile::tempdir().unwrap();
        untar_gz_limited(Cursor::new(&gz), dest2.path(), 50, 100).unwrap();
        assert_eq!(
            std::fs::metadata(dest2.path().join("f0.txt"))
                .unwrap()
                .len(),
            50
        );
    }

    #[cfg(unix)]
    #[test]
    fn untar_create_dest_error() {
        use std::os::unix::fs::PermissionsExt;
        let ro = tempfile::tempdir().unwrap();
        let rodest = ro.path().join("d");
        std::fs::create_dir_all(&rodest).unwrap();
        std::fs::set_permissions(&rodest, std::fs::Permissions::from_mode(0o555)).unwrap();
        let bytes = vec![0u8; 1024];
        assert!(untar(Cursor::new(bytes), &rodest.join("child")).is_err());
        std::fs::set_permissions(&rodest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
