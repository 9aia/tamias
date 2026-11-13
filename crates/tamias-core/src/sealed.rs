//! `sealed:` packs — age-encrypted payloads tamias carries **opaquely**
//! (tamias ADR-0004 / org P-027).
//!
//! A sealed pack's payload is ciphertext only: the engine, store,
//! cache, registry, TUI, and logs treat every payload file as an
//! opaque blob — digest, verify, and sign work unchanged because they
//! already operate on bytes. Tamias has **no decrypt path**; opening
//! is cuora's job, in the consumer's context, via the content-type
//! driver.
//!
//! Layout (payload-relative):
//!
//! ```text
//! sealed/
//!   recipients            # age public keys, one per line (public by design)
//!   registry-token.age    # age ciphertext of the secret bytes
//! tamias.yaml             # manifest; `sealed:` maps logical → file
//! ```
//!
//! This module owns the manifest-side [`Sealed`] block, its semantic
//! checks, and [`check_ciphertext_only`] — the publish/install gate
//! that refuses a sealed pack carrying stray plaintext.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::digest::payload_files;
use crate::manifest::{is_valid_name, Manifest, MANIFEST_NAMES};
use crate::Error;

/// The only sealing format v1 understands — `age` file encryption
/// (X25519 recipients + ChaCha20-Poly1305 STREAM), header line
/// `age-encryption.org/v1`.
pub const AGE_FORMAT: &str = "age/v1";

/// age file magic — every age-encrypted file opens with this header
/// line. The publish/install ciphertext gate keys on it.
pub const AGE_MAGIC: &[u8] = b"age-encryption.org/v1";

/// The `sealed:` manifest block — present means the pack is
/// **ciphertext-only**. Orthogonal to `type:`: `type: secrets` is a
/// convention paired with the cuora driver, not a requirement.
///
/// ```yaml
/// sealed:
///   format: age/v1
///   recipients: sealed/recipients      # payload-relative path
///   files:
///     registry-token: sealed/registry-token.age
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Sealed {
    /// Sealing format — only `age/v1` exists in v1.
    pub format: String,
    /// Payload-relative path of the recipients file — age public keys,
    /// one per line. Public by design; it rides the payload so the
    /// pack digest (and any `tamias-sign` signature) covers it.
    pub recipients: String,
    /// Logical secret name → payload-relative `.age` file. The map is
    /// the index cuora's driver resolves `cuora://` refs through.
    pub files: std::collections::BTreeMap<String, String>,
}

/// A clean payload-relative path: `/`-separated, no empty/`.`/`..`
/// segments, no leading `/`, no NUL/backslash. Same safety bar as
/// `bins[].payload`, minus the `tools/bin/` prefix rule.
fn check_rel_path(what: &str, path: &str) -> Result<(), Error> {
    let bad = |why: &str| Err(Error::Failed(format!("sealed {what} '{path}' — {why}")));
    if path.is_empty() || path.contains(['\0', '\\']) {
        return bad("empty or contains NUL/backslash");
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return bad("must be a clean relative path (no empty, '.', or '..' segments)");
    }
    Ok(())
}

impl Sealed {
    /// Semantic checks the schema can't express — known format,
    /// non-empty file map, valid logical names + payload-relative
    /// `.age` paths, distinct declared paths, no manifest-name
    /// collisions.
    ///
    /// # Errors
    /// Fails on a malformed `sealed:` block.
    pub fn check(&self) -> Result<(), Error> {
        if self.format != AGE_FORMAT {
            return Err(Error::Failed(format!(
                "sealed format '{}' is not supported (only '{AGE_FORMAT}')",
                self.format
            )));
        }
        check_rel_path("recipients", &self.recipients)?;
        if MANIFEST_NAMES.contains(&self.recipients.as_str()) {
            return Err(Error::Failed(format!(
                "sealed recipients path '{}' collides with a manifest filename",
                self.recipients
            )));
        }
        if self.files.is_empty() {
            return Err(Error::Failed(
                "sealed.files is empty — a sealed pack must map at least one secret".to_string(),
            ));
        }
        let mut declared = BTreeSet::from([self.recipients.as_str()]);
        for (name, path) in &self.files {
            if !is_valid_name(name) {
                return Err(Error::Failed(format!(
                    "invalid sealed secret name '{name}' — expected [A-Za-z0-9][A-Za-z0-9._-]*"
                )));
            }
            check_rel_path(&format!("files['{name}']"), path)?;
            if !path.ends_with(".age") {
                return Err(Error::Failed(format!(
                    "sealed files['{name}'] '{path}' must end in .age"
                )));
            }
            if MANIFEST_NAMES.contains(&path.as_str()) {
                return Err(Error::Failed(format!(
                    "sealed files['{name}'] '{path}' collides with a manifest filename"
                )));
            }
            if !declared.insert(path.as_str()) {
                return Err(Error::Failed(format!(
                    "sealed path '{path}' is declared more than once"
                )));
            }
        }
        Ok(())
    }
}

