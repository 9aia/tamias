#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-bin — the **declared bins** lifecycle (proposal P-023):
//! `bins[]` entries in a package manifest become commands on `PATH`.
//!
//! Two declaration kinds (see [`tamias_core::BinDecl`]):
//!
//! - `shim: "cli"` + `driver: <kind>` — a generated front-end. Deploy
//!   writes a tiny shell script into the bin dir that execs
//!   `tamias bin exec <label> <name> "$@"`; `tamias` itself is the
//!   helper binary, so driver resolution stays dynamic
//!   (`tamias-driver-<kind>` on PATH/`<tamias-home>/drivers/`, with the
//!   capabilities fallback — e.g. a generic `tamias-driver-cli`). The
//!   script is preferred over a compiled helper: zero build, and the
//!   dispatch logic lives in one place (`tamias bin exec`).
//! - `payload: tools/bin/<triple>/<file>` — a real binary shipped in
//!   the payload. Only the host triple's file deploys; the `<triple>`
//!   segment in the declared path is substituted with
//!   [`tamias_driver::target_triple`] when the literal path doesn't
//!   exist, so one manifest serves every platform. A payload whose
//!   `tools/bin/` ships platform dirs but none for this host fails the
//!   install — the same rule `drivers/bin/` already enforces.
//!
//! Deployed payload bins link through the same per-package
//! `bin-alias/<@scope/>name` → entry-dir alias `tamias bin` uses for
//! `bin/` payloads, so upgrades retarget once and every link keeps
//! resolving. Ownership is proven by the link target (payload) or a
//! `# tamias-bin: <label>` marker comment (shim scripts), which is
//! what lets `destroy`/`remove` remove only what a package deployed —
//! name collisions warn and last-deploy wins, never a hard error.
//!
//! The bin dir itself is the caller's (`$TAMIAS_BIN_DIR` →
//! `XDG_BIN_HOME` → `~/.local/bin` in the CLI); this crate takes
//! explicit paths so every surface — CLI today, serve/TUI later —
//! shares one implementation.

use std::path::{Path, PathBuf};

use tamias_core::{BinDecl, Error, Manifest, StoreEntry, StoreRecord};

/// Payload subdir holding per-platform tool binaries —
/// `tools/bin/<target-triple>/<file>` (mirrors `drivers/bin/`).
pub const TOOLS_BIN: &str = "tools/bin";

/// First line marker every generated shim script carries — ownership
/// proof for `destroy`/collision detection (`# tamias-bin: <label>`).
const SHIM_MARKER: &str = "# tamias-bin:";

/// What [`deploy_bins`] did — action + warning lines for the caller
/// to render (the CLI prints actions like driver actions).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeployReport {
    /// One line per materialized/confirmed artifact.
    pub actions: Vec<String>,
    /// Non-fatal problems (name collisions won, foreign files kept).
    pub warnings: Vec<String>,
    /// Per-decl outcome — the structured half of `actions`.
    pub bins: Vec<DeployedBin>,
}

/// One declared bin's deploy outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployedBin {
    /// The command name (`bins[].name`).
    pub name: String,
    /// `shim` or `payload`.
    pub kind: &'static str,
    /// The bin-dir path.
    pub path: PathBuf,
    /// `deployed` (fresh) or `already`.
    pub status: &'static str,
}

/// What [`destroy_bins`] removed — paths unlinked/deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DestroyReport {
    /// Bin-dir paths removed.
    pub removed: Vec<String>,
    /// Bin names left in place because a *different* package owns them.
    pub kept: Vec<String>,
}

/// One declared bin as [`list_bins`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinListing {
    /// The command name (`bins[].name`).
    pub name: String,
    /// The declaring package's label (`@scope/name@version`).
    pub package: String,
    /// `shim` or `payload`.
    pub kind: &'static str,
    /// Driver kind for `shim` decls.
    pub driver: Option<String>,
    /// Whether `<bin_dir>/<name>` currently resolves to this package.
    pub deployed: bool,
    /// The bin-dir path (`<bin_dir>/<name>`).
    pub path: PathBuf,
    /// Another installed package declares the same bin name.
    pub contended: bool,
}

/// The install-time gate for `tools/bin/` payloads + declared bins —
/// run by `tamias-install` on the tree about to be committed:
///
/// - `tools/bin/` platform dirs exist but none matches this host →
///   hard error (same rule as `drivers/bin/`).
/// - every `bins[].payload` must resolve to a real file in the
///   payload — a declared bin that can't materialize is a broken
///   package, not a deploy-time surprise.
///
/// # Errors
/// Fails on the no-matching-platform error or an unresolvable
/// declared payload bin.
pub fn check_bins(payload_root: &Path, manifest: &Manifest) -> Result<(), Error> {
    check_platform(payload_root)?;
    for decl in &manifest.bins {
        if decl.payload.is_some() {
            resolve_payload_bin(payload_root, decl)?;
        }
    }
    Ok(())
}

/// The `tools/bin/` platform rule — error only when platform dirs
/// exist and none is this host's (a payload with just portable files
/// or no `tools/bin/` at all passes). Public so `install --plan` can
/// surface the same would-be failure as a problem instead of erroring.
///
/// # Errors
/// The no-matching-platform error.
pub fn check_platform(payload_root: &Path) -> Result<(), Error> {
    let bin = payload_root.join(TOOLS_BIN);
    if !bin.is_dir() {
        return Ok(());
    }
    let triples = platform_dirs(&bin)?;
    if triples.is_empty() {
        return Ok(());
    }
    let host = tamias_driver::target_triple();
    if !triples.iter().any(|t| t == host) {
        return Err(Error::Failed(format!(
            "package ships tool binaries for {} but not this platform ({host})",
            triples.join(", ")
        )));
    }
    Ok(())
}

