//! Store pruning — the pure half of `tamias prune`.
//!
//! The store keeps every version ever installed (`name@old` alongside
//! `name@new`); old versions are legitimate rollback targets, so
//! pruning is always an explicit operator act. [`plan_prune`] decides
//! *what* is superseded — semver-max per `(scope, name)` group — and
//! the surfaces (CLI, TUI, serve) decide *how* removals run through
//! the same uninstall path `remove`/`uninstall` use.
//!
//! Conservative by construction: entries whose version isn't valid
//! semver (unversioned digest labels, hand-edited records, odd
//! version strings) are never pruned and never count as the "latest"
//! to keep — they land in [`PrunePlan::skipped`] with the reason.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use semver::Version;

use crate::StoreEntry;

/// Semver *precedence* — major/minor/patch/pre, **ignoring build
/// metadata** (spec §10). `Version::cmp` itself orders build
/// metadata, which would let `1.0.0+build.2` evict `1.0.0` as
/// "newer"; for pruning, same-precedence builds are one version and
/// are kept or dropped together.
fn precedence_cmp(a: &Version, b: &Version) -> Ordering {
    a.major
        .cmp(&b.major)
        .then_with(|| a.minor.cmp(&b.minor))
        .then_with(|| a.patch.cmp(&b.patch))
        .then_with(|| a.pre.cmp(&b.pre))
}

/// One entry [`plan_prune`] marked for removal, with the labels that
/// survive in its `(scope, name)` group — the "keeping …" half of the
/// report.
#[derive(Clone, Debug)]
pub struct PruneRemoval {
    /// The superseded store entry.
    pub entry: StoreEntry,
    /// Labels kept for the same package — the rollback targets.
    pub kept: Vec<String>,
}

/// An entry pruning refused to judge — reported, never removed.
#[derive(Clone, Debug)]
pub struct PruneSkip {
    /// The entry's `@scope/name@label`.
    pub label: String,
    /// Why it was skipped (`unversioned`, `non-semver version '…'`).
    pub reason: String,
}

/// What [`plan_prune`] decided over a store snapshot.
#[derive(Clone, Debug, Default)]
pub struct PrunePlan {
    /// Superseded entries to remove, sorted oldest-first within each
    /// `(scope, name)` group; groups in `(scope, name)` order.
    pub removals: Vec<PruneRemoval>,
    /// Entries skipped as ambiguous — only reported for multi-entry
    /// groups (a lone unversioned install is a no-op, not a warning).
    pub skipped: Vec<PruneSkip>,
}

