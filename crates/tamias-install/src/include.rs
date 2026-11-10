//! `include:` base resolution — the install half of payload
//! composition (PL-0007 / P-004).
//!
//! Each `Manifest::include` entry resolves **in manifest order** to a
//! base payload dir:
//!
//! - an **installed store name** — full label (`@scope/name@version`),
//!   scoped name (`@scope/name`), or bare name (`name`, ambiguous
//!   across scopes → error) — via [`find_store_entry`] over
//!   `Store::entries`. A missing name is a hard error naming it with a
//!   hint to use an installable ref; `@`-prefixed entries never fall
//!   through to path resolution.
//! - an **installable ref** — anything the ref grammar claims
//!   (`git:`/`cf:`/`http(s)`/paths) that isn't a store name — recurses
//!   through [`crate::install_inner`], so the base gets the full
//!   pipeline (fetch → *its own* includes → drivers → store commit)
//!   and the outer package composes over the committed payload.
//!
//! Recursion is bounded two ways ([`IncludeState`]): a depth cap
//! ([`MAX_INCLUDE_DEPTH`]) and a cycle set of in-progress package
//! labels — a package including itself or an ancestor fails loudly.
//!
//! Composition itself is *not* transitive on the store-name path: a
//! store entry's payload is whatever was committed at *its* install
//! (already composed if it had `include:` then). We compose stored
//! payloads as-is — never re-resolve their includes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tamias_core::{find_store_entry, Error, StoreEntry, StoreRecord};
use tamias_source::{expand_path, Spec};
use tamias_source_fs::FsSpec;
use tamias_source_git::GitSpec;
use tamias_source_http::HttpSpec;
use tamias_store_local::Store;

use crate::{install_inner, Env, Progress, Surface};

/// Include-recursion cap — a chain of ref-includes deeper than this
/// fails instead of recursing forever (cycles are caught earlier by
/// [`IncludeState::in_progress`]; the cap bounds pathological DAGs).
pub(crate) const MAX_INCLUDE_DEPTH: usize = 8;

/// Cycle/depth state threaded through recursive installs. Created
/// fresh per top-level [`crate::install_with_progress`] call.
#[derive(Default)]
pub(crate) struct IncludeState {
    /// Labels (`@scope/name@version`, `name@<sha12>`) of packages whose
    /// `include:` lists are currently resolving — the ancestor chain.
    /// [`install_inner`] inserts its own label before resolving and
    /// removes it after; [`resolve_base`] rejects a base whose store
    /// label is already in the set.
    pub in_progress: HashSet<String>,
    /// Current include-recursion depth — `0` at the top-level install,
    /// incremented around each ref-include's recursive install.
    pub depth: usize,
}

/// The store-entry label for a not-yet-committed package — same rule
/// as [`StoreEntry::label`] (`@scope/name@version`, sha short form
/// when unversioned) so in-progress keys compare equal to store hits.
pub(crate) fn pending_label(
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    resolved: &str,
) -> String {
    let record = StoreRecord {
        name: name.to_string(),
        scope: scope.map(str::to_string),
        version: version.map(str::to_string),
        kind: None,
        source_ref: String::new(),
        resolved: resolved.to_string(),
        digest: String::new(),
        signature: None,
        signer: None,
        trust: None,
        installed_at: 0,
    };
    StoreEntry {
        dir: PathBuf::new(),
        record,
    }
    .label()
}

/// Resolve every `include:` entry of `includes` to a base payload dir,
/// **in manifest order** (earlier entries outrank later ones —
/// [`tamias_core::overlay`] applies them last-to-first).
///
/// `report` sees a [`Progress::Including`] event per resolved base so
/// surfaces can show composition happening; ref-includes additionally
/// stream the recursive install's own events.
///
/// # Errors
/// Fails on an ambiguous/missing store name, an unparseable ref, a
/// cycle, depth-cap breach, or any recursive-install failure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_bases(
    env: &Env,
    includes: &[String],
    state: &mut IncludeState,
    allow_drivers: bool,
    surface: Surface,
    report: &mut dyn FnMut(Progress),
) -> Result<Vec<PathBuf>, Error> {
    let store = Store::new(&env.store_root);
    let mut bases = Vec::with_capacity(includes.len());
    for text in includes {
        // Re-read per entry: a ref-include earlier in the list installs
        // into the store, and a later store-name entry may name it.
        let entries = store.entries()?;
        bases.push(resolve_base(
            env,
            &entries,
            text,
            state,
            allow_drivers,
            surface,
            report,
        )?);
    }
    Ok(bases)
}