/// Dir names directly under `tools/bin/` — each is a target-triple
/// candidate (same convention as `drivers/bin/`).
fn platform_dirs(bin: &Path) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(bin).map_err(Error::io(format!("read {}", bin.display())))? {
        let entry = entry.map_err(Error::io("read tools/bin entry"))?;
        if entry
            .file_type()
            .map_err(Error::io(format!("stat {}", entry.path().display())))?
            .is_dir()
        {
            if let Some(name) = entry.file_name().to_str() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Pack-time conformance gate for `bins[].payload` decls — every
/// declared payload must have a shipped counterpart *for some*
/// platform. The install host's triple is substituted at deploy time
/// ([`resolve_payload_bin`]), so pack time can't pin one: a
/// `tools/bin/<triple>/<rest>` decl is satisfied by `<rest>` under
/// *any* shipped platform dir, while a portable `tools/bin/<file>`
/// decl needs that literal file (mirroring the resolver's rules —
/// substitution only applies to paths deep enough to carry a triple
/// segment).
///
/// Catches authoring typos at `tamias pack`/`publish` instead of
/// install time. [`check_bins`] remains the install-side gate — it
/// additionally enforces the host-platform rule, which is
/// host-dependent and must not run at pack time.
///
/// # Errors
/// Fails naming each decl with no shipped counterpart.
pub fn check_shipped_bins(payload_root: &Path, manifest: &Manifest) -> Result<(), Error> {
    let decls: Vec<&BinDecl> = manifest
        .bins
        .iter()
        .filter(|d| d.payload.is_some())
        .collect();
    if decls.is_empty() {
        return Ok(());
    }
    let shipped = shipped_payload_keys(payload_root)?;
    let mut missing = Vec::new();
    for decl in decls {
        // `validate()` guarantees a payload decl carries a path; an
        // unvalidated manifest just reports as missing.
        let Some(payload) = decl.payload.as_deref() else {
            continue;
        };
        let mut key: Vec<String> = payload.split('/').map(str::to_string).collect();
        if key.len() >= 4 {
            key[2] = "*".to_string();
        }
        if !shipped.contains(&key) {
            missing.push(format!("'{}' ({payload})", decl.name));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "declared bins[] payloads not shipped under {}: {}",
            payload_root.join(TOOLS_BIN).display(),
            missing.join(", ")
        )))
    }
}

/// Every file under `tools/bin/` as a match key — component list with
/// the platform-dir segment wildcarded (`tools/bin/*/<rest>`), or the
/// literal `tools/bin/<file>` for portable files. Dirs directly under
/// `tools/bin/` are target triples by convention (same rule as
/// [`platform_dirs`]), so their names never appear in keys.
fn shipped_payload_keys(
    payload_root: &Path,
) -> Result<std::collections::BTreeSet<Vec<String>>, Error> {
    let mut out = std::collections::BTreeSet::new();
    let bin = payload_root.join(TOOLS_BIN);
    if !bin.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&bin).map_err(Error::io(format!("read {}", bin.display())))? {
        let entry = entry.map_err(Error::io(format!("read {}", bin.display())))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            // Platform dir — its name wildcards out of the key.
            collect_payload_keys(
                &path,
                &mut vec!["tools".into(), "bin".into(), "*".into()],
                &mut out,
            )?;
        } else if path.is_file() {
            out.insert(vec!["tools".into(), "bin".into(), name]);
        }
    }
    Ok(out)
}

/// Recursive walk filling `out` — `prefix` is the key's leading
/// components (already wildcarded for the platform dir).
fn collect_payload_keys(
    dir: &Path,
    prefix: &mut Vec<String>,
    out: &mut std::collections::BTreeSet<Vec<String>>,
) -> Result<(), Error> {
    for entry in std::fs::read_dir(dir).map_err(Error::io(format!("read {}", dir.display())))? {
        let entry = entry.map_err(Error::io(format!("read {}", dir.display())))?;
        let path = entry.path();
        // `is_file`/`is_dir` follow symlinks — a linked payload file
        // still counts as shipped (same as `resolve_payload_bin`).
        if path.is_dir() {
            prefix.push(entry.file_name().to_string_lossy().to_string());
            collect_payload_keys(&path, prefix, out)?;
            prefix.pop();
        } else if path.is_file() {
            let mut key = prefix.clone();
            key.push(entry.file_name().to_string_lossy().to_string());
            out.insert(key);
        }
    }
    Ok(())
}

/// `tools/bin/` contents as a human list for error messages —
/// `<triple>/<name>` for platform files, bare `<name>` for portable
/// ones.
fn tools_bin_listing(bin: &Path) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(bin).map_err(Error::io(format!("read {}", bin.display())))? {
        let entry = entry.map_err(Error::io("read tools/bin entry"))?;
        let ft = entry
            .file_type()
            .map_err(Error::io(format!("stat {}", entry.path().display())))?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if ft.is_dir() {
            for f in std::fs::read_dir(entry.path())
                .map_err(Error::io(format!("read {}", entry.path().display())))?
            {
                let f = f.map_err(Error::io("read tools/bin file"))?;
                if f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    if let Some(fname) = f.file_name().to_str() {
                        out.push(format!("{name}/{fname}"));
                    }
                }
            }
        } else if ft.is_file() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Resolve a `bins[].payload` declaration to `(abs_src, rel)` — the
/// file inside `payload_root` and its payload-relative path *with the
/// host triple substituted* (what alias-relative link targets use).
///
/// Resolution order:
/// 1. the declared path verbatim (`tools/bin/<host>/<file>` on the
///    matching host, or a portable `tools/bin/<file>`);
/// 2. `tools/bin/<declared-triple>/…` with the triple segment replaced
///    by [`tamias_driver::target_triple`] — one manifest, every host;
/// 3. error naming what `tools/bin/` actually ships.
///
/// # Errors
/// Fails when neither form exists or `tools/bin/` is unreadable.
pub fn resolve_payload_bin(
    payload_root: &Path,
    decl: &BinDecl,
) -> Result<(PathBuf, PathBuf), Error> {
    let declared = decl
        .payload
        .as_deref()
        .ok_or_else(|| Error::Failed(format!("bin '{}' has no payload path", decl.name)))?;
    let literal = payload_root.join(declared);
    if literal.is_file() {
        return Ok((literal, PathBuf::from(declared)));
    }
    // Triple substitution — `tools/bin/<seg>/…` where <seg> isn't this
    // host becomes `tools/bin/<host>/…`. Only paths deep enough to
    // carry a triple segment (tools/bin/<x>/<file>) qualify; a bare
    // `tools/bin/<file>` is portable and has nothing to substitute.
    let comps: Vec<&str> = declared.split('/').collect();
    if comps.len() >= 4 {
        let host = tamias_driver::target_triple();
        if comps[2] != host {
            let mut sub = comps.clone();
            sub[2] = host;
            let rel = sub.join("/");
            let candidate = payload_root.join(&rel);
            if candidate.is_file() {
                return Ok((candidate, PathBuf::from(rel)));
            }
        }
    }
    let bin = payload_root.join(TOOLS_BIN);
    let shipped = if bin.is_dir() {
        tools_bin_listing(&bin).unwrap_or_default()
    } else {
        Vec::new()
    };
    Err(Error::Failed(format!(
        "bin '{}': payload '{}' not found{}",
        decl.name,
        declared,
        if shipped.is_empty() {
            " — package ships no tools/bin/ payload".to_string()
        } else {
            format!(" — tools/bin/ ships: {}", shipped.join(", "))
        }
    )))
}