/// Group store entries by `(scope, name)` and pick the superseded
/// ones: within each group the newest `keep` distinct semver
/// *precedences* survive (all entries sharing a kept precedence are
/// kept — build-metadata ties can't split a version), everything
/// older is a [`PruneRemoval`]. Groups with a single entry are
/// no-ops; entries without a parseable semver `version:` are
/// [`PruneSkip`]ped, never removed.
///
/// `keep` is clamped to ≥1 — keeping zero versions would delete the
/// package outright, which is `remove`/`uninstall`'s job, not prune's.
#[must_use]
pub fn plan_prune(entries: &[StoreEntry], keep: usize) -> PrunePlan {
    let keep = keep.max(1);
    let mut groups: BTreeMap<(Option<String>, String), Vec<&StoreEntry>> = BTreeMap::new();
    for entry in entries {
        groups
            .entry((entry.record.scope.clone(), entry.record.name.clone()))
            .or_default()
            .push(entry);
    }
    let mut plan = PrunePlan::default();
    for (_key, group) in groups {
        if group.len() < 2 {
            continue;
        }
        // Split the group into semver-versioned entries (prune
        // candidates) and ambiguous ones (reported, kept).
        let mut versioned: Vec<(Version, &StoreEntry)> = Vec::new();
        for entry in &group {
            match &entry.record.version {
                Some(v) => match Version::parse(v) {
                    Ok(parsed) => versioned.push((parsed, *entry)),
                    Err(_) => plan.skipped.push(PruneSkip {
                        label: entry.label(),
                        reason: format!("version '{v}' is not semver"),
                    }),
                },
                None => plan.skipped.push(PruneSkip {
                    label: entry.label(),
                    reason: "unversioned entry (digest label)".to_string(),
                }),
            }
        }
        // Newest precedence first; label breaks ties so output is
        // deterministic. Entries sharing a precedence (build-metadata
        // variants like 1.0.0+a / 1.0.0+b) stay adjacent.
        versioned.sort_by(|(va, a), (vb, b)| {
            precedence_cmp(vb, va).then_with(|| a.label().cmp(&b.label()))
        });
        // The `keep` newest *distinct* precedences survive.
        let mut kept_versions: Vec<Version> = Vec::new();
        for (v, _) in &versioned {
            if kept_versions.len() >= keep {
                break;
            }
            if !kept_versions
                .iter()
                .any(|k| precedence_cmp(k, v) == Ordering::Equal)
            {
                kept_versions.push(v.clone());
            }
        }
        let is_kept = |v: &Version| {
            kept_versions
                .iter()
                .any(|k| precedence_cmp(k, v) == Ordering::Equal)
        };
        let kept: Vec<String> = versioned
            .iter()
            .filter(|(v, _)| is_kept(v))
            .map(|(_, e)| e.label())
            .collect();
        // Removals oldest-first for a readable report.
        let mut doomed: Vec<&StoreEntry> = versioned
            .iter()
            .filter(|(v, _)| !is_kept(v))
            .map(|(_, e)| *e)
            .collect();
        doomed.sort_by_key(|e| e.label());
        for entry in doomed {
            plan.removals.push(PruneRemoval {
                entry: entry.clone(),
                kept: kept.clone(),
            });
        }
    }
    plan
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::StoreRecord;
    use std::path::PathBuf;

    fn entry(name: &str, scope: Option<&str>, version: Option<&str>) -> StoreEntry {
        StoreEntry {
            dir: PathBuf::from(format!("/store/{name}@{}", version.unwrap_or("x"))),
            record: StoreRecord {
                name: name.into(),
                scope: scope.map(str::to_string),
                version: version.map(str::to_string),
                kind: None,
                description: None,
                keywords: Vec::new(),
                homepage: None,
                repository: None,
                license: None,
                authors: Vec::new(),
                source_ref: "./x".into(),
                resolved: "sha256:abc".into(),
                digest: String::new(),
                signature: None,
                signer: None,
                trust: None,
                installed_at: 0,
            },
        }
    }

    fn labels(plan: &PrunePlan) -> Vec<String> {
        plan.removals.iter().map(|r| r.entry.label()).collect()
    }

    #[test]
    fn keeps_semver_max_prunes_the_rest() {
        let entries = vec![
            entry("w", None, Some("0.3.2")),
            entry("w", None, Some("0.3.5")),
            entry("w", None, Some("0.3.7")),
        ];
        let plan = plan_prune(&entries, 1);
        assert_eq!(labels(&plan), ["w@0.3.2", "w@0.3.5"]);
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.removals[0].kept, ["w@0.3.7"]);
        assert_eq!(plan.removals[1].kept, ["w@0.3.7"]);
    }

    #[test]
    fn keep_n_retains_newest_n_versions() {
        let entries = vec![
            entry("w", None, Some("1.0.0")),
            entry("w", None, Some("2.0.0")),
            entry("w", None, Some("3.0.0")),
        ];
        let plan = plan_prune(&entries, 2);
        assert_eq!(labels(&plan), ["w@1.0.0"]);
        assert_eq!(plan.removals[0].kept, ["w@3.0.0", "w@2.0.0"]);
        // keep >= group size is a no-op.
        assert!(plan_prune(&entries, 3).removals.is_empty());
        // keep clamps to ≥1 — 0 still keeps the latest.
        assert_eq!(labels(&plan_prune(&entries, 0)), ["w@1.0.0", "w@2.0.0"]);
    }

    #[test]
    fn single_version_and_single_entry_groups_are_noops() {
        assert!(plan_prune(&[entry("w", None, Some("1.0.0"))], 1)
            .removals
            .is_empty());
        // Two *different* packages never prune each other.
        let entries = vec![
            entry("a", None, Some("1.0.0")),
            entry("b", None, Some("2.0.0")),
        ];
        assert!(plan_prune(&entries, 1).removals.is_empty());
    }

    #[test]
    fn scopes_group_independently() {
        let entries = vec![
            entry("w", Some("acme"), Some("1.0.0")),
            entry("w", Some("acme"), Some("2.0.0")),
            entry("w", None, Some("9.9.9")),
        ];
        let plan = plan_prune(&entries, 1);
        assert_eq!(labels(&plan), ["@acme/w@1.0.0"]);
        assert_eq!(plan.removals[0].kept, ["@acme/w@2.0.0"]);
    }

    #[test]
    fn non_semver_and_unversioned_entries_are_skipped_not_removed() {
        let entries = vec![
            entry("w", None, Some("1.0.0")),
            entry("w", None, Some("2.0.0")),
            entry("w", None, Some("not-a-version")),
            entry("w", None, None),
        ];
        let plan = plan_prune(&entries, 1);
        // 1.0.0 is superseded by 2.0.0; the non-semver pair is skipped.
        assert_eq!(labels(&plan), ["w@1.0.0"]);
        assert_eq!(plan.skipped.len(), 2);
        assert!(plan.skipped.iter().any(|s| s.label == "w@not-a-version"));
        assert!(plan
            .skipped
            .iter()
            .any(|s| s.reason.contains("unversioned")));
    }

    #[test]
    fn lone_non_semver_entry_is_silent() {
        // A single-entry group is a no-op — no skip noise either.
        let plan = plan_prune(&[entry("w", None, Some("weird"))], 1);
        assert!(plan.removals.is_empty() && plan.skipped.is_empty());
    }

    #[test]
    fn prerelease_and_build_metadata_ordering() {
        let entries = vec![
            entry("w", None, Some("1.0.0")),
            entry("w", None, Some("1.0.0-rc.1")),
            entry("w", None, Some("1.0.0+build.2")),
            entry("w", None, Some("1.0.0+build.1")),
        ];
        let plan = plan_prune(&entries, 1);
        // 1.0.0-* precedes 1.0.0; the two build-metadata variants share
        // one precedence and are both kept with it.
        assert_eq!(labels(&plan), ["w@1.0.0-rc.1"]);
        assert_eq!(plan.removals[0].kept.len(), 3);
    }

    #[test]
    fn skipped_alongside_real_prunes() {
        let entries = vec![
            entry("w", None, Some("0.1.0")),
            entry("w", None, Some("0.2.0")),
            entry("w", None, None),
        ];
        let plan = plan_prune(&entries, 1);
        assert_eq!(labels(&plan), ["w@0.1.0"]);
        assert_eq!(plan.skipped.len(), 1);
    }
}