/// One include entry → one base payload dir. See the module docs for
/// the store-name-then-ref order.
#[allow(clippy::too_many_arguments)]
fn resolve_base(
    env: &Env,
    entries: &[StoreEntry],
    text: &str,
    state: &mut IncludeState,
    allow_drivers: bool,
    surface: Surface,
    report: &mut dyn FnMut(Progress),
) -> Result<PathBuf, Error> {
    // 1. Store name — label / `@scope/name` / bare `name`.
    if let Some(entry) = find_store_entry(entries, text)? {
        let label = entry.label();
        if state.in_progress.contains(&label) {
            return Err(Error::Failed(format!(
                "include cycle: '{text}' resolves to {label}, which is already being installed"
            )));
        }
        report(Progress::Including { base: label });
        return Ok(entry.dir);
    }
    // `@scope/name` is a *name*, never a path — a missing one errors
    // here instead of falling through to fs-ref resolution.
    if text.starts_with('@') {
        return Err(missing_base(text));
    }
    // 2. Installable ref — recurse through the full pipeline.
    let spec = crate::refs().parse(text).map_err(|_| missing_base(text))?;
    // A remote include base is a transitive pull under this install's
    // consent flags — warn when the ref isn't pinned so the operator
    // knows provenance rides on "whatever the remote serves now".
    // `cf:` refs need no warning: they're pinned by name@version plus
    // the index-recorded payload digest.
    if let Some(reason) = unpinned_remote_ref(&spec) {
        eprintln!("warning: include '{text}' is an unpinned remote ref — {reason}");
    }
    if spec.is::<FsSpec>() {
        // `FsSpec.path` is `expand_path(input)` — recompute it rather
        // than downcasting the opaque spec.
        if !expand_path(Path::new(text)).exists() {
            return Err(missing_base(text));
        }
    }
    if state.depth >= MAX_INCLUDE_DEPTH {
        return Err(Error::Failed(format!(
            "include '{text}' exceeds the {MAX_INCLUDE_DEPTH}-deep nesting cap"
        )));
    }
    report(Progress::Including {
        base: text.to_string(),
    });
    state.depth += 1;
    let result = install_inner(env, text, allow_drivers, surface, report, state);
    state.depth -= 1;
    let entry = result?;
    Ok(entry.dir)
}

/// Why a parsed include-base ref is an *unpinned remote pull*, or
/// `None` when it's pinned/local. Remote refs fetched through `include:`
/// are transitive pulls the top-level ref's consent flags cover — the
/// warning is advisory (remote includes are legitimate), not a failure.
///
/// - `http(s)://…` without a `#sha256:` fragment → warn (unpinned).
/// - `git:…` without a `#<rev>` that is a full 40-hex sha → warn
///   (moving refs like `#main`/`#v1` resolve at fetch time). Local-path
///   clones (`/`, `.`, `~`, `file://`) aren't remote pulls → no warn.
/// - `cf:` is registry-pinned (name@version + index payload digest) →
///   no warn; fs paths and store names never reach here as remote.
fn unpinned_remote_ref(spec: &Spec) -> Option<&'static str> {
    if let Some(http) = spec.downcast_ref::<HttpSpec>() {
        return (http.digest.is_none()).then_some("http ref has no #sha256: digest pin");
    }
    if let Some(git) = spec.downcast_ref::<GitSpec>() {
        let local = git.url.starts_with("file://")
            || git.url.starts_with('/')
            || git.url.starts_with('.')
            || git.url.starts_with('~');
        if !local {
            let pinned = git
                .rev
                .as_deref()
                .is_some_and(|rev| rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()));
            if !pinned {
                return Some("git ref is not pinned to a full commit sha");
            }
        }
    }
    None
}

/// How an `include:` entry resolves for [`crate::plan_install`] — the
/// store-name-then-ref classification of [`resolve_base`] minus the
/// recursive install and any fetch.
pub(crate) enum IncludePlan {
    /// An installed store entry — carries its [`StoreEntry::label`].
    Store(String),
    /// Not a store name, but the ref grammar claims it — a real
    /// install would recursively install it as a base.
    WouldInstall,
    /// Neither — carries why (an `@`-name that's not installed, an
    /// ambiguous store name, an unparseable ref, or a path that
    /// doesn't exist). A real install fails naming this entry.
    Missing(String),
}