/// `@scope/name` or `name` — the package identity *without* the
/// version label. This is the ownership token shim scripts carry:
/// versions of one package share it, so an upgrade re-deploy reads as
/// "our own shim" (refresh), not a cross-package collision.
pub fn package_id(record: &StoreRecord) -> String {
    match &record.scope {
        Some(scope) => format!("@{scope}/{}", record.name),
        None => record.name.clone(),
    }
}

/// `@scope/name` or `name` as a relative path — the alias's location
/// under the alias root (mirrors the CLI's `alias_rel`).
fn alias_rel(record: &StoreRecord) -> PathBuf {
    match &record.scope {
        Some(scope) => Path::new(&format!("@{scope}")).join(&record.name),
        None => PathBuf::from(&record.name),
    }
}

/// The per-package alias — `<alias_root>/@scope/name` → entry dir.
pub fn alias_path(alias_root: &Path, record: &StoreRecord) -> PathBuf {
    alias_root.join(alias_rel(record))
}

/// Point `alias` at `entry_dir` — one symlink rewrite retargets every
/// link the package owns (unix only; copy platforms resolve through
/// the entry dir directly and need no alias). An existing non-symlink
/// occupant is an error unless `force`.
fn ensure_alias(alias: &Path, entry_dir: &Path, force: bool) -> Result<(), Error> {
    #[cfg(unix)]
    {
        if let Some(parent) = alias.parent() {
            std::fs::create_dir_all(parent)
                .map_err(Error::io(format!("create {}", parent.display())))?;
        }
        if let Ok(meta) = std::fs::symlink_metadata(alias) {
            if meta.file_type().is_symlink() {
                let current = std::fs::read_link(alias).unwrap_or_default();
                if current.as_path() == entry_dir {
                    return Ok(());
                }
                std::fs::remove_file(alias)
                    .map_err(Error::io(format!("replace {}", alias.display())))?;
            } else if meta.is_dir() {
                if !force {
                    return Err(Error::Failed(format!(
                        "{} is a directory — refusing to replace (use --force)",
                        alias.display()
                    )));
                }
                std::fs::remove_dir_all(alias)
                    .map_err(Error::io(format!("remove {}", alias.display())))?;
            } else if force {
                std::fs::remove_file(alias)
                    .map_err(Error::io(format!("remove {}", alias.display())))?;
            } else {
                return Err(Error::Failed(format!(
                    "{} already exists — refusing to overwrite (use --force)",
                    alias.display()
                )));
            }
        }
        std::os::unix::fs::symlink(entry_dir, alias).map_err(Error::io(format!(
            "link {} → {}",
            alias.display(),
            entry_dir.display()
        )))
    }
    #[cfg(not(unix))]
    {
        let _ = (alias, entry_dir, force);
        Ok(())
    }
}

/// The generated shim script — `tamias` itself is the helper binary:
/// the script only maps the command name onto `tamias bin exec`,
/// which re-resolves the kind driver at call time (PATH +
/// `<tamias-home>/drivers/`). The `# tamias-bin:` marker is the
/// ownership proof `destroy`/collision checks read.
fn shim_script(pkg: &str, label: &str, name: &str) -> String {
    format!(
        "#!/bin/sh\n\
         {SHIM_MARKER} {pkg} — generated by `tamias bin deploy`; do not edit.\n\
         exec tamias bin exec '{label}' '{name}' \"$@\"\n"
    )
}

/// Best-effort exec bit on unix.
fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// The label marker inside a shim script at `path`, when it is one of
/// ours — `Some(label)` for a tamias-generated shim, `None` for
/// anything else (foreign file, binary, symlink, unreadable).
fn shim_marker(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return None;
    }
    // A shim is a few hundred bytes; cap the read so a huge foreign
    // file isn't slurped for a marker check.
    let text = std::fs::read_to_string(path).ok()?;
    let head: String = text.chars().take(4096).collect();
    let line = head.lines().find(|l| l.starts_with(SHIM_MARKER))?;
    // The marker line reads `# tamias-bin: <pkg> — generated by …`;
    // ownership is the first whitespace-separated token only.
    Some(
        line[SHIM_MARKER.len()..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string(),
    )
}

/// Whether `dest` is a symlink into `alias` — the payload-bin
/// ownership proof (links hang off the per-package alias, so entry-dir
/// churn can't break or confuse them).
fn links_into_alias(dest: &Path, alias: &Path) -> bool {
    std::fs::symlink_metadata(dest)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
        && std::fs::read_link(dest)
            .map(|t| t.starts_with(alias))
            .unwrap_or(false)
}

/// Byte-compare — the copy-platform ownership check for payload bins
/// (and `same_file` semantics for idempotent re-deploys).
fn same_file(a: &Path, b: &Path) -> Result<bool, Error> {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => Ok(x == y),
        (Err(e), _) | (_, Err(e)) => Err(Error::io("read file for compare")(e)),
    }
}

/// The "already exists" refusal — same wording the CLI's `bin_conflict`
/// uses so `--force` reads consistently.
fn conflict(dest: &Path) -> Error {
    Error::Failed(format!(
        "{} already exists — refusing to overwrite (use --force)",
        dest.display()
    ))
}

