#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-publish — author-side packing: a payload dir + manifest → a
//! `.tgz` (the registry stores these; the http source fetches them).
//!
//! `include:` handling (PL-0007 / P-004): by default [`pack`]
//! **flattens** — each base resolves *read-only* (installed store
//! entry → its payload dir; a ref already installed → the entry whose
//! `source_ref` matches; an existing fs path → resolved in place) and
//! [`tamias_core::overlay`] composes them under the local payload in a
//! scratch dir that is packed instead. Publish never installs
//! anything: an unresolvable base fails naming it. `keep_includes`
//! ships the raw payload dir for consumers that compose themselves.
//!
//! Flattening is **not transitive**: a base contributes whatever
//! payload is stored/installed — a store entry is already composed if
//! it had `include:` at its own install; an fs-dir base's own
//! `include:` is *not* re-resolved (same rule as install's
//! store-name path). The packed manifest is unchanged — `include:`
//! stays as provenance.

use std::fs;
use std::path::{Path, PathBuf};

use tamias_core::{
    find_store_entry, overlay, pack_tgz, payload_root, untar, untar_gz, Error, Manifest,
    StoreEntry, MANIFEST_NAMES,
};
use tamias_source::{expand_path, RefKind};
use tamias_source_fs::FsRefKind;
use tamias_store_local::Store;

/// How [`pack`] treats a manifest's `include:` list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Includes {
    /// Resolve bases read-only and pack the composed payload — the
    /// default for `publish`/`bundle pack`.
    #[default]
    Flatten,
    /// Ship the raw payload dir; consumers compose the bases
    /// themselves (`--keep-includes`).
    Keep,
}

/// The composed payload [`compose`] produced — `dir` holds the
/// overlay result (manifest pinned to the local package's), `_scratch`
/// keeps the tempdir alive until the caller is done packing/uploading.
pub struct Composed {
    /// The composed payload root — pass to `pack_tgz`/`registry.publish`.
    pub dir: PathBuf,
    /// Owns the scratch dir; dropping it deletes the composed tree.
    _scratch: tempfile::TempDir,
}

/// Compose `root`'s payload over its manifest's `include:` bases into
/// a fresh scratch dir — the read-only half of payload composition.
/// Callers check `manifest.include.is_empty()` first; an empty list
/// still produces a valid composed copy.
///
/// # Errors
/// Fails on an unresolvable/ambiguous base or I/O errors while
/// overlaying. Never installs, fetches, or mutates the store.
pub fn compose(root: &Path, store_root: &Path) -> Result<Composed, Error> {
    let manifest = Manifest::load(root)?;
    let store = Store::new(store_root);
    let entries = store.entries()?;
    let mut bases = Vec::with_capacity(manifest.include.len());
    // Scratch dirs for fs-tarball bases must outlive the overlay —
    // collect them so they drop only after `overlay` returns.
    let mut tarball_scratch = Vec::new();
    for text in &manifest.include {
        bases.push(resolve_base(&entries, text, &mut tarball_scratch)?);
    }
    let scratch = tempfile::tempdir().map_err(Error::io("create include-compose scratch"))?;
    let base_refs: Vec<&Path> = bases.iter().map(PathBuf::as_path).collect();
    overlay(&base_refs, root, scratch.path())?;
    pin_manifest(root, scratch.path())?;
    Ok(Composed {
        dir: scratch.path().to_path_buf(),
        _scratch: scratch,
    })
}

/// One include entry → one base payload dir, read-only. Order:
/// installed store name → store entry whose `source_ref` is the entry
/// verbatim (an already-installed ref) → an existing fs path (dir or
/// tarball, resolved in place — never installed). Anything else fails
/// naming the base.
fn resolve_base(
    entries: &[StoreEntry],
    text: &str,
    tarball_scratch: &mut Vec<tempfile::TempDir>,
) -> Result<PathBuf, Error> {
    // 1. Store name — label / `@scope/name` / bare `name`.
    if let Some(entry) = find_store_entry(entries, text)? {
        return Ok(entry.dir);
    }
    // `@scope/name` is a name, never a path.
    if text.starts_with('@') {
        return Err(unresolvable(text));
    }
    // 2. A ref that's already installed — provenance match.
    if let Some(entry) = entries.iter().find(|e| e.record.source_ref == text) {
        return Ok(entry.dir.clone());
    }
    // 3. Locally resolvable fs path — dir or tarball, in place.
    if FsRefKind.claims(text) {
        let path = expand_path(Path::new(text));
        if path.is_dir() {
            return payload_root(&path);
        }
        if path.is_file() {
            return untar_base(&path, tarball_scratch);
        }
    }
    Err(unresolvable(text))
}

