//! tamias-content — the content-type port: a package's `type` decides
//! *where* and *how* it installs, beyond landing in the tamias store.
//!
//! The engine is generic — it moves bytes + a manifest and unpacks into
//! the store. A [`ContentType`] adds the product-specific placement: a
//! `peregrin-bundle` also mirrors into peregrin's store; a `files` pack
//! needs nothing beyond the store. Data-first: a type may be *declared*
//! (a manifest recipe) with an optional provider binary — the engine never
//! links a product crate.

use std::path::{Path, PathBuf};

use tamias_core::{copy_tree, Error, StoreEntry};

/// What a content type is given when it installs.
pub struct InstallCtx<'a> {
    /// The unpacked payload's root (holds the manifest).
    pub payload_root: &'a Path,
    /// The ref the install was invoked with.
    pub reference: &'a str,
    /// The store entry the engine already created.
    pub entry: &'a StoreEntry,
}

/// What a content type did — human-readable action lines.
#[derive(Debug, Default)]
pub struct Report {
    /// One line per placement (`peregrin store: /…`).
    pub actions: Vec<String>,
}

/// A content type — extra placement beyond the tamias store.
pub trait ContentType: Send + Sync {
    /// The `type:` value this handles.
    fn name(&self) -> &'static str;

    /// Additional placement for this kind.
    ///
    /// # Errors
    /// Propagates I/O failures placing the payload.
    fn install(&self, ctx: &InstallCtx) -> Result<Report, Error>;
}

/// The escape hatch — the store install already satisfied it.
pub struct Files;

impl ContentType for Files {
    fn name(&self) -> &'static str {
        "files"
    }
    fn install(&self, _ctx: &InstallCtx) -> Result<Report, Error> {
        Ok(Report::default())
    }
}

/// Peregrin workflow bundles — also mirror into peregrin's store so the
/// engine picks them up (`~/.local/share/peregrin/store/<name>@<ver>`).
pub struct PeregrinBundle;

impl ContentType for PeregrinBundle {
    fn name(&self) -> &'static str {
        "peregrin-bundle"
    }
    fn install(&self, ctx: &InstallCtx) -> Result<Report, Error> {
        let rec = &ctx.entry.record;
        let label = format!(
            "{}@{}",
            rec.name,
            rec.version.as_deref().unwrap_or("unversioned")
        );
        let dest = peregrin_store_root().join(label);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .map_err(Error::io(format!("clear {}", dest.display())))?;
        }
        copy_tree(ctx.payload_root, &dest)?;
        Ok(Report {
            actions: vec![format!("peregrin store: {}", dest.display())],
        })
    }
}

/// Registered content types.
#[must_use]
pub fn registry() -> Vec<Box<dyn ContentType>> {
    vec![Box::new(Files), Box::new(PeregrinBundle)]
}

/// Look up a content type by its `type:` name.
#[must_use]
pub fn by_name(name: &str) -> Option<Box<dyn ContentType>> {
    registry().into_iter().find(|c| c.name() == name)
}

/// `$PEREGRIN_HOME` → `$XDG_DATA_HOME/peregrin` → `~/.local/share/peregrin`,
/// then `store`.
fn peregrin_store_root() -> PathBuf {
    std::env::var_os("PEREGRIN_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("peregrin")))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/peregrin"))
        })
        .unwrap_or_else(|| PathBuf::from(".peregrin"))
        .join("store")
}
