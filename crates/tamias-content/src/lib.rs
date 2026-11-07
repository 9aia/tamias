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

/// myco project-management configs — kinds/views/templates. Placed under
/// the myco packs dir so `myco` can pick them up.
pub struct MycoConfig;

impl ContentType for MycoConfig {
    fn name(&self) -> &'static str {
        "myco-config"
    }
    fn install(&self, ctx: &InstallCtx) -> Result<Report, Error> {
        let dest = myco_packs_root().join(&ctx.entry.record.name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .map_err(Error::io(format!("clear {}", dest.display())))?;
        }
        copy_tree(ctx.payload_root, &dest)?;
        Ok(Report {
            actions: vec![format!("myco pack: {}", dest.display())],
        })
    }
}

/// `$MYCO_HOME` → `$XDG_CONFIG_HOME/myco` → `~/.config/myco`, then `packs`.
fn myco_packs_root() -> PathBuf {
    std::env::var_os("MYCO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|d| PathBuf::from(d).join("myco")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/myco")))
        .unwrap_or_else(|| PathBuf::from(".myco"))
        .join("packs")
}

/// kuda domain/language packs — placed under kuda's domains dir.
pub struct KudaDomain;

impl ContentType for KudaDomain {
    fn name(&self) -> &'static str {
        "kuda-domain"
    }
    fn install(&self, ctx: &InstallCtx) -> Result<Report, Error> {
        let dest = config_root("KUDA_HOME", "kuda")
            .join("domains")
            .join(&ctx.entry.record.name);
        place_pack(ctx.payload_root, &dest)
    }
}

/// mimus rule packs — placed under mimus's rules dir.
pub struct MimusRules;

impl ContentType for MimusRules {
    fn name(&self) -> &'static str {
        "mimus-rules"
    }
    fn install(&self, ctx: &InstallCtx) -> Result<Report, Error> {
        let dest = config_root("MIMUS_HOME", "mimus")
            .join("rules")
            .join(&ctx.entry.record.name);
        place_pack(ctx.payload_root, &dest)
    }
}

/// Copy `payload_root` into `dest` (replacing it), reporting the placement.
fn place_pack(payload_root: &Path, dest: &Path) -> Result<Report, Error> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(Error::io(format!("clear {}", dest.display())))?;
    }
    copy_tree(payload_root, dest)?;
    Ok(Report {
        actions: vec![format!("placed: {}", dest.display())],
    })
}

/// `<env_var>` → `$XDG_CONFIG_HOME/<app>` → `~/.config/<app>` → `./.<app>`.
fn config_root(env_var: &str, app: &str) -> PathBuf {
    std::env::var_os(env_var)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|d| PathBuf::from(d).join(app)))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(format!(".config/{app}")))
        })
        .unwrap_or_else(|| PathBuf::from(format!(".{app}")))
}

/// Registered content types.
#[must_use]
pub fn registry() -> Vec<Box<dyn ContentType>> {
    vec![
        Box::new(Files),
        Box::new(PeregrinBundle),
        Box::new(MycoConfig),
        Box::new(KudaDomain),
        Box::new(MimusRules),
    ]
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
