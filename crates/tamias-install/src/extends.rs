//! `template.extends:` parent materialization — the install half of
//! template-pack composition (scarab's `type: template` driver).
//!
//! A pack manifest may declare `template.extends:` inside its
//! driver-owned `template:` table. The vocabulary is the agnostic
//! composition spec (scarab's `scarab-template/COMPOSITION.md`):
//! `extends` is a source string, a `{source, via}` table, or a list
//! of either — tamias interprets *only* the `tamias-ref` scheme
//! (absent `via` = the ambient default, which under tamias is
//! `tamias-ref`); any other `via` fails the install. `source` stays
//! an opaque locator — store name or installable ref.
//!
//! At install, after `include:` composition and the signature/consent
//! gates, each parent's payload is resolved (store name → that
//! entry's committed payload; anything else the ref grammar claims →
//! a recursive [`crate::install_inner`], so the parent gets the full
//! pipeline — fetch, *its own* extends, drivers, store commit) and
//! copied into the composed tree: a single parent lands at
//! [`PARENT_DIR`] (`.scarab-parent/`, the v1 layout); several parents
//! land at [`PARENTS_DIR`]/`<index>/` (`.scarab-parents/0/`, `1/`, …)
//! in declaration order — lowest precedence first.
//!
//! Because each parent's committed payload already carries *its*
//! materialized parents (`.scarab-parent*/` inside it), nesting is
//! automatic: a grandparent lands inside its parent's dir. The store
//! commit therefore carries the whole ancestor chain, and every
//! later driver verb — `install` (this dispatch), `deploy`,
//! `uninstall` — sees it under `--dir <payload>` with no extra work.
//!
//! Recursion is bounded like `include:` ([`IncludeState`]): a depth
//! cap ([`MAX_EXTENDS_DEPTH`]) and an in-progress label set
//! ([`IncludeState::extends_in_progress`]) that catches
//! `A extends B, B extends A` cycles through refs. Store-name
//! parents can't cycle — their payloads are already committed and
//! finite. Sibling parents resolve sequentially.
//!
//! The signature/consent/trust digest deliberately covers the tree
//! *before* `.scarab-parent*/` is added: publish doesn't flatten
//! `extends`, so a signed parent-extending pack still verifies
//! against its published bytes, and per-package driver consent binds
//! to the package's own payload. `Store::install` digests the final
//! tree (parents included), so `verify` stays honest.

use std::path::{Path, PathBuf};

use tamias_core::{copy_tree, find_store_entry, Error};
use tamias_source::expand_path;
use tamias_source_fs::FsSpec;
use tamias_store_local::Store;

use crate::include::{self, IncludeState};
use crate::{install_inner, Env, Progress, Surface};

/// Extends-recursion cap — an `extends` chain deeper than this fails
/// instead of recursing forever (ref cycles are caught earlier by
/// [`IncludeState::extends_in_progress`]; the cap bounds pathological
/// DAGs). Same bound as [`include::MAX_INCLUDE_DEPTH`] and scarab's
/// `DEFAULT_MAX_DEPTH`.
pub(crate) const MAX_EXTENDS_DEPTH: usize = 8;

/// Payload subdir a single resolved parent materializes into — the
/// driver (`tamias-driver-template`) discovers it by this name.
pub(crate) const PARENT_DIR: &str = ".scarab-parent";

/// Payload subdir several resolved parents materialize into — one
/// numbered child (`0/`, `1/`, …) per `extends` declaration, in
/// declaration order (lowest precedence first).
pub(crate) const PARENTS_DIR: &str = ".scarab-parents";

/// `via` schemes tamias can resolve — absent `via` means the ambient
/// default, which under a tamias install is `tamias-ref`.
const TAMIAS_VIA: &[&str] = &["tamias-ref", "tamias"];

/// One declared `extends` parent after spec parsing — `source` is the
/// opaque locator, `via` the (already validated) scheme hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtParent {
    /// The opaque source locator (store name or installable ref).
    pub source: String,
    /// The resolution-scheme hint — always a [`TAMIAS_VIA`] value or
    /// `None` (ambient default).
    pub via: Option<String>,
}