/// True when `bytes` opens with the age file magic — the ciphertext
/// marker the sealed-pack gate validates against.
#[must_use]
pub fn is_age_ciphertext(bytes: &[u8]) -> bool {
    bytes.starts_with(AGE_MAGIC)
}

/// True when the file at `path` carries the age magic header.
///
/// # Errors
/// Propagates I/O failures opening/reading the file.
pub fn has_age_magic(path: &Path) -> Result<bool, Error> {
    let mut head = [0u8; 64];
    let mut file = File::open(path).map_err(Error::io(format!("open {}", path.display())))?;
    let n = file
        .read(&mut head)
        .map_err(Error::io(format!("read {}", path.display())))?;
    Ok(is_age_ciphertext(&head[..n]))
}

/// The publish/install gate for sealed packs (ADR-0004 §2): every
/// payload file — except the manifest spellings and the declared
/// `sealed.recipients` target (a public-key list, plaintext by
/// design) — must end in `.age` **and** carry the age magic. Declared
/// `sealed.files` paths must exist. No warn-mode: a sealed pack with
/// stray plaintext is a bug, not a preference.
///
/// No-op when the manifest has no `sealed:` block.
///
/// # Errors
/// Fails naming the offending file, or when a declared path is
/// missing from the payload.
pub fn check_ciphertext_only(root: &Path, manifest: &Manifest) -> Result<(), Error> {
    let Some(sealed) = &manifest.sealed else {
        return Ok(());
    };
    let rel = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let files = payload_files(root)?;
    let present: BTreeSet<String> = files.iter().map(|p| rel(p)).collect();
    // Declared paths must actually ship — same spirit as the
    // `bins[].payload` shipped-files gate.
    for declared in std::iter::once(&sealed.recipients).chain(sealed.files.values()) {
        if !present.contains(declared) {
            return Err(Error::Failed(format!(
                "sealed pack declares '{declared}' but it is not in the payload"
            )));
        }
    }
    for path in &files {
        let rel_path = rel(path);
        if MANIFEST_NAMES.contains(&rel_path.as_str()) || rel_path == sealed.recipients {
            continue;
        }
        if !rel_path.ends_with(".age") {
            return Err(Error::Failed(format!(
                "sealed pack carries non-ciphertext payload file '{rel_path}' — \
                 every payload file must be a .age file (or the manifest / recipients list)"
            )));
        }
        if !has_age_magic(&root.join(path))? {
            return Err(Error::Failed(format!(
                "sealed pack payload file '{rel_path}' lacks the age magic header — \
                 it is not age ciphertext"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn sealed_manifest() -> Manifest {
        Manifest {
            name: "secrets-x".into(),
            version: Some("1.0.0".into()),
            kind: Some("secrets".into()),
            sealed: Some(Sealed {
                format: AGE_FORMAT.into(),
                recipients: "sealed/recipients".into(),
                files: std::collections::BTreeMap::from([(
                    "registry-token".into(),
                    "sealed/registry-token.age".into(),
                )]),
            }),
            ..Default::default()
        }
    }

    /// Minimal age-shaped ciphertext — magic line + a body. The gate
    /// only checks the header; real encryption is cuora's job.
    fn fake_age(dir: &Path, rel: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = AGE_MAGIC.to_vec();
        bytes.extend_from_slice(b"\n-> X25519 fake\n\nBODY");
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn sealed_block_validates() {
        assert!(sealed_manifest().validate().is_ok());
        // unknown format
        let mut m = sealed_manifest();
        m.sealed.as_mut().unwrap().format = "sops/v1".into();
        assert!(m.validate().is_err());
        // empty files map
        let mut m = sealed_manifest();
        m.sealed.as_mut().unwrap().files.clear();
        assert!(m.validate().is_err());
        // bad logical name / bad path / non-.age path / traversal
        for (name, path) in [
            ("bad name", "sealed/a.age"),
            ("ok", "../escape.age"),
            ("ok", "sealed//a.age"),
            ("ok", "plain.txt"),
            ("ok", "tamias.yaml"),
        ] {
            let mut m = sealed_manifest();
            m.sealed.as_mut().unwrap().files =
                std::collections::BTreeMap::from([(name.into(), path.into())]);
            assert!(m.validate().is_err(), "{name} -> {path} must fail");
        }
        // duplicate declared path
        let mut m = sealed_manifest();
        m.sealed
            .as_mut()
            .unwrap()
            .files
            .insert("other".into(), "sealed/registry-token.age".into());
        assert!(m.validate().is_err());
        // recipients path colliding with a files value
        let mut m = sealed_manifest();
        m.sealed.as_mut().unwrap().recipients = "sealed/registry-token.age".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn sealed_rejects_include_bins_provider() {
        for (include, bins, provider) in [
            (vec!["base".to_string()], vec![], None),
            (vec![], vec![crate::manifest::BinDecl::default()], None),
            (vec![], vec![], Some(crate::manifest::Provider::default())),
        ] {
            let m = Manifest {
                include,
                bins,
                provider,
                ..sealed_manifest()
            };
            assert!(m.validate().is_err());
        }
        // deps stay allowed
        let m = Manifest {
            deps: vec!["other/pkg".into()],
            ..sealed_manifest()
        };
        assert!(m.validate().is_ok());
    }

    #[test]
    fn ciphertext_gate_passes_clean_pack() {
        let dir = tempfile::tempdir().unwrap();
        let m = sealed_manifest();
        std::fs::write(dir.path().join("tamias.yaml"), "name: secrets-x\n").unwrap();
        std::fs::write(dir.path().join("sealed/recipients"), "age1pub...").unwrap_err();
        // recipients needs its parent dir
        std::fs::create_dir_all(dir.path().join("sealed")).unwrap();
        std::fs::write(dir.path().join("sealed/recipients"), "age1pub...").unwrap();
        fake_age(dir.path(), "sealed/registry-token.age");
        // an undeclared .age file with magic is fine (ADR: ciphertext is opaque)
        fake_age(dir.path(), "sealed/extra.age");
        assert!(check_ciphertext_only(dir.path(), &m).is_ok());
    }

    #[test]
    fn ciphertext_gate_rejects_stray_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let m = sealed_manifest();
        std::fs::write(dir.path().join("tamias.yaml"), "name: secrets-x\n").unwrap();
        std::fs::create_dir_all(dir.path().join("sealed")).unwrap();
        std::fs::write(dir.path().join("sealed/recipients"), "age1pub...").unwrap();
        fake_age(dir.path(), "sealed/registry-token.age");
        // stray plaintext — no .age suffix
        std::fs::write(dir.path().join("notes.txt"), "oops").unwrap();
        let err = check_ciphertext_only(dir.path(), &m)
            .unwrap_err()
            .to_string();
        assert!(err.contains("notes.txt"), "{err}");
        // .age suffix but no magic (notes.txt removed — the gate reports
        // the first offender in sorted order)
        std::fs::remove_file(dir.path().join("notes.txt")).unwrap();
        std::fs::write(dir.path().join("sealed/fake.age"), "not age").unwrap();
        let err = check_ciphertext_only(dir.path(), &m)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sealed/fake.age"), "{err}");
        // declared path missing from payload
        let mut m2 = sealed_manifest();
        m2.sealed
            .as_mut()
            .unwrap()
            .files
            .insert("ghost".into(), "sealed/ghost.age".into());
        m2.validate().unwrap();
        let err = check_ciphertext_only(dir.path(), &m2)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sealed/ghost.age"), "{err}");
    }

    #[test]
    fn ciphertext_gate_noop_without_sealed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "plain").unwrap();
        let m = Manifest {
            name: "x".into(),
            ..Default::default()
        };
        assert!(check_ciphertext_only(dir.path(), &m).is_ok());
    }
}