/// Materialize `dest` for a shim script — plain file write. An
/// existing dest is `Already` when it carries *this* package's marker,
/// a collision warning + replace when it carries another tamias
/// package's marker (last-deploy wins, per P-023), and a hard
/// conflict for foreign files unless `force`.
fn place_shim(
    dest: &Path,
    script: &str,
    pkg: &str,
    alias_root: &Path,
    force: bool,
    warnings: &mut Vec<String>,
) -> Result<&'static str, Error> {
    if let Ok(meta) = std::fs::symlink_metadata(dest) {
        if meta.file_type().is_symlink() {
            // A payload-bin link (this or another package's alias) is a
            // collision — warn + take it; foreign links conflict.
            let cur = std::fs::read_link(dest)
                .map_err(Error::io(format!("read link {}", dest.display())))?;
            if cur.starts_with(alias_root) {
                warnings.push(format!(
                    "bin name collision: {} → {} — overwriting",
                    dest.display(),
                    cur.display(),
                ));
            } else if !force {
                return Err(conflict(dest));
            }
            std::fs::remove_file(dest).map_err(Error::io(format!("remove {}", dest.display())))?;
        } else if meta.is_dir() {
            if !force {
                return Err(conflict(dest));
            }
            remove_dest(dest, true)?;
        } else if let Some(owner) = shim_marker(dest) {
            if owner == pkg {
                // Our own shim — refresh it only if contents drifted.
                if std::fs::read_to_string(dest).ok().as_deref() == Some(script) {
                    return Ok("already");
                }
            } else {
                warnings.push(format!(
                    "bin name collision: {} is deployed by {owner} — overwriting for {pkg}",
                    dest.display(),
                ));
            }
            std::fs::remove_file(dest).map_err(Error::io(format!("remove {}", dest.display())))?;
        } else if !force {
            return Err(conflict(dest));
        } else {
            std::fs::remove_file(dest).map_err(Error::io(format!("remove {}", dest.display())))?;
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(Error::io(format!("create {}", parent.display())))?;
    }
    std::fs::write(dest, script).map_err(Error::io(format!("write {}", dest.display())))?;
    set_executable(dest);
    Ok("deployed")
}

fn remove_dest(dest: &Path, is_dir: bool) -> Result<(), Error> {
    let res = if is_dir {
        std::fs::remove_dir_all(dest)
    } else {
        std::fs::remove_file(dest)
    };
    res.map_err(Error::io(format!("remove {}", dest.display())))
}

/// Materialize `dest` for a payload bin — unix symlinks `dest →
/// target` (the alias-relative path), elsewhere copies `src`. Mirrors
/// the CLI's `place_bin`/`materialize` rules: a different symlink or a
/// foreign file conflicts unless `force`; a same-bytes copy is
/// `Already`.
fn place_payload(
    src: &Path,
    dest: &Path,
    target: &Path,
    alias_root: &Path,
    force: bool,
    warnings: &mut Vec<String>,
) -> Result<&'static str, Error> {
    let meta = match std::fs::symlink_metadata(dest) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(Error::io(format!("stat {}", dest.display()))(e)),
    };
    let Some(meta) = meta else {
        return materialize_payload(src, dest, target).map(|()| "deployed");
    };
    if meta.file_type().is_symlink() {
        let cur =
            std::fs::read_link(dest).map_err(Error::io(format!("read link {}", dest.display())))?;
        if cur.as_path() == target {
            return Ok("already");
        }
        // A link into *another* tamias package's alias is a collision —
        // warn + take it (last-deploy wins); anything else conflicts.
        if cur.starts_with(alias_root) {
            warnings.push(format!(
                "bin name collision: {} → {} — overwriting",
                dest.display(),
                cur.display(),
            ));
        } else if !force {
            return Err(conflict(dest));
        }
        std::fs::remove_file(dest).map_err(Error::io(format!("remove {}", dest.display())))?;
        return materialize_payload(src, dest, target).map(|()| "deployed");
    }
    if meta.is_dir() {
        return Err(Error::Failed(format!(
            "{} is a directory — `tamias bin` won't touch it",
            dest.display()
        )));
    }
    if !meta.is_file() {
        return Err(Error::Failed(format!(
            "{} is not a regular file",
            dest.display()
        )));
    }
    #[cfg(not(unix))]
    if same_file(src, dest)? {
        return Ok("already");
    }
    if !force {
        return Err(conflict(dest));
    }
    std::fs::remove_file(dest).map_err(Error::io(format!("remove {}", dest.display())))?;
    materialize_payload(src, dest, target).map(|()| "deployed")
}

/// Write `dest` for a payload bin: unix symlinks `dest → target` (the
/// alias path, so entry-dir churn can't break it); elsewhere copies
/// `src` — Windows symlink creation needs privileges.
fn materialize_payload(src: &Path, dest: &Path, target: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        let _ = src;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(Error::io(format!("create {}", parent.display())))?;
        }
        std::os::unix::fs::symlink(target, dest).map_err(Error::io(format!(
            "link {} → {}",
            dest.display(),
            target.display()
        )))
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(Error::io(format!("create {}", parent.display())))?;
        }
        std::fs::copy(src, dest).map_err(Error::io(format!(
            "copy {} → {}",
            src.display(),
            dest.display()
        )))?;
        Ok(())
    }
}