/// Unpack a tarball base into a scratch dir and return its payload
/// root. The [`tempfile::TempDir`] is pushed onto `scratch` so it
/// lives until the overlay completes.
fn untar_base(path: &Path, scratch: &mut Vec<tempfile::TempDir>) -> Result<PathBuf, Error> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let dir = tempfile::tempdir().map_err(Error::io("create include-tarball scratch"))?;
    let file = fs::File::open(path).map_err(Error::io(format!("open {}", path.display())))?;
    if name.ends_with(".tgz") || name.ends_with(".tar.gz") {
        untar_gz(file, dir.path())?;
    } else if name.ends_with(".tar") {
        untar(file, dir.path())?;
    } else {
        return Err(Error::Failed(format!(
            "unsupported include tarball '{}' — expected .tgz/.tar.gz/.tar",
            path.display()
        )));
    }
    let root = payload_root(dir.path())?;
    scratch.push(dir);
    Ok(root)
}

/// The publish-side failure — names the base and says why it can't be
/// flattened (publish never installs; `--keep-includes` is the out).
fn unresolvable(text: &str) -> Error {
    Error::Failed(format!(
        "include '{text}' can't be resolved for publish — it is not an installed \
         package or ref, and publish never installs; install it first, point \
         include: at an installed name/ref, or republish with --keep-includes"
    ))
}

/// Pin the composed payload's manifest to the local package's manifest
/// file, byte-for-byte — same rule as install's compose: the top
/// package's manifest is authoritative (`include:` stays for
/// provenance); stray manifest spellings a base contributed are
/// removed. See `tamias_install`'s `include::pin_manifest` — duplicated
/// here because `tamias-publish` must not depend on the install
/// pipeline.
fn pin_manifest(local: &Path, dest: &Path) -> Result<(), Error> {
    let Some(src) = Manifest::path_in(local) else {
        return Ok(());
    };
    let file_name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for candidate in MANIFEST_NAMES {
        let target = dest.join(candidate);
        if candidate == file_name {
            fs::copy(&src, &target)
                .map_err(Error::io(format!("pin manifest {}", target.display())))?;
        } else if target.exists() {
            fs::remove_file(&target).map_err(Error::io(format!(
                "remove stray manifest {}",
                target.display()
            )))?;
        }
    }
    Ok(())
}

