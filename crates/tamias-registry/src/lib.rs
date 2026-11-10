#![cfg_attr(coverage, feature(coverage_attribute))]
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
    /// Armored `ssh-keygen -Y` signature over the provenance anchors
    /// (see `tamias-sign`) — written by `publish --sign`, checked by
    /// `cf:` installs and `tamias verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Publisher display identity carried by the index (P-005) —
    /// display metadata only; nothing populates it yet (no publisher
    /// identity exists), but the schema round-trips it through the
    /// local and `cf` registries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
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

    /// Pack + index a payload. `signature` is the armored
    /// `ssh-keygen -Y` signature over the payload's provenance anchors
    /// (`tamias-sign`), stored on the index entry when `Some`.
    ///
    /// # Errors
    /// Adapter-specific.
    fn publish(
        &self,
        payload_root: &Path,
        manifest: &Manifest,
        signature: Option<&str>,
    ) -> Result<RegistryEntry, Error>;

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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use super::*;

    #[test]
    fn label_and_ref_parse() {
        let entry = RegistryEntry {
            name: "n".into(),
            scope: None,
            version: None,
            kind: None,
            digest: "sha256:d".into(),
            signature: None,
            publisher: None,
            file: "n/0.0.0.tgz".into(),
        };
        assert_eq!(entry.label(), "n");
        let scoped = RegistryEntry {
            scope: Some("s".into()),
            version: Some("1.0.0".into()),
            ..entry
        };
        assert_eq!(scoped.label(), "@s/n@1.0.0");
        assert_eq!(
            parse_registry_ref("./x"),
            RegistryRef::Local(PathBuf::from("./x"))
        );
        assert_eq!(
            parse_registry_ref("http://h/x/"),
            RegistryRef::Http("http://h/x".into())
        );
    }

    #[test]
    fn publisher_round_trips_through_index_json() {
        let entry = RegistryEntry {
            name: "n".into(),
            scope: Some("acme".into()),
            version: Some("1.0.0".into()),
            kind: None,
            digest: "sha256:d".into(),
            signature: None,
            publisher: Some("@acme".into()),
            file: "acme/n/1.0.0.tgz".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"publisher\":\"@acme\""), "{json}");
        let back: RegistryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
        // absent → None, and skipped on serialize
        let bare = RegistryEntry {
            publisher: None,
            ..entry
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("publisher"), "{json}");
        let back: RegistryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.publisher, None);
    }
}
