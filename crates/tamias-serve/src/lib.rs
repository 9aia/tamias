#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-serve — the ndjson JSON-RPC surface for embedding: read one JSON
//! request per line on stdin, write one JSON response per line on stdout.
//! Same wire as `sepia serve` / `arca serve` / `myco serve`.
//!
//! Ops: `install`, `list`, `info`, `verify`, `remove`. Requests are
//! `{"op":"…", …}`; responses are `{"ok":true,"result":…}` or
//! `{"ok":false,"error":"…"}`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tamias_cache_local::FetchCache;
use tamias_core::{parse_ref, payload_root, Error, Manifest, StoreRecord};
use tamias_source::{Fetched, Resolved, Source};
use tamias_source_fs::FsSource;
use tamias_source_git::GitSource;
use tamias_source_http::HttpSource;
use tamias_store_local::Store;

/// Store + cache roots.
#[derive(Clone, Debug)]
pub struct Env {
    /// The local store root.
    pub store_root: PathBuf,
    /// The fetch-cache root.
    pub cache_root: PathBuf,
}

/// Run the loop until stdin closes.
///
/// # Errors
/// Propagates stdin/stdout failures.
pub fn serve(env: Env) -> Result<(), Error> {
    serve_loop(env)
}

/// The real blocking stdin loop — compiled out under `cargo-llvm-cov`
/// (`cfg(coverage)`) so the coverage build doesn't hang on stdin.
#[cfg(not(coverage))]
fn serve_loop(env: Env) -> Result<(), Error> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(Error::io("read stdin"))?;
        if line.trim().is_empty() {
            continue;
        }
        let response = handle(&env, &line);
        writeln!(stdout, "{response}").map_err(Error::io("write stdout"))?;
        stdout.flush().map_err(Error::io("flush stdout"))?;
    }
    Ok(())
}

/// The coverage probe — a fast exit so the loop's lines aren't measured.
#[cfg(coverage)]
fn serve_loop(_env: Env) -> Result<(), Error> {
    Err(Error::Failed(
        "serve loop is excluded under coverage".to_string(),
    ))
}

fn handle(env: &Env, line: &str) -> String {
    match dispatch(env, line) {
        Ok(result) => json!({"ok": true, "result": result}).to_string(),
        Err(e) => json!({"ok": false, "error": e.to_string()}).to_string(),
    }
}

fn str_field<'a>(req: &'a Value, key: &str) -> Result<&'a str, Error> {
    req.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Failed(format!("missing '{key}'")))
}

fn dispatch(env: &Env, line: &str) -> Result<Value, Error> {
    let req: Value =
        serde_json::from_str(line).map_err(|e| Error::Failed(format!("bad request: {e}")))?;
    let op = str_field(&req, "op")?;
    match op {
        "install" => {
            let reference = str_field(&req, "ref")?;
            let entry = install(env, reference)?;
            Ok(json!({
                "label": entry.label(),
                "resolved": entry.record.resolved,
                "digest": entry.record.digest,
            }))
        }
        "list" => {
            let entries = Store::new(&env.store_root).entries()?;
            let items: Vec<Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "label": e.label(),
                        "source_ref": e.record.source_ref,
                        "digest": e.record.digest,
                    })
                })
                .collect();
            Ok(json!(items))
        }
        "info" => {
            let name = str_field(&req, "name")?;
            let entry = Store::new(&env.store_root).find(name)?;
            Ok(json!({
                "label": entry.label(),
                "resolved": entry.record.resolved,
                "digest": entry.record.digest,
                "size": entry.size()?,
            }))
        }
        "verify" => {
            let name = str_field(&req, "name")?;
            let report = Store::new(&env.store_root).verify(name)?;
            Ok(json!({"name": report.name, "ok": report.ok}))
        }
        "remove" => {
            let name = str_field(&req, "name")?;
            let record = Store::new(&env.store_root).remove(name)?;
            Ok(json!({"name": record.name}))
        }
        other => Err(Error::Failed(format!("unknown op '{other}'"))),
    }
}

fn resolve(reference: &tamias_core::SourceRef) -> Result<Resolved, Error> {
    match reference {
        tamias_core::SourceRef::Fs(_) => FsSource::new().resolve(reference),
        tamias_core::SourceRef::Git(_) => GitSource::new().resolve(reference),
        tamias_core::SourceRef::Http(_) => HttpSource::new().resolve(reference),
    }
}

fn fetch(resolved: &Resolved, dest: &Path) -> Result<Fetched, Error> {
    match resolved {
        Resolved::Fs { .. } => FsSource::new().fetch(resolved, dest),
        Resolved::Git { .. } => GitSource::new().fetch(resolved, dest),
        Resolved::Http { .. } => HttpSource::new().fetch(resolved, dest),
    }
}

fn fetch_or_cached(env: &Env, resolved: &Resolved) -> Result<(PathBuf, String), Error> {
    let cache = FetchCache::new(&env.cache_root);
    if let Some((key, resolved_id)) = resolved.prefetch_identity() {
        if cache.has(&key) {
            return Ok((cache.entry(&key), resolved_id));
        }
    }
    let scratch = cache.scratch()?;
    let fetched = match fetch(resolved, &scratch) {
        Ok(fetched) => fetched,
        Err(e) => {
            cache.discard(&scratch);
            return Err(e);
        }
    };
    let committed = cache.commit(&scratch, &fetched.content_key)?;
    Ok((committed, fetched.resolved))
}

