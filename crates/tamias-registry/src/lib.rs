//! tamias-registry — the registry port: an index of published packages plus
//! publish. Adapters: `-local` (a directory, optionally served over HTTP)
//! and `-cf` (a Cloudflare Worker + R2).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tamias_core::{Error, Manifest};

/// One indexed package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// Bare name.
    pub name: String,
    /// Scope, when namespaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Declared version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Content type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `sha256:<hex>` of the packed payload.
    pub digest: String,
    /// Path to the `.tgz`, relative to the registry root.
    pub file: String,
}

impl RegistryEntry {
    /// `@scope/name@version` (or name-only fallbacks).
    #[must_use]
    pub fn label(&self) -> String {
        let scoped = match &self.scope {
            Some(scope) => format!("@{scope}/{}", self.name),
            None => self.name.clone(),
        };
        match &self.version {
            Some(v) => format!("{scoped}@{v}"),
            None => scoped,
        }
    }
}

/// The registry port.
pub trait Registry {
    /// Every indexed entry.
    ///
    /// # Errors
    /// Adapter-specific.
    fn entries(&self) -> Result<Vec<RegistryEntry>, Error>;

    /// Pack + index a payload.
    ///
    /// # Errors
    /// Adapter-specific.
    fn publish(&self, payload_root: &Path, manifest: &Manifest) -> Result<RegistryEntry, Error>;

    /// Entries whose name/scope contains `query` (case-insensitive).
    ///
    /// # Errors
    /// Adapter-specific (delegates to [`Registry::entries`]).
    fn search(&self, query: &str) -> Result<Vec<RegistryEntry>, Error> {
        let q = query.to_lowercase();
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&q)
                    || e.scope.as_deref().unwrap_or("").to_lowercase().contains(&q)
            })
            .collect())
    }
}

/// A registry ref — a local dir, or an http(s) base URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryRef {
    /// A directory path.
    Local(PathBuf),
    /// An http(s) base URL.
    Http(String),
}

/// Parse a `--registry` value into a [`RegistryRef`].
#[must_use]
pub fn parse_registry_ref(value: &str) -> RegistryRef {
    if value.starts_with("http://") || value.starts_with("https://") {
        RegistryRef::Http(value.trim_end_matches('/').to_string())
    } else {
        RegistryRef::Local(PathBuf::from(value))
    }
}