/// Deploy an installed entry's declared `bins[]` into `bin_dir`.
///
/// - `shim` decls get a generated script (`tamias bin exec` front-end).
/// - `payload` decls link/copy the host-triple file through the
///   package's `bin-alias` (created/retargeted here — the same alias
///   `tamias bin`'s `bin/` payload links use, so upgrades keep
///   working).
///
/// Name collisions never hard-fail: a dest owned by another tamias
/// package is replaced with a warning (last-deploy wins); a foreign
/// file conflicts unless `force`. On error, artifacts created by this
/// call are removed again — a failed deploy leaves no half-state (the
/// shared alias may remain; it is harmless bookkeeping).
///
/// # Errors
/// Manifest read, alias, resolution, or filesystem failures.
pub fn deploy_bins(
    entry: &StoreEntry,
    bin_dir: &Path,
    alias_root: &Path,
    force: bool,
) -> Result<DeployReport, Error> {
    let manifest = entry.manifest()?;
    if manifest.bins.is_empty() {
        return Ok(DeployReport::default());
    }
    let pkg = package_id(&entry.record);
    let label = entry.label();
    let alias = alias_path(alias_root, &entry.record);
    let mut report = DeployReport::default();
    let mut created: Vec<PathBuf> = Vec::new();
    let result = (|| -> Result<(), Error> {
        ensure_alias(&alias, &entry.dir, force)?;
        std::fs::create_dir_all(bin_dir)
            .map_err(Error::io(format!("create {}", bin_dir.display())))?;
        for decl in &manifest.bins {
            let dest = bin_dir.join(&decl.name);
            let status = if decl.shim.is_some() {
                place_shim(
                    &dest,
                    &shim_script(&pkg, &label, &decl.name),
                    &pkg,
                    alias_root,
                    force,
                    &mut report.warnings,
                )?
            } else {
                let (src, rel) = resolve_payload_bin(&entry.dir, decl)?;
                // Link through the alias — `target` is what the bin-dir
                // entry resolves to; `src` only matters for copies.
                let target = alias.join(&rel);
                place_payload(
                    &src,
                    &dest,
                    &target,
                    alias_root,
                    force,
                    &mut report.warnings,
                )?
            };
            if status == "deployed" {
                created.push(dest.clone());
            }
            report.bins.push(DeployedBin {
                name: decl.name.clone(),
                kind: if decl.shim.is_some() {
                    "shim"
                } else {
                    "payload"
                },
                path: dest.clone(),
                status: if status == "deployed" {
                    "deployed"
                } else {
                    "already"
                },
            });
            report
                .actions
                .push(format!("bin: {} ({status})", dest.display()));
        }
        Ok(())
    })();
    if result.is_err() {
        for path in created.iter().rev() {
            let _ = std::fs::remove_file(path);
        }
    }
    result?;
    Ok(report)
}

/// Destroy an entry's declared bins — remove the bin-dir artifacts
/// this package owns, leave foreign/other-package files alone.
///
/// `entry_dir` is `Some` while the store entry exists (enables the
/// copy-platform byte-compare ownership check); `None` after
/// `tamias remove` — symlinks are still identifiable through the
/// alias, scripts through their marker, copies are left (can't prove
/// ownership without the payload).
///
/// # Errors
/// Filesystem failures; unresolvable payload decls are skipped (the
/// package is going away anyway).
pub fn destroy_bins(
    entry_dir: Option<&Path>,
    record: &StoreRecord,
    decls: &[BinDecl],
    bin_dir: &Path,
    alias_root: &Path,
    force: bool,
) -> Result<DestroyReport, Error> {
    // Ownership token — package identity, not the versioned label, so
    // destroying one coexisting version doesn't orphan another's shim
    // semantics (last-deploy still wins on the file itself).
    let label = package_id(record);
    let alias = alias_path(alias_root, record);
    let mut report = DestroyReport::default();
    for decl in decls {
        let dest = bin_dir.join(&decl.name);
        let Ok(meta) = std::fs::symlink_metadata(&dest) else {
            continue;
        };
        let owned = if decl.shim.is_some() {
            shim_marker(&dest).as_deref() == Some(label.as_str())
        } else {
            let rel = entry_dir
                .and_then(|dir| resolve_payload_bin(dir, decl).ok())
                .map(|(_, rel)| rel);
            if meta.file_type().is_symlink() {
                links_into_alias(&dest, &alias)
            } else if meta.is_file() {
                match (&entry_dir, &rel) {
                    (Some(dir), Some(rel)) => same_file(&dir.join(rel), &dest).unwrap_or(false),
                    _ => false,
                }
            } else {
                false
            }
        };
        if owned || (force && (meta.file_type().is_symlink() || meta.is_file())) {
            remove_dest(&dest, meta.is_dir())?;
            report.removed.push(dest.display().to_string());
        } else {
            report.kept.push(decl.name.clone());
        }
    }
    Ok(report)
}

/// Every installed entry's declared bins with deploy state — the
/// `tamias bin list` data. `alias_root` is the `bin-alias` root the
/// deploy side uses (a sibling of the store root). Entries whose
/// manifest can't be read are skipped (a torn store entry can't
/// declare bins anyway).
///
/// # Errors
/// Propagates per-entry manifest/digest I/O failures.
pub fn list_bins(
    entries: &[StoreEntry],
    bin_dir: &Path,
    alias_root: &Path,
) -> Result<Vec<BinListing>, Error> {
    // (decl, package label, entry, alias) flattened first so contention
    // counts every declarer, deployed or not.
    struct Row<'a> {
        decl: BinDecl,
        entry: &'a StoreEntry,
        label: String,
        alias: PathBuf,
    }
    let mut rows = Vec::new();
    for entry in entries {
        let Ok(manifest) = entry.manifest() else {
            continue;
        };
        let label = entry.label();
        for decl in &manifest.bins {
            rows.push(Row {
                decl: decl.clone(),
                entry,
                label: label.clone(),
                alias: alias_path(alias_root, &entry.record),
            });
        }
    }
    let mut name_counts = std::collections::BTreeMap::<String, usize>::new();
    for row in &rows {
        *name_counts.entry(row.decl.name.clone()).or_default() += 1;
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let dest = bin_dir.join(&row.decl.name);
        let deployed = dest_ownership(
            &dest,
            &row.decl,
            row.entry,
            &package_id(&row.entry.record),
            &row.alias,
        );
        out.push(BinListing {
            name: row.decl.name.clone(),
            package: row.label.clone(),
            kind: if row.decl.shim.is_some() {
                "shim"
            } else {
                "payload"
            },
            driver: row.decl.driver.clone(),
            deployed,
            path: dest,
            contended: name_counts[&row.decl.name] > 1,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.package.cmp(&b.package)));
    Ok(out)
}