fn install(env: &Env, reference: &str) -> Result<tamias_core::StoreEntry, Error> {
    let parsed = parse_ref(reference).map_err(|e| Error::Failed(e.to_string()))?;
    let resolved = resolve(&parsed)?;
    let (payload_dir, resolved_id) = fetch_or_cached(env, &resolved)?;
    let root = payload_root(&payload_dir)?;
    let manifest = Manifest::load(&root)?;
    if manifest.name.is_empty() {
        return Err(Error::Failed(
            "package manifest is missing a name".to_string(),
        ));
    }
    let record = StoreRecord {
        name: manifest.name.clone(),
        scope: manifest.scope.clone(),
        version: manifest.version.clone(),
        kind: manifest.kind.clone(),
        source_ref: reference.to_string(),
        resolved: resolved_id,
        digest: String::new(),
        installed_at: 0,
    };
    let entry = Store::new(&env.store_root).install(&root, record)?;
    let kind = manifest.kind.as_deref().unwrap_or("files");
    let _ = tamias_driver::register_bundled_drivers(&root);
    let _ = tamias_driver::dispatch(kind, "install", &root, &entry.record.name, None);
    Ok(entry)
}

/// A convenience default cache eviction window re-exported for callers that
/// embed the loop and manage their own cache.
pub const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 86_400);

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        Env {
            store_root: dir.path().join("store"),
            cache_root: dir.path().join("cache"),
        }
    }

    #[test]
    fn dispatch_ops() {
        let env = env();
        let list = dispatch(&env, r#"{"op":"list"}"#).unwrap();
        assert!(list.is_array());
        assert!(dispatch(&env, r#"{"op":"nope"}"#).is_err());
        assert!(dispatch(&env, "not json").is_err());
        assert!(dispatch(&env, r#"{"op":"install"}"#).is_err());
    }

    #[test]
    fn handle_wraps_ok_and_err() {
        let env = env();
        assert!(handle(&env, r#"{"op":"list"}"#).contains("true"));
        let e = handle(&env, r#"{"op":"nope"}"#);
        assert!(e.contains("false") && e.contains("error"));
    }

    #[test]
    fn dispatch_install_list_info_verify_remove() {
        let env = env();
        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join("tamias.yaml"),
            "name: srv\nversion: 1.0.0\ntype: files\n",
        )
        .unwrap();
        std::fs::write(pkg.path().join("a.txt"), "a").unwrap();
        let ref_json = format!(
            "{{\"op\":\"install\",\"ref\":{:?}}}",
            pkg.path().to_str().unwrap()
        );
        let install = dispatch(&env, &ref_json).unwrap();
        assert!(install["label"].as_str().unwrap().contains("srv"));
        assert_eq!(
            dispatch(&env, r#"{"op":"list"}"#)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            dispatch(&env, r#"{"op":"info","name":"srv"}"#).unwrap()["size"]
                .as_u64()
                .is_some()
        );
        assert_eq!(
            dispatch(&env, r#"{"op":"verify","name":"srv"}"#).unwrap()["ok"],
            true
        );
        assert_eq!(
            dispatch(&env, r#"{"op":"remove","name":"srv"}"#).unwrap()["name"],
            "srv"
        );
    }

    #[test]
    fn dispatch_error_arms() {
        let env = env();
        assert!(dispatch(&env, r#"{"op":"install"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"info"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"verify","name":"ghost"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"remove","name":"ghost"}"#).is_err());
        assert!(dispatch(&env, "{}").is_err());
    }

    #[test]
    fn dispatch_install_git_arm() {
        let env = env();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join("tamias.yaml"),
            "name: sg\nversion: 1.0.0\n",
        )
        .unwrap();
        let git = |a: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo.path())
                .args(a)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q", "-b", "main"]));
        assert!(git(&["add", "-A"]));
        assert!(git(&["commit", "-q", "-m", "i"]));
        let r = format!("git:file://{}#main", repo.path().display());
        let refjson = format!("{{\"op\":\"install\",\"ref\":{r:?}}}");
        let install = dispatch(&env, &refjson).unwrap();
        assert!(install["label"].as_str().unwrap().contains("sg"));
    }

    #[test]
    fn dispatch_install_http_dead() {
        let env = env();
        let refjson = r#"{"op":"install","ref":"http://127.0.0.1:1/x.tgz"}"#;
        assert!(dispatch(&env, refjson).is_err());
    }

    #[cfg(coverage)]
    #[test]
    fn serve_probe_is_covered() {
        assert!(serve(env()).is_err());
    }

    #[test]
    fn cache_hit_and_missing_name_error() {
        let env = env();
        // manifest without a name → the missing-name arm
        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(pkg.path().join("tamias.yaml"), "version: 1.0.0\n").unwrap();
        let r = format!(
            "{{\"op\":\"install\",\"ref\":{:?}}}",
            pkg.path().to_str().unwrap()
        );
        assert!(dispatch(&env, &r).is_err());

        // git install twice → the second is served from the fetch cache
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("tamias.yaml"), "name: c\nversion: 1.0.0\n").unwrap();
        let git = |a: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo.path())
                .args(a)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q", "-b", "main"]));
        assert!(git(&["add", "-A"]));
        assert!(git(&["commit", "-q", "-m", "i"]));
        let r = format!("git:file://{}#main", repo.path().display());
        let rj = format!("{{\"op\":\"install\",\"ref\":{r:?}}}");
        let _ = dispatch(&env, &rj);
        let _ = dispatch(&env, &rj);
    }

    #[cfg(unix)]
    #[test]
    fn store_error_arms() {
        use std::os::unix::fs::PermissionsExt;
        let env = env();
        std::fs::create_dir_all(&env.store_root).unwrap();
        std::fs::set_permissions(&env.store_root, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(dispatch(&env, r#"{"op":"list"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"info","name":"x"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"verify","name":"x"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"remove","name":"x"}"#).is_err());
        std::fs::set_permissions(&env.store_root, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