/// `template.extends` parents declared by the manifest inside `dir` —
/// empty when absent or JSON-null.
///
/// The `template:` table is driver-owned: tamias re-reads the raw
/// manifest file (the typed [`tamias_core::Manifest`] drops unknown
/// keys) and validates only this one field, per the agnostic
/// composition spec (scarab `COMPOSITION.md`):
///
/// - `"<source>"` — one parent, bare string.
/// - `{"source": "<source>", "via": "<scheme>"}` — one structured
///   parent (`via` optional; must be a tamias scheme).
/// - `[<string-or-table>, …]` — several parents, lowest precedence
///   first. An empty list is an error.
///
/// Anything else — non-string sources, empty sources, unknown `via`
/// schemes — fails.
///
/// # Errors
/// Fails on manifest read/parse errors or a malformed `extends`.
pub(crate) fn parents(dir: &Path) -> Result<Vec<ExtParent>, Error> {
    let Some(path) = tamias_core::Manifest::path_in(dir) else {
        return Ok(Vec::new());
    };
    let text =
        std::fs::read_to_string(&path).map_err(Error::io(format!("read {}", path.display())))?;
    // Same syntax dispatch as `Manifest::parse_at`: .yaml/.yml → YAML,
    // everything else → TOML. Both normalize to JSON for the lookup.
    let value: serde_json::Value = if matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml" | "yml")
    ) {
        let yaml: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))?;
        serde_json::to_value(yaml)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))?
    } else {
        let table: toml::Table = toml::from_str(&text)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))?;
        serde_json::to_value(table)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))?
    };
    let Some(extends) = value.get("template").and_then(|t| t.get("extends")) else {
        return Ok(Vec::new());
    };
    if extends.is_null() {
        return Ok(Vec::new());
    }
    let items: Vec<serde_json::Value> = match extends {
        serde_json::Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    if items.is_empty() {
        return Err(Error::Failed(format!(
            "{}: template.extends is an empty list — name at least one parent",
            path.display()
        )));
    }
    items.iter().map(|item| parse_parent(item, &path)).collect()
}

/// One `extends` item (string or `{source, via}` map) → [`ExtParent`].
fn parse_parent(item: &serde_json::Value, path: &Path) -> Result<ExtParent, Error> {
    let bad = |why: &str| Error::Failed(format!("{}: template.extends {why}", path.display()));
    let (source, via) = match item {
        serde_json::Value::String(s) => (s.clone(), None),
        serde_json::Value::Object(map) => {
            let source = map.get("source").and_then(|v| v.as_str()).ok_or_else(|| {
                bad("parent must be a string or {source, via} with a string source")
            })?;
            let via = match map.get("via") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(bad("parent 'via' must be a string")),
            };
            (source.to_string(), via)
        }
        _ => {
            return Err(bad(
                "parent must be a string or {source, via} with a string source",
            ));
        }
    };
    if source.trim().is_empty() {
        return Err(bad("parent source must be a non-empty string"));
    }
    if let Some(v) = &via {
        if !TAMIAS_VIA.contains(&v.as_str()) {
            return Err(Error::Failed(format!(
                "{}: template.extends via '{v}' is not a tamias composition source — \
                 tamias resolves '{}' only",
                path.display(),
                TAMIAS_VIA.join("'/'")
            )));
        }
    }
    Ok(ExtParent { source, via })
}

/// Resolve each declared parent to a payload dir and copy it into
/// `child_root` — `.scarab-parent/` for a single parent,
/// `.scarab-parents/<index>/` (declaration order) for several —
/// replacing any dirs already there (a declared `extends` is
/// authoritative over vendored bytes).
///
/// `child_label` is the installing package's store label
/// ([`include::pending_label`]) — the cycle-set key. `report` sees a
/// [`Progress::Including`] event per parent, and a ref-parent's
/// recursive install streams its own events like an `include:` base.
///
/// # Errors
/// Fails on an ambiguous/missing store name, an unparseable ref, a
/// cycle, a depth-cap breach, or any recursive-install/copy failure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize(
    env: &Env,
    child_root: &Path,
    child_label: &str,
    parents: &[ExtParent],
    state: &mut IncludeState,
    allow_drivers: bool,
    surface: Surface,
    report: &mut dyn FnMut(Progress),
) -> Result<(), Error> {
    let single = parents.len() == 1;
    for (i, parent) in parents.iter().enumerate() {
        let resolved = resolve_parent(
            env,
            child_label,
            &parent.source,
            state,
            allow_drivers,
            surface,
            report,
        )?;
        let dest = if single {
            child_root.join(PARENT_DIR)
        } else {
            child_root.join(PARENTS_DIR).join(i.to_string())
        };
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .map_err(Error::io(format!("clear {}", dest.display())))?;
        }
        // create_dir_all first: an empty parent payload must still
        // leave a dir the driver's locator can descend into (its
        // manifest is what the chain walk reads — a payload always
        // has one).
        std::fs::create_dir_all(&dest).map_err(Error::io(format!("create {}", dest.display())))?;
        copy_tree(&resolved, &dest)?;
    }
    Ok(())
}