/// Whether the artifact at `dest` currently resolves to `entry` —
/// `deployed` for [`list_bins`]. Same ownership rules as
/// [`destroy_bins`]: shim marker label for scripts, alias-prefix for
/// payload links, byte-compare for copies.
fn dest_ownership(
    dest: &Path,
    decl: &BinDecl,
    entry: &StoreEntry,
    label: &str,
    alias: &Path,
) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dest) else {
        return false;
    };
    if decl.shim.is_some() {
        return shim_marker(dest).as_deref() == Some(label);
    }
    if meta.file_type().is_symlink() {
        return links_into_alias(dest, alias);
    }
    if meta.is_file() {
        return resolve_payload_bin(&entry.dir, decl)
            .and_then(|(src, _)| same_file(&src, dest))
            .unwrap_or(false);
    }
    false
}

// ---- shared roots + legacy `bin/` payload unlink ----
//
// The CLI's `bin/` payload links and the declared `bins[]` surfaces
// share one alias root and one bin dir; these helpers let every
// uninstall path (`tamias remove`, `tamias uninstall`, serve's
// `remove` op, the TUI's Remove job — all funnel through
// `tamias_install::remove` or the CLI verbs) run the same teardown.

/// The bin dir deployed surfaces land in — `$TAMIAS_BIN_DIR` →
/// `$XDG_BIN_HOME` → `~/.local/bin` (the freedesktop default).
pub fn default_bin_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TAMIAS_BIN_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_BIN_HOME") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/bin"))
        .unwrap_or_else(|| PathBuf::from(".local/bin"))
}

/// The `bin-alias` root for a store root — a sibling of the store
/// (`<tamias-home>/bin-alias` under `TAMIAS_HOME`), so it survives
/// store entry churn.
pub fn default_alias_root(store_root: &Path) -> PathBuf {
    store_root
        .parent()
        .map_or_else(|| store_root.join("bin-alias"), |p| p.join("bin-alias"))
}