/// Classify one `include:` entry without installing anything — see
/// [`IncludePlan`]. Mirrors [`resolve_base`]'s order: store name via
/// [`find_store_entry`] first, `@`-prefixed names never fall through
/// to ref resolution, fs paths must exist on disk.
pub(crate) fn plan_include(entries: &[StoreEntry], text: &str) -> IncludePlan {
    match find_store_entry(entries, text) {
        Ok(Some(entry)) => return IncludePlan::Store(entry.label()),
        Ok(None) => {}
        // Ambiguous bare name — the real install errors; surface the
        // reason so the plan's problems list stays accurate.
        Err(e) => return IncludePlan::Missing(e.to_string()),
    }
    if text.starts_with('@') {
        return IncludePlan::Missing(missing_base(text).to_string());
    }
    match crate::refs().parse(text) {
        Ok(spec) => {
            // `FsSpec.path` is `expand_path(input)` — same existence
            // check `resolve_base` makes before recursing.
            if spec.is::<FsSpec>() && !expand_path(Path::new(text)).exists() {
                IncludePlan::Missing(missing_base(text).to_string())
            } else {
                IncludePlan::WouldInstall
            }
        }
        Err(_) => IncludePlan::Missing(missing_base(text).to_string()),
    }
}

/// The hard error for an include entry that is neither an installed
/// store name nor a resolvable ref.
fn missing_base(text: &str) -> Error {
    Error::Failed(format!(
        "include '{text}' is not installed — store names must already be in the store; \
         use an installable ref (path, git:, cf:, http:) or `tamias install {text}` first"
    ))
}

/// Pin the composed payload's manifest to the *local* package's
/// manifest file, byte-for-byte. [`tamias_core::overlay`] doc-merges
/// `.yaml`/`.json` files — right for payload docs, wrong for the
/// manifest itself (a base's `deps:`/`include:` arrays would concat
/// into the composed manifest). The top package's manifest is
/// authoritative: `include:` stays for provenance, nothing inherits.
/// Stray manifest files a base contributed under a different
/// [`MANIFEST_NAMES`](tamias_core::MANIFEST_NAMES) spelling are
/// removed so `Manifest::load` sees exactly one.
///
/// # Errors
/// Propagates copy/remove failures.
pub(crate) fn pin_manifest(local: &Path, dest: &Path) -> Result<(), Error> {
    let Some(src) = tamias_core::Manifest::path_in(local) else {
        return Ok(());
    };
    let file_name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for candidate in tamias_core::MANIFEST_NAMES {
        let target = dest.join(candidate);
        if candidate == file_name {
            std::fs::copy(&src, &target).map_err(tamias_core::Error::io(format!(
                "pin manifest {}",
                target.display()
            )))?;
        } else if target.exists() {
            std::fs::remove_file(&target).map_err(tamias_core::Error::io(format!(
                "remove stray manifest {}",
                target.display()
            )))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Parse `input` through the same grammar `resolve_base` uses.
    fn parsed(input: &str) -> Spec {
        crate::refs().parse(input).unwrap()
    }

    #[test]
    fn unpinned_remote_ref_flags() {
        // http(s) without #sha256: → warn; pinned → clean.
        assert!(unpinned_remote_ref(&parsed("https://x/y.tgz")).is_some());
        assert!(unpinned_remote_ref(&parsed("http://x/y.tgz")).is_some());
        let sha = "a".repeat(64);
        assert!(unpinned_remote_ref(&parsed(&format!("https://x/y.tgz#sha256:{sha}"))).is_none());
        assert!(unpinned_remote_ref(&parsed(&format!("http://x/y.tgz#sha256={sha}"))).is_none());

        // git: remote without a full-sha rev → warn.
        assert!(unpinned_remote_ref(&parsed("git:https://h/o/r")).is_some());
        assert!(unpinned_remote_ref(&parsed("git:org/repo#main")).is_some());
        assert!(unpinned_remote_ref(&parsed("git:org/repo#v1.0")).is_some());
        assert!(unpinned_remote_ref(&parsed("git:org/repo#abc1234")).is_some());
        // full 40-hex sha → pinned.
        let commit = "0123456789abcdef0123456789abcdef01234567";
        assert!(unpinned_remote_ref(&parsed(&format!("git:org/repo#{commit}"))).is_none());
        // local clones aren't remote pulls.
        assert!(unpinned_remote_ref(&parsed("git:file:///tmp/r")).is_none());
        assert!(unpinned_remote_ref(&parsed("git:/abs/path")).is_none());
        assert!(unpinned_remote_ref(&parsed("git:./rel/path")).is_none());
        assert!(unpinned_remote_ref(&parsed("git:~/path")).is_none());

        // cf: is registry-pinned; fs paths and plain names are local.
        assert!(unpinned_remote_ref(&parsed("cf:9aia/bucket/hello@0.1.0")).is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg");
        std::fs::create_dir_all(&path).unwrap();
        assert!(unpinned_remote_ref(&parsed(path.to_str().unwrap())).is_none());
    }
}
