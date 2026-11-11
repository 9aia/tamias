//! `template.extends:` parent materialization — the install half of
//! template-pack composition (scarab's `type: template` driver).
//!
//! A pack manifest may declare `template.extends: <ref>` inside its
//! driver-owned `template:` table. tamias never interprets the table
//! beyond this one key: at install, after `include:` composition and
//! the signature/consent gates, the parent payload is resolved
//! (store name → that entry's committed payload; anything else the
//! ref grammar claims → a recursive [`crate::install_inner`], so the
//! parent gets the full pipeline — fetch, *its own* extends,
//! drivers, store commit) and copied into the composed tree at
//! [`PARENT_DIR`].
//!
//! Because the parent's committed payload already carries *its*
//! `.scarab-parent/` (materialized by its own install), nesting is
//! automatic: a grandparent lands at
//! `.scarab-parent/.scarab-parent/`. The store commit therefore
//! carries the whole ancestor chain, and every later driver verb —
//! `install` (this dispatch), `deploy`, `uninstall` — sees it under
//! `--dir <payload>` with no extra work.
//!
//! Recursion is bounded like `include:` ([`IncludeState`]): a depth
//! cap ([`MAX_EXTENDS_DEPTH`]) and an in-progress label set
//! ([`IncludeState::extends_in_progress`]) that catches
//! `A extends B, B extends A` cycles through refs. Store-name
//! parents can't cycle — their payloads are already committed and
//! finite.
//!
//! The signature/consent/trust digest deliberately covers the tree
//! *before* `.scarab-parent/` is added: publish doesn't flatten
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
/// DAGs). Same bound as [`include::MAX_INCLUDE_DEPTH`].
pub(crate) const MAX_EXTENDS_DEPTH: usize = 8;

/// Payload subdir a resolved parent materializes into — the driver
/// (`tamias-driver-template`) discovers the chain by this name.
pub(crate) const PARENT_DIR: &str = ".scarab-parent";

/// `template.extends` declared by the manifest inside `dir` —
/// `Some(ref)` when present, `None` when absent or JSON-null.
///
/// The `template:` table is driver-owned: tamias re-reads the raw
/// manifest file (the typed [`tamias_core::Manifest`] drops unknown
/// keys) and validates only this one field — it must be a non-empty
/// string; anything else fails the install.
///
/// # Errors
/// Fails on manifest read/parse errors or a malformed `extends`.
pub(crate) fn extends_ref(dir: &Path) -> Result<Option<String>, Error> {
    let Some(path) = tamias_core::Manifest::path_in(dir) else {
        return Ok(None);
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
    match value.get("template").and_then(|t| t.get("extends")) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(_) => Err(Error::Failed(format!(
            "{}: template.extends must be a non-empty string ref",
            path.display()
        ))),
    }
}

/// Resolve `parent_ref` to a payload dir and copy it into
/// `child_root/.scarab-parent/` — replacing any dir already there
/// (a declared `extends` is authoritative over vendored bytes).
///
/// `child_label` is the installing package's store label
/// ([`include::pending_label`]) — the cycle-set key. `report` sees a
/// [`Progress::Including`] event for the parent, and a ref-parent's
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
    parent_ref: &str,
    state: &mut IncludeState,
    allow_drivers: bool,
    surface: Surface,
    report: &mut dyn FnMut(Progress),
) -> Result<(), Error> {
    let parent = resolve_parent(
        env,
        child_label,
        parent_ref,
        state,
        allow_drivers,
        surface,
        report,
    )?;
    let dest = child_root.join(PARENT_DIR);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(Error::io(format!("clear {}", dest.display())))?;
    }
    // create_dir_all first: an empty parent payload must still leave a
    // `.scarab-parent/` the driver can descend into (its manifest is
    // what load_chain reads — a payload always has one).
    std::fs::create_dir_all(&dest).map_err(Error::io(format!("create {}", dest.display())))?;
    copy_tree(&parent, &dest)
}

/// One `template.extends` ref → one parent payload dir. Mirrors
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
    //    payload already carries its own `.scarab-parent/` chain.
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
    fn extends_ref_reads_yaml_and_toml() {
        let dir = tempfile::tempdir().unwrap();
        manifest(dir.path(), "  extends: git:org/base\n");
        assert_eq!(
            extends_ref(dir.path()).unwrap().as_deref(),
            Some("git:org/base")
        );
        // absent / null → None
        manifest(dir.path(), "  files: []\n");
        assert_eq!(extends_ref(dir.path()).unwrap(), None);
        // non-string / empty → error
        manifest(dir.path(), "  extends: 42\n");
        assert!(extends_ref(dir.path()).is_err());
        manifest(dir.path(), "  extends: \"  \"\n");
        assert!(extends_ref(dir.path()).is_err());
        // toml spelling
        std::fs::write(
            dir.path().join("tamias.toml"),
            "name = \"x\"\ntype = \"template\"\n[template]\nextends = \"git:org/t\"\n",
        )
        .unwrap();
        std::fs::remove_file(dir.path().join("tamias.yaml")).unwrap();
        assert_eq!(
            extends_ref(dir.path()).unwrap().as_deref(),
            Some("git:org/t")
        );
    }
}