/// Regular files directly under `<entry_dir>/bin/` — non-recursive,
/// dotfiles skipped, sorted. A missing `bin/` yields an empty vec.
/// `file_type` doesn't follow symlinks, so a linked file never
/// qualifies. This is the *legacy* (undeclared) payload surface.
pub fn payload_bin_names(entry_dir: &Path) -> Result<Vec<std::ffi::OsString>, Error> {
    let bin = entry_dir.join("bin");
    let read = match std::fs::read_dir(&bin) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(format!("read {}", bin.display()))(e)),
    };
    let mut out = Vec::new();
    for item in read {
        let item = item.map_err(Error::io(format!("read {}", bin.display())))?;
        let name = item.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let is_file = item.file_type().map(|t| t.is_file()).unwrap_or(false);
        if is_file {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// What [`unlink_payload_bins`] removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnlinkReport {
    /// Bin-dir paths removed.
    pub removed: Vec<String>,
    /// The package's alias path.
    pub alias: PathBuf,
    /// Whether the alias itself was dropped.
    pub alias_removed: bool,
}

/// Remove the bin-dir entries a package's legacy `bin/` payload owns
/// plus its alias — the undeclared half of [`destroy_bins`].
///
/// Candidates come from the payload's `bin/` (when `entry` is still
/// installed) and from scanning the bin dir for links into the
/// package's alias — which also catches files a newer payload
/// version dropped, and works after the store entry is gone
/// (`entry: None`, `name` then the `@scope/name`/`name` verbatim).
/// Copied files (Windows) are only removed when their bytes still
/// match the payload, unless `force`.
///
/// # Errors
/// Filesystem failures; "package is not installed" when `entry` is
/// `None` and neither alias nor links exist.
pub fn unlink_payload_bins(
    name: &str,
    entry: Option<&StoreEntry>,
    bin_dir: &Path,
    alias_root: &Path,
    force: bool,
) -> Result<UnlinkReport, Error> {
    let alias = match entry {
        Some(e) => alias_path(alias_root, &e.record),
        // Uninstalled — the arg is `@scope/name` or `name` verbatim.
        None => alias_root.join(name),
    };
    let mut names = std::collections::BTreeSet::<std::ffi::OsString>::new();
    if let Some(e) = entry {
        for f in payload_bin_names(&e.dir)? {
            names.insert(f);
        }
    }
    // Links into this package's alias — covers payload files since
    // removed and the entry-already-gone case. Anything under the
    // alias is ours (declared `tools/bin` links included), so the
    // prefix is the alias itself, not just `alias/bin`.
    if let Ok(read) = std::fs::read_dir(bin_dir) {
        for item in read.flatten() {
            let Ok(ft) = item.file_type() else {
                continue;
            };
            if !ft.is_symlink() {
                continue;
            }
            let Ok(target) = std::fs::read_link(item.path()) else {
                continue;
            };
            if target.starts_with(&alias) {
                // The bin-dir entry's own name — a declared payload
                // bin's dest name can differ from the payload file's.
                names.insert(item.file_name());
            }
        }
    }
    let alias_meta = std::fs::symlink_metadata(&alias).ok();
    if entry.is_none() && alias_meta.is_none() && names.is_empty() {
        return Err(Error::Failed(format!("package '{name}' is not installed")));
    }
    let mut removed = Vec::new();
    for file in names {
        let dest = bin_dir.join(&file);
        let Ok(meta) = std::fs::symlink_metadata(&dest) else {
            continue;
        };
        let ft = meta.file_type();
        let owned = if ft.is_symlink() {
            std::fs::read_link(&dest)
                .map(|t| t.starts_with(&alias))
                .unwrap_or(false)
        } else if meta.is_file() {
            // A copy (Windows, or a hand copy) — ours only while the
            // bytes still match the payload.
            match entry {
                Some(e) => {
                    let src = e.dir.join("bin").join(&file);
                    src.is_file() && same_file(&src, &dest).unwrap_or(false)
                }
                None => false,
            }
        } else {
            false
        };
        if owned || (force && (ft.is_symlink() || meta.is_file())) {
            std::fs::remove_file(&dest).map_err(Error::io(format!("remove {}", dest.display())))?;
            removed.push(dest.display().to_string());
        } else if ft.is_symlink() || meta.is_file() {
            eprintln!("tamias: leaving {} — not a {name} link", dest.display());
        }
        // directories are never touched
    }
    let mut alias_removed = false;
    if let Some(meta) = &alias_meta {
        let is_link = meta.file_type().is_symlink();
        if is_link || (force && meta.is_dir()) {
            let res = if meta.is_dir() && !is_link {
                std::fs::remove_dir_all(&alias)
            } else {
                std::fs::remove_file(&alias)
            };
            res.map_err(Error::io(format!("remove {}", alias.display())))?;
            alias_removed = true;
        } else if !is_link {
            eprintln!(
                "tamias: leaving alias {} — not a symlink (use --force)",
                alias.display()
            );
        }
    }
    Ok(UnlinkReport {
        removed,
        alias,
        alias_removed,
    })
}

/// Best-effort teardown of everything `deploy_bins` +
/// [`unlink_payload_bins`] put on the bin dir — declared `bins[]`
/// first (their payload links share the alias the unlink removes),
/// then the legacy `bin/` payload. Both halves run even when one
/// fails; errors are joined into one [`Error`]. Call while the store
/// entry still exists so copy-platform byte-compares can read the
/// payload.
///
/// # Errors
/// The first filesystem/manifest failure encountered.
pub fn cleanup_after_remove(
    name: &str,
    entry: &StoreEntry,
    bin_dir: &Path,
    alias_root: &Path,
) -> Result<(), Error> {
    let decls = entry.manifest().map(|m| m.bins).unwrap_or_default();
    let mut first_err = None;
    if let Err(err) = destroy_bins(
        Some(&entry.dir),
        &entry.record,
        &decls,
        bin_dir,
        alias_root,
        false,
    ) {
        first_err = Some(err);
    }
    if let Err(err) = unlink_payload_bins(name, Some(entry), bin_dir, alias_root, false) {
        if first_err.is_none() {
            first_err = Some(err);
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use tamias_core::StoreRecord;

    fn record(name: &str) -> StoreRecord {
        StoreRecord {
            name: name.into(),
            scope: None,
            version: Some("1.0.0".into()),
            kind: None,
            source_ref: "./x".into(),
            resolved: "sha256:abc".into(),
            digest: "sha256:d".into(),
            signature: None,
            signer: None,
            trust: None,
            installed_at: 0,
        }
    }

    fn entry(dir: &Path, name: &str) -> StoreEntry {
        StoreEntry {
            dir: dir.to_path_buf(),
            record: record(name),
        }
    }

    fn payload_decl(name: &str, p: &str) -> BinDecl {
        BinDecl {
            name: name.into(),
            shim: None,
            driver: None,
            payload: Some(p.into()),
        }
    }

    #[allow(dead_code)]
    fn shim_decl(name: &str) -> BinDecl {
        BinDecl {
            name: name.into(),
            shim: Some("cli".into()),
            driver: Some("x".into()),
            payload: None,
        }
    }

    fn manifest_with(bins: Vec<BinDecl>) -> Manifest {
        Manifest {
            name: "pkg".into(),
            version: Some("1.0.0".into()),
            bins,
            ..Default::default()
        }
    }

    #[test]
    fn platform_rule_errors_only_when_dirs_miss_host() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // no tools/bin at all → ok
        assert!(check_bins(root, &manifest_with(vec![])).is_ok());
        // portable file only → ok
        let tb = root.join("tools/bin");
        std::fs::create_dir_all(&tb).unwrap();
        std::fs::write(tb.join("tool"), "x").unwrap();
        assert!(check_bins(root, &manifest_with(vec![])).is_ok());
        // a non-host triple dir → hard error
        let other = tb.join("aarch64-unknown-none");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("tool"), "x").unwrap();
        let err = check_bins(root, &manifest_with(vec![])).unwrap_err();
        assert!(err.to_string().contains("not this platform"), "{err}");
        // host triple dir present → ok
        let host = tb.join(tamias_driver::target_triple());
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("tool"), "x").unwrap();
        assert!(check_bins(root, &manifest_with(vec![])).is_ok());
    }

    #[test]
    fn resolve_payload_bin_literal_and_triple_substitution() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let host = tamias_driver::target_triple();
        let dir = root.join(format!("tools/bin/{host}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tool"), "bin").unwrap();
        // literal host path
        let decl = payload_decl("tool", &format!("tools/bin/{host}/tool"));
        let (src, rel) = resolve_payload_bin(root, &decl).unwrap();
        assert!(src.is_file());
        assert_eq!(rel, PathBuf::from(format!("tools/bin/{host}/tool")));
        // foreign triple → substituted to host
        let decl = payload_decl("tool", "tools/bin/x86_64-pc-windows-msvc/tool");
        let (src, rel) = resolve_payload_bin(root, &decl).unwrap();
        assert!(src.is_file());
        assert_eq!(rel, PathBuf::from(format!("tools/bin/{host}/tool")));
        // portable file
        std::fs::write(root.join("tools/bin/portable"), "x").unwrap();
        let decl = payload_decl("portable", "tools/bin/portable");
        assert!(resolve_payload_bin(root, &decl).is_ok());
        // missing → error listing shipped files
        let decl = payload_decl("ghost", "tools/bin/nowhere/ghost");
        let err = resolve_payload_bin(root, &decl).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn shipped_bins_gate_is_host_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // payload decl but no tools/bin at all → error
        let m = manifest_with(vec![payload_decl("tool", "tools/bin/t/tool")]);
        let err = check_shipped_bins(root, &m).unwrap_err();
        assert!(err.to_string().contains("not shipped"), "{err}");
        // a foreign-triple dir satisfies a decl naming any triple —
        // pack time can't know the install host
        let foreign = root.join("tools/bin/aarch64-apple-darwin");
        std::fs::create_dir_all(foreign.join("sub")).unwrap();
        std::fs::write(foreign.join("tool"), "x").unwrap();
        std::fs::write(foreign.join("sub/nested"), "x").unwrap();
        assert!(check_shipped_bins(root, &m).is_ok());
        // nested rest paths match too
        let m = manifest_with(vec![payload_decl(
            "nested",
            "tools/bin/x86_64-pc-windows-msvc/sub/nested",
        )]);
        assert!(check_shipped_bins(root, &m).is_ok());
        // a portable decl needs the literal portable file — a
        // platform file of the same name does NOT satisfy it
        std::fs::write(root.join("tools/bin/portable"), "x").unwrap();
        let m = manifest_with(vec![payload_decl("p", "tools/bin/portable")]);
        assert!(check_shipped_bins(root, &m).is_ok());
        let m = manifest_with(vec![payload_decl("p2", "tools/bin/missing")]);
        assert!(check_shipped_bins(root, &m).is_err());
        // wrong filename under the right shape → error naming the decl
        let m = manifest_with(vec![payload_decl(
            "ghost",
            "tools/bin/another-triple/ghost",
        )]);
        let err = check_shipped_bins(root, &m).unwrap_err();
        assert!(err.to_string().contains("'ghost'"), "{err}");
        // shim-only manifests pass with no tools/bin at all
        let m = manifest_with(vec![shim_decl("s")]);
        assert!(check_shipped_bins(root, &m).is_ok());
    }

    #[test]
    fn deploy_destroy_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let entry_dir = store.join("pkg@1.0.0");
        let host = tamias_driver::target_triple();
        let tb = entry_dir.join(format!("tools/bin/{host}"));
        std::fs::create_dir_all(&tb).unwrap();
        std::fs::write(tb.join("tool"), "#!/bin/sh\necho tool\n").unwrap();
        std::fs::write(
            entry_dir.join("tamias.yaml"),
            "name: pkg\nversion: 1.0.0\nbins:\n  \
             - name: pkg-shim\n    shim: cli\n    driver: pkg\n  \
             - name: pkg-tool\n    payload: tools/bin/other-triple/pkg-tool\n",
        )
        .unwrap();
        // note: payload path names a foreign triple — resolution must
        // substitute the host triple, but the FILE must exist under the
        // host dir; place it there under the declared file name.
        std::fs::write(tb.join("pkg-tool"), "#!/bin/sh\necho pkg-tool\n").unwrap();
        let bin_dir = tmp.path().join("bin");
        let alias_root = tmp.path().join("bin-alias");
        let e = entry(&entry_dir, "pkg");

        let report = deploy_bins(&e, &bin_dir, &alias_root, false).unwrap();
        assert_eq!(report.actions.len(), 2);
        assert!(report.warnings.is_empty());
        let shim = bin_dir.join("pkg-shim");
        let tool = bin_dir.join("pkg-tool");
        assert!(shim.is_file());
        let script = std::fs::read_to_string(&shim).unwrap();
        assert!(script.contains("# tamias-bin: pkg"), "{script}");
        assert!(
            script.contains("bin exec 'pkg@1.0.0' 'pkg-shim'"),
            "{script}"
        );
        #[cfg(unix)]
        {
            let target = std::fs::read_link(&tool).unwrap();
            assert!(
                target.starts_with(alias_root.join("pkg/tools/bin")),
                "{target:?}"
            );
        }
        // idempotent
        let report = deploy_bins(&e, &bin_dir, &alias_root, false).unwrap();
        assert!(report.actions.iter().all(|a| a.contains("already")));
        // list sees both deployed
        let rows = list_bins(std::slice::from_ref(&e), &bin_dir, &alias_root).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.deployed));
        // destroy removes both
        let manifest = e.manifest().unwrap();
        let d = destroy_bins(
            Some(&entry_dir),
            &e.record,
            &manifest.bins,
            &bin_dir,
            &alias_root,
            false,
        )
        .unwrap();
        assert_eq!(d.removed.len(), 2);
        assert!(!shim.exists() && !tool.exists());
        // destroy post-remove (no entry dir) still gets the shim via marker
        std::fs::remove_dir_all(&entry_dir).unwrap();
        // redeploy needs the payload — skip; marker path covered by shim only
        let d = destroy_bins(
            None,
            &e.record,
            &manifest.bins,
            &bin_dir,
            &alias_root,
            false,
        )
        .unwrap();
        assert!(d.removed.is_empty()); // already gone
    }

    #[test]
    fn collision_warns_and_last_deploy_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        let alias_root = tmp.path().join("bin-alias");
        // two packages declaring the same shim name
        for (dir_name, pkg) in [("a@1.0.0", "a"), ("b@1.0.0", "b")] {
            let dir = tmp.path().join("store").join(dir_name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("tamias.yaml"),
                format!("name: {pkg}\nversion: 1.0.0\nbins:\n  - name: shared\n    shim: cli\n    driver: {pkg}\n"),
            )
            .unwrap();
            let e = entry(&dir, pkg);
            deploy_bins(&e, &bin_dir, &alias_root, false).unwrap();
        }
        let script = std::fs::read_to_string(bin_dir.join("shared")).unwrap();
        assert!(script.contains("# tamias-bin: b"), "{script}");
        // list flags contention; only b is deployed
        let entries = vec![
            entry(&tmp.path().join("store/a@1.0.0"), "a"),
            entry(&tmp.path().join("store/b@1.0.0"), "b"),
        ];
        let rows = list_bins(&entries, &bin_dir, &alias_root).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.contended));
        assert!(
            rows.iter()
                .find(|r| r.package == "b@1.0.0")
                .unwrap()
                .deployed
        );
        assert!(
            !rows
                .iter()
                .find(|r| r.package == "a@1.0.0")
                .unwrap()
                .deployed
        );
    }

    #[test]
    fn foreign_file_conflicts_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let dir = store.join("pkg@1.0.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tamias.yaml"),
            "name: pkg\nversion: 1.0.0\nbins:\n  - name: tool\n    shim: cli\n    driver: pkg\n",
        )
        .unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("tool"), "foreign").unwrap();
        let e = entry(&dir, "pkg");
        assert!(deploy_bins(&e, &bin_dir, &tmp.path().join("ba"), false).is_err());
        deploy_bins(&e, &bin_dir, &tmp.path().join("ba"), true).unwrap();
        assert!(std::fs::read_to_string(bin_dir.join("tool"))
            .unwrap()
            .contains("# tamias-bin: pkg"));
    }
}