/// One `extends` source → one parent payload dir. Mirrors
/// [`include::resolve_base`]'s store-name-then-ref order; see the
/// module docs for the recursion bounds.
#[allow(clippy::too_many_arguments)]
fn resolve_parent(
    env: &Env,
    child_label: &str,
    parent_ref: &str,
    state: &mut IncludeState,
    allow_drivers: bool,
    surface: Surface,
    report: &mut dyn FnMut(Progress),
) -> Result<PathBuf, Error> {
    // 1. Store name — label / `@scope/name` / bare `name`. The stored
    //    payload already carries its own `.scarab-parent*/` chain.
    let entries = Store::new(&env.store_root).entries()?;
    if let Some(entry) = find_store_entry(&entries, parent_ref)? {
        report(Progress::Including {
            base: entry.label(),
        });
        return Ok(entry.dir);
    }
    // `@scope/name` is a *name*, never a path.
    if parent_ref.starts_with('@') {
        return Err(missing_parent(parent_ref));
    }
    // 2. Installable ref — recurse through the full pipeline.
    let spec = crate::refs()
        .parse(parent_ref)
        .map_err(|_| missing_parent(parent_ref))?;
    if let Some(reason) = include::unpinned_remote_ref(&spec) {
        eprintln!("warning: template.extends '{parent_ref}' is an unpinned remote ref — {reason}");
    }
    if spec.is::<FsSpec>() && !expand_path(Path::new(parent_ref)).exists() {
        return Err(missing_parent(parent_ref));
    }
    if state.extends_in_progress.contains(child_label) {
        return Err(Error::Failed(format!(
            "template.extends cycle: {child_label} is already being installed as a parent"
        )));
    }
    if state.extends_depth >= MAX_EXTENDS_DEPTH {
        return Err(Error::Failed(format!(
            "template.extends '{parent_ref}' exceeds the {MAX_EXTENDS_DEPTH}-deep nesting cap"
        )));
    }
    report(Progress::Including {
        base: parent_ref.to_string(),
    });
    state.extends_in_progress.insert(child_label.to_string());
    state.extends_depth += 1;
    let result = install_inner(env, parent_ref, allow_drivers, surface, report, state);
    state.extends_depth -= 1;
    state.extends_in_progress.remove(child_label);
    Ok(result?.dir)
}

/// The hard error for an extends ref that is neither an installed
/// store name nor a resolvable ref.
fn missing_parent(parent_ref: &str) -> Error {
    Error::Failed(format!(
        "template.extends '{parent_ref}' is not installed — store names must already be in the \
         store; use an installable ref (path, git:, cf:, http:) or `tamias install {parent_ref}` first"
    ))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Write a manifest file with the given `template:` table body.
    fn manifest(dir: &Path, body: &str) {
        std::fs::write(
            dir.join("tamias.yaml"),
            format!("name: x\ntype: template\ntemplate:\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn parents_reads_string_table_and_list() {
        let dir = tempfile::tempdir().unwrap();
        manifest(dir.path(), "  extends: git:org/base\n");
        let p = parents(dir.path()).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].source, "git:org/base");
        assert_eq!(p[0].via, None);

        manifest(
            dir.path(),
            "  extends:\n    source: git:org/base\n    via: tamias-ref\n",
        );
        let p = parents(dir.path()).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].via.as_deref(), Some("tamias-ref"));

        manifest(
            dir.path(),
            "  extends:\n    - git:org/a\n    - source: git:org/b\n      via: tamias\n",
        );
        let p = parents(dir.path()).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].source, "git:org/a");
        assert_eq!(p[1].source, "git:org/b");

        // absent / null → empty
        manifest(dir.path(), "  files: []\n");
        assert!(parents(dir.path()).unwrap().is_empty());
        manifest(dir.path(), "  extends: null\n");
        assert!(parents(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn parents_rejects_malformed() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            "  extends: 42\n",
            "  extends: \"  \"\n",
            "  extends: []\n",
            "  extends:\n    - 42\n",
            "  extends:\n    source: git:org/x\n    via: handl-kit\n",
            "  extends:\n    source: git:org/x\n    via: 7\n",
            "  extends:\n    via: tamias-ref\n",
        ] {
            manifest(dir.path(), body);
            assert!(parents(dir.path()).is_err(), "body: {body}");
        }
    }

    #[test]
    fn parents_reads_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tamias.toml"),
            "name = \"x\"\ntype = \"template\"\n[template]\nextends = \"git:org/t\"\n",
        )
        .unwrap();
        let p = parents(dir.path()).unwrap();
        assert_eq!(p[0].source, "git:org/t");
    }
}