/// Pack `dir` into a `.tgz` — `<name>-<version>.tgz` by default.
///
/// `includes` selects `include:` handling: [`Includes::Flatten`] (the
/// default) resolves bases read-only against `store_root` and packs
/// the composed payload; [`Includes::Keep`] ships the raw dir.
///
/// # Errors
/// Fails when `dir` has no manifest, a base can't be resolved, or on
/// I/O error.
pub fn pack(
    dir: &Path,
    out: Option<&Path>,
    includes: Includes,
    store_root: &Path,
) -> Result<PathBuf, Error> {
    let root = payload_root(dir)?;
    let manifest = Manifest::load(&root)?;
    // Sealed packs are ciphertext-only (ADR-0004): refuse stray
    // plaintext / missing `.age` files / bad magic at pack time.
    // `sealed:` manifests can't declare `include:`, so `root` is the
    // payload that ships on every path below.
    tamias_core::sealed::check_ciphertext_only(&root, &manifest)?;
    let default_name = match &manifest.version {
        Some(v) => format!("{}-{v}.tgz", manifest.name),
        None => format!("{}.tgz", manifest.name),
    };
    let out = out.map_or_else(|| PathBuf::from(&default_name), Path::to_path_buf);
    if includes == Includes::Flatten && !manifest.include.is_empty() {
        let composed = compose(&root, store_root)?;
        // Declared `bins[].payload` files must ship in the tarball —
        // catch authoring typos here, not at install time. Checked on
        // the composed payload so `include:` bases can satisfy decls.
        tamias_bin::check_shipped_bins(&composed.dir, &manifest)?;
        return pack_tgz(&composed.dir, &out);
    }
    tamias_bin::check_shipped_bins(&root, &manifest)?;
    pack_tgz(&root, &out)
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

    fn store_entry(root: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            format!("name: {}\nversion: 1.0.0\n", name.replace('/', "-")),
        )
        .unwrap();
        for (f, c) in files {
            std::fs::write(dir.join(f), c).unwrap();
        }
        dir
    }

    #[test]
    fn packs_a_dir() {
        let _g = EnvGuard::new();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.2.0\n").unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.tgz");
        let packed = pack(src.path(), Some(&out), Includes::Flatten, store.path()).unwrap();
        assert!(packed.is_file());
    }

    #[test]
    fn pack_without_version_uses_name() {
        let _g = EnvGuard::new();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: nv\n").unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let out = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(out.path()).unwrap();
        let packed = pack(src.path(), None, Includes::Flatten, store.path()).unwrap();
        std::env::set_current_dir(old).unwrap();
        assert_eq!(packed.file_name().unwrap().to_str().unwrap(), "nv.tgz");
    }

    #[test]
    fn pack_flattens_store_name_include() {
        let tmp = tempfile::tempdir().unwrap();
        let store_root = tmp.path().join("store");
        let store = Store::new(&store_root);
        // install a base into the store directly
        let base_dir = store_entry(tmp.path(), "basepkg", &[("base.txt", "B")]);
        let record = tamias_core::StoreRecord {
            name: "base".into(),
            scope: None,
            version: Some("1.0.0".into()),
            kind: None,
            source_ref: "./base".into(),
            resolved: "sha256:abc".into(),
            digest: String::new(),
            signature: None,
            signer: None,
            trust: None,
            installed_at: 0,
        };
        store.install(&base_dir, record).unwrap();
        // package includes `base`, adds its own file + yaml merge
        let src = tmp.path().join("top");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("tamias.yaml"),
            "name: top\nversion: 2.0.0\ninclude:\n  - base\n",
        )
        .unwrap();
        std::fs::write(src.join("top.txt"), "T").unwrap();
        std::fs::write(src.join("cfg.yaml"), "k: top\n").unwrap();
        // flatten → composed tgz carries the base payload
        let out = tmp.path().join("flat.tgz");
        pack(&src, Some(&out), Includes::Flatten, &store_root).unwrap();
        let entries = tgz_files(&out);
        assert!(entries.iter().any(|e| e == "base.txt"), "{entries:?}");
        assert!(entries.iter().any(|e| e == "top.txt"), "{entries:?}");
        // keep-includes → raw payload only
        let raw = tmp.path().join("raw.tgz");
        pack(&src, Some(&raw), Includes::Keep, &store_root).unwrap();
        let raw_entries = tgz_files(&raw);
        assert!(
            !raw_entries.iter().any(|e| e == "base.txt"),
            "{raw_entries:?}"
        );
        assert!(
            raw_entries.iter().any(|e| e == "top.txt"),
            "{raw_entries:?}"
        );
    }

    #[test]
    fn pack_missing_base_fails_naming_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store_root = tmp.path().join("store");
        let src = tmp.path().join("top");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("tamias.yaml"),
            "name: top\nversion: 1.0.0\ninclude:\n  - ghost\n",
        )
        .unwrap();
        let err = pack(&src, None, Includes::Flatten, &store_root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost"), "{err}");
        assert!(err.contains("keep-includes"), "{err}");
        // keep-includes still packs fine
        let out = tmp.path().join("top.tgz");
        pack(&src, Some(&out), Includes::Keep, &store_root).unwrap();
        assert!(out.is_file());
    }

    #[test]
    fn pack_resolves_fs_dir_and_installed_ref_bases() {
        let tmp = tempfile::tempdir().unwrap();
        let store_root = tmp.path().join("store");
        let store = Store::new(&store_root);
        // an fs-dir base resolved in place (not installed)
        let fsbase = store_entry(tmp.path(), "fsbase", &[("fs.txt", "F")]);
        // an installed ref base — source_ref match
        let refdir = store_entry(tmp.path(), "refbase", &[("ref.txt", "R")]);
        store
            .install(
                &refdir,
                tamias_core::StoreRecord {
                    name: "refb".into(),
                    scope: None,
                    version: Some("1.0.0".into()),
                    kind: None,
                    source_ref: "/some/ref".into(),
                    resolved: "sha256:abc".into(),
                    digest: String::new(),
                    signature: None,
                    signer: None,
                    trust: None,
                    installed_at: 0,
                },
            )
            .unwrap();
        let fs_ref = fsbase.display().to_string();
        let src = tmp.path().join("top");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("tamias.yaml"),
            format!("name: top\nversion: 1.0.0\ninclude:\n  - '{fs_ref}'\n  - /some/ref\n"),
        )
        .unwrap();
        let out = tmp.path().join("top.tgz");
        pack(&src, Some(&out), Includes::Flatten, &store_root).unwrap();
        let entries = tgz_files(&out);
        assert!(entries.iter().any(|e| e == "fs.txt"), "{entries:?}");
        assert!(entries.iter().any(|e| e == "ref.txt"), "{entries:?}");
    }

    /// T-005: `bins[].payload` decls are gated at pack time — a decl
    /// with no shipped counterpart fails `pack`, while a decl naming a
    /// *foreign* triple passes as long as some platform dir ships the
    /// file (the install host's triple is substituted at deploy).
    #[test]
    fn pack_gates_declared_payload_bins() {
        let tmp = tempfile::tempdir().unwrap();
        let store_root = tmp.path().join("store");
        let src = tmp.path().join("tool");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("tamias.yaml"),
            "name: tool\nversion: 1.0.0\nbins:\n  \
             - name: tool\n    payload: tools/bin/x86_64-unknown-linux-gnu/tool\n",
        )
        .unwrap();
        // nothing shipped → pack fails naming the decl
        let err = pack(&src, None, Includes::Keep, &store_root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'tool'"), "{err}");
        // a foreign-triple dir satisfies the decl on any host
        let foreign = src.join("tools/bin/aarch64-apple-darwin");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("tool"), "bin").unwrap();
        let out = tmp.path().join("tool.tgz");
        pack(&src, Some(&out), Includes::Keep, &store_root).unwrap();
        let entries = tgz_files(&out);
        assert!(
            entries
                .iter()
                .any(|e| e.ends_with("tools/bin/aarch64-apple-darwin/tool")),
            "{entries:?}"
        );
    }

    /// ADR-0004: `pack` enforces the sealed ciphertext-only gate — a
    /// clean `.age` payload ships, stray plaintext / missing declared
    /// files / `include:` on a sealed manifest all refuse.
    #[test]
    fn pack_gates_sealed_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let store_root = tmp.path().join("store");
        let write_age = |dir: &std::path::Path, rel: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut bytes = tamias_core::sealed::AGE_MAGIC.to_vec();
            bytes.extend_from_slice(b"\n-> X25519 fake\n\nBODY");
            std::fs::write(path, bytes).unwrap();
        };
        let sealed_yaml = "name: sec\nversion: 1.0.0\nsealed:\n  format: age/v1\n  \
                          recipients: sealed/recipients\n  files:\n    \
                          token: sealed/token.age\n";
        let src = tmp.path().join("sec");
        std::fs::create_dir_all(src.join("sealed")).unwrap();
        std::fs::write(src.join("tamias.yaml"), sealed_yaml).unwrap();
        std::fs::write(src.join("sealed/recipients"), "age1pub…\n").unwrap();
        write_age(&src, "sealed/token.age");
        // clean → packs
        let out = tmp.path().join("sec.tgz");
        pack(&src, Some(&out), Includes::Keep, &store_root).unwrap();
        let entries = tgz_files(&out);
        assert!(
            entries.iter().any(|e| e.ends_with("sealed/token.age")),
            "{entries:?}"
        );
        // stray plaintext → refused
        std::fs::write(src.join("notes.txt"), "oops").unwrap();
        let err = pack(&src, None, Includes::Keep, &store_root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("notes.txt"), "{err}");
        std::fs::remove_file(src.join("notes.txt")).unwrap();
        // .age suffix without the magic → refused
        std::fs::write(src.join("sealed/fake.age"), "not age").unwrap();
        let err = pack(&src, None, Includes::Keep, &store_root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sealed/fake.age"), "{err}");
        std::fs::remove_file(src.join("sealed/fake.age")).unwrap();
        // declared file missing → refused
        std::fs::remove_file(src.join("sealed/token.age")).unwrap();
        let err = pack(&src, None, Includes::Keep, &store_root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sealed/token.age"), "{err}");
        write_age(&src, "sealed/token.age");
        // sealed + include: → manifest validation refuses
        std::fs::write(
            src.join("tamias.yaml"),
            format!("{sealed_yaml}include:\n  - base\n"),
        )
        .unwrap();
        let err = pack(&src, None, Includes::Keep, &store_root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("include"), "{err}");
    }

    /// File list inside a `.tgz` — entries are stored under a single
    /// top-level dir; we compare suffixes.
    fn tgz_files(tgz: &Path) -> Vec<String> {
        let mut names = Vec::new();
        // untar_gz writes to a dir — use a scratch and walk it
        let dir = tempfile::tempdir().unwrap();
        untar_gz(fs::File::open(tgz).unwrap(), dir.path()).unwrap();
        for rel in tamias_core::digest::payload_files(dir.path()).unwrap() {
            names.push(rel.to_string_lossy().replace('\\', "/"));
        }
        names
    }
}
