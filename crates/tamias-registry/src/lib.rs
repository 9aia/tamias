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

/// The result of [`Registry::publish`] — the new index entry plus the
/// older versions the retention policy pruned from the index (P-024).
#[derive(Clone, Debug)]
pub struct PublishOutcome {
    /// The published index entry.
    pub entry: RegistryEntry,
    /// `label()`s of index entries the retention policy removed —
    /// empty when retention is unset/disabled. Payload blobs are never
    /// pruned (index-entry prune only, v1): installed copies still
    /// verify, and the `.tgz` stays fetchable by direct URL — but the
    /// version no longer resolves through the index (`search`,
    /// `cf:<name>@<version>` installs).
    pub pruned: Vec<String>,
}

impl std::ops::Deref for PublishOutcome {
    type Target = RegistryEntry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

/// Apply a retention policy to `entries` after `published` joined the
/// index — the pure core both registry adapters share (P-024).
///
/// Only the published package's `(scope, name)` group is touched.
/// Within it, versions that parse as semver order descending —
/// prereleases count in the same ordering (`1.0.0-rc.1` sorts below
/// `1.0.0`), publish order is irrelevant — and only the `keep` most
/// recent stay indexed, plus `published` itself (never pruned, even
/// when it sorts outside the window — e.g. re-publishing an old
/// version). Entries with no version or a non-semver version can't be
/// ordered, so they're always kept and don't count toward `keep`.
/// `keep = 0` keeps only the published version. Returns the labels of
/// the pruned entries, sorted for stable reporting.
#[must_use]
pub fn apply_retention(
    entries: &mut Vec<RegistryEntry>,
    keep: usize,
    published: &RegistryEntry,
) -> Vec<String> {
    let same_pkg = |e: &RegistryEntry| e.name == published.name && e.scope == published.scope;
    // Orderable = same package + semver-parseable version. The
    // published entry's position is its dedup key (scope, name,
    // version) — unique within the index.
    let mut orderable: Vec<(usize, semver::Version, &str)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| same_pkg(e))
        .filter_map(|(i, e)| {
            let v = e.version.as_deref()?;
            semver::Version::parse(v).ok().map(|sv| (i, sv, v))
        })
        .collect();
    if orderable.is_empty() {
        return Vec::new();
    }
    // Newest first; the version string breaks semver ties (build
    // metadata doesn't participate in `Version::cmp`).
    orderable.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(a.2)));
    let orderable_idx: std::collections::HashSet<usize> =
        orderable.iter().map(|(i, _, _)| *i).collect();
    let mut retained: std::collections::HashSet<usize> =
        orderable.iter().take(keep).map(|(i, _, _)| *i).collect();
    // The published version is never pruned — force-retain it even
    // when it sorts outside the top-`keep` window.
    if let Some(pos) = entries
        .iter()
        .position(|e| same_pkg(e) && e.version == published.version && e.file == published.file)
    {
        retained.insert(pos);
    }
    let mut pruned: Vec<String> = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| orderable_idx.contains(i) && !retained.contains(i))
        .map(|(_, e)| e.label())
        .collect();
    pruned.sort();
    if pruned.is_empty() {
        return pruned;
    }
    let drop: std::collections::HashSet<usize> =
        orderable_idx.difference(&retained).copied().collect();
    let mut i = 0usize;
    entries.retain(|_| {
        let keep_it = !drop.contains(&i);
        i += 1;
        keep_it
    });
    pruned
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
    ) -> Result<PublishOutcome, Error>;

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

    fn entry(name: &str, scope: Option<&str>, version: Option<&str>) -> RegistryEntry {
        let scoped = match scope {
            Some(s) => format!("@{s}/{name}"),
            None => name.to_string(),
        };
        RegistryEntry {
            name: name.into(),
            scope: scope.map(str::to_string),
            version: version.map(str::to_string),
            kind: None,
            digest: "sha256:d".into(),
            signature: None,
            publisher: None,
            file: match version {
                Some(v) => format!("{scoped}/{v}.tgz"),
                None => format!("{scoped}/0.0.0.tgz"),
            },
        }
    }

    fn versions(entries: &[RegistryEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| e.version.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn retention_keeps_newest_semver_versions() {
        // publish order ≠ semver order: 1.0.0 was published last but
        // 2.0.0 is newer.
        let mut entries = vec![
            entry("n", None, Some("1.0.0")),
            entry("n", None, Some("2.0.0")),
            entry("n", None, Some("0.9.0")),
            entry("n", None, Some("1.5.0")),
        ];
        let published = entries[1].clone(); // 2.0.0
        let pruned = apply_retention(&mut entries, 2, &published);
        assert_eq!(versions(&entries), vec!["2.0.0", "1.5.0"]);
        assert_eq!(pruned, vec!["n@0.9.0", "n@1.0.0"]);
    }

    #[test]
    fn retention_counts_prereleases_in_semver_order() {
        let mut entries = vec![
            entry("n", None, Some("1.0.0")),
            entry("n", None, Some("2.0.0-rc.1")),
            entry("n", None, Some("1.0.0-rc.2")),
        ];
        let published = entries[0].clone();
        // keep=2 → 1.0.0 + 2.0.0-rc.1 (prerelease of a *newer* line
        // still outranks 1.0.0-rc.2)
        let pruned = apply_retention(&mut entries, 2, &published);
        assert_eq!(versions(&entries), vec!["1.0.0", "2.0.0-rc.1"]);
        assert_eq!(pruned, vec!["n@1.0.0-rc.2"]);
    }

    #[test]
    fn retention_keep_zero_keeps_only_published() {
        let mut entries = vec![
            entry("n", None, Some("1.0.0")),
            entry("n", None, Some("2.0.0")),
            entry("n", None, Some("3.0.0")),
        ];
        let published = entries[2].clone();
        let pruned = apply_retention(&mut entries, 0, &published);
        assert_eq!(versions(&entries), vec!["3.0.0"]);
        assert_eq!(pruned, vec!["n@1.0.0", "n@2.0.0"]);
    }

    #[test]
    fn retention_never_prunes_published_old_version() {
        // Re-publishing 0.5.0 when 3.0.0 exists: keep=1 would normally
        // retain only 3.0.0, but the published entry is force-kept.
        let mut entries = vec![
            entry("n", None, Some("0.5.0")),
            entry("n", None, Some("2.0.0")),
            entry("n", None, Some("3.0.0")),
        ];
        let published = entries[0].clone();
        let pruned = apply_retention(&mut entries, 1, &published);
        assert_eq!(versions(&entries), vec!["0.5.0", "3.0.0"]);
        assert_eq!(pruned, vec!["n@2.0.0"]);
    }

    #[test]
    fn retention_protects_unorderable_versions() {
        let mut entries = vec![
            entry("n", None, Some("latest")), // dist-tag, not semver
            entry("n", None, None),           // versionless
            entry("n", None, Some("1.0.0")),
            entry("n", None, Some("2.0.0")),
        ];
        let published = entries[3].clone();
        let pruned = apply_retention(&mut entries, 1, &published);
        // only 1.0.0 pruned — unorderable entries always survive and
        // don't consume keep slots
        assert_eq!(entries.len(), 3);
        assert!(entries
            .iter()
            .any(|e| e.version.as_deref() == Some("latest")));
        assert!(entries.iter().any(|e| e.version.is_none()));
        assert_eq!(pruned, vec!["n@1.0.0"]);
    }

    #[test]
    fn retention_scopes_to_published_package() {
        let mut entries = vec![
            entry("a", None, Some("1.0.0")),
            entry("a", None, Some("2.0.0")),
            entry("b", Some("s"), Some("1.0.0")),
            entry("b", Some("s"), Some("2.0.0")),
            entry("b", None, Some("9.9.9")), // same name, different scope
        ];
        let published = entries[1].clone(); // a@2.0.0
        let pruned = apply_retention(&mut entries, 1, &published);
        assert_eq!(pruned, vec!["a@1.0.0"]);
        assert_eq!(entries.len(), 4);
        // scoped/unscoped same-name groups are independent
        assert!(entries.iter().any(|e| e.label() == "@s/b@1.0.0"));
        assert!(entries.iter().any(|e| e.label() == "b@9.9.9"));
    }

    #[test]
    fn retention_noop_when_within_keep() {
        let mut entries = vec![
            entry("n", None, Some("1.0.0")),
            entry("n", None, Some("2.0.0")),
        ];
        let published = entries[1].clone();
        assert!(apply_retention(&mut entries, 5, &published).is_empty());
        assert_eq!(entries.len(), 2);
    }
}
