//! The install record — the provenance a store entry carries and
//! `list`/`info`/`verify` report on.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::digest::dir_size;
use crate::{Error, Manifest};

/// Per-entry install record written inside the store dir.
pub const RECORD_FILE: &str = ".tamias-store.json";

/// What an install records — the provenance `list`/`info`/`verify`
/// report on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreRecord {
    /// Bare package name (from the payload manifest).
    pub name: String,
    /// Scope, when the package is namespaced (`@scope/name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Author-declared version, when the manifest carries one.
    pub version: Option<String>,
    /// Content type (`peregrin-bundle`, `myco-config`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The exact source ref the install was invoked with.
    pub source_ref: String,
    /// What the source resolved to — commit sha or `sha256:<hex>`.
    pub resolved: String,
    /// `sha256:<hex>` of the unpacked payload — `verify` recomputes it.
    pub digest: String,
    /// Unix seconds at install time.
    pub installed_at: u64,
}

/// A store entry — dir plus its parsed record.
#[derive(Clone, Debug)]
pub struct StoreEntry {
    /// `<root>/@scope/name@<label>` on disk.
    pub dir: PathBuf,
    /// Parsed [`RECORD_FILE`].
    pub record: StoreRecord,
}

impl StoreEntry {
    /// `@scope/name@version`, or the sha short form when unversioned.
    #[must_use]
    pub fn label(&self) -> String {
        let scoped = match &self.record.scope {
            Some(scope) => format!("@{scope}/{}", self.record.name),
            None => self.record.name.clone(),
        };
        format!("{}@{}", scoped, version_label(&self.record))
    }

    /// Total payload bytes.
    ///
    /// # Errors
    /// Propagates I/O failures while walking the payload.
    pub fn size(&self) -> Result<u64, Error> {
        dir_size(&self.dir)
    }

    /// The payload manifest.
    ///
    /// # Errors
    /// Fails when no manifest is present.
    pub fn manifest(&self) -> Result<Manifest, Error> {
        Manifest::load(&self.dir)
    }
}

/// `version` when declared, else the resolved sha's short form.
fn version_label(record: &StoreRecord) -> String {
    record.version.clone().unwrap_or_else(|| {
        let sha = record
            .resolved
            .strip_prefix("sha256:")
            .unwrap_or(&record.resolved);
        sha.chars().take(12).collect()
    })
}

/// Depth-first payload copy — `.git/` dirs and the root [`RECORD_FILE`]
/// are skipped.
///
/// # Errors
/// Propagates I/O failures.
pub fn copy_tree(src: &Path, dest: &Path) -> Result<(), Error> {
    for rel in crate::digest::payload_files(src)? {
        let from = src.join(&rel);
        let to = dest.join(&rel);
        let parent = to.parent().unwrap_or(dest);
        std::fs::create_dir_all(parent)
            .map_err(Error::io(format!("create {}", parent.display())))?;
        std::fs::copy(&from, &to).map_err(Error::io(format!(
            "copy {} → {}",
            from.display(),
            to.display()
        )))?;
    }
    Ok(())
}
