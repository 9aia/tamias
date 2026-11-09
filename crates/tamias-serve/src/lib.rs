#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-serve — the ndjson JSON-RPC surface for embedding: read one JSON
//! request per line on stdin, write one JSON response per line on stdout.
//! Same wire as `sepia serve` / `cuora serve` / `myco serve`.
//!
//! Ops: `install`, `list`, `info`, `verify`, `remove`. Requests are
//! `{"op":"…", …}`; responses are `{"ok":true,"result":…}` or
//! `{"ok":false,"error":"…"}`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tamias_cache_local::FetchCache;
use tamias_core::{payload_root, Error, Manifest, StoreRecord};
use tamias_source::{Planned, Refs, Sources};
use tamias_source_cf::{CfRefKind, CfSource};
use tamias_source_fs::{FsRefKind, FsSource};
use tamias_source_git::{GitRefKind, GitSource};
use tamias_source_http::{HttpRefKind, HttpSource};
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
            // Optional consent for payload-shipped `drivers/` — same rule as
            // `tamias install --allow-drivers` / `TAMIAS_ALLOW_DRIVERS=1`.
            let allow_drivers = req
                .get("allow_drivers")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let entry = install(env, reference, allow_drivers)?;
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
            let store = Store::new(&env.store_root);
            let entry = store.find(name)?;
            let kind = entry.record.kind.as_deref().unwrap_or("files");
            // Same teardown rule as `tamias uninstall`: a failing driver
            // keeps the store entry; a missing one warns loudly.
            if !tamias_driver::dispatch(kind, "uninstall", &entry.dir, &entry.record.name, None)?
                && kind != "files"
            {
                eprintln!(
                    "tamias-serve: warning — no driver for type '{kind}'; \
                     provider resources may remain"
                );
            }
            let record = store.remove(name)?;
            Ok(json!({"name": record.name}))
        }
        other => Err(Error::Failed(format!("unknown op '{other}'"))),
    }
}

/// The ref grammar — kinds in claim order; the fs fallback (paths) is last.
fn refs() -> Refs {
    Refs::new()
        .with(Arc::new(GitRefKind))
        .with(Arc::new(CfRefKind))
        .with(Arc::new(HttpRefKind))
        .with(Arc::new(FsRefKind))
}

/// The source registry.
fn sources() -> Sources {
    Sources::new()
        .with(Arc::new(GitSource::new()))
        .with(Arc::new(CfSource::new()))
        .with(Arc::new(HttpSource::new()))
        .with(Arc::new(FsSource::new()))
}

fn fetch_or_cached(env: &Env, planned: &Planned) -> Result<(PathBuf, String), Error> {
    let cache = FetchCache::new(&env.cache_root);
    if let Some((key, resolved_id)) = planned.prefetch_identity() {
        // Integrity-checked hit — a tampered or pre-metadata entry is
        // evicted inside `hit` and reported as a miss → refetch.
        if let Some(dir) = cache.hit(&key)? {
            return Ok((dir, resolved_id));
        }
    }
    let scratch = cache.scratch()?;
    let fetched = match planned.fetch(&scratch) {
        Ok(fetched) => fetched,
        Err(e) => {
            cache.discard(&scratch);
            return Err(e);
        }
    };
    let committed = cache.commit(&scratch, &fetched.content_key)?;
    Ok((committed, fetched.resolved))
}

fn install(
    env: &Env,
    reference: &str,
    allow_drivers: bool,
) -> Result<tamias_core::StoreEntry, Error> {
    let span = tracing::info_span!("install", reference);
    let _enter = span.enter();
    let spec = refs().parse(reference)?;
    let planned = sources().plan(&*spec)?;
    let (payload_dir, resolved_id) = fetch_or_cached(env, &planned)?;
    let root = payload_root(&payload_dir)?;
    let manifest = Manifest::load(&root)?;
    if manifest.name.is_empty() {
        return Err(Error::Failed(
            "package manifest is missing a name".to_string(),
        ));
    }
    // Bundled drivers are arbitrary code shipped inside the package — refuse
    // before the store entry is written unless the caller consented via
    // `allow_drivers` or `TAMIAS_ALLOW_DRIVERS=1`.
    let shipped = tamias_driver::bundled_drivers(&root)?;
    if !shipped.is_empty() && !tamias_driver::drivers_allowed(allow_drivers) {
        return Err(Error::Failed(format!(
            "package ships bundled driver(s): {} — refusing to install + run them; \
             pass \"allow_drivers\":true or set TAMIAS_ALLOW_DRIVERS=1 to consent",
            shipped.join(", ")
        )));
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
    // Same semantics as `tamias install`: driver registration + dispatch
    // run *before* the store commit, so a missing or failing driver fails
    // the install with no committed entry.
    let kind = manifest.kind.as_deref().unwrap_or("files");
    tamias_driver::register_bundled_drivers(&root)?;
    if !tamias_driver::dispatch(kind, "install", &root, &record.name, None)? && kind != "files" {
        return Err(Error::Failed(format!(
            "no driver for content type '{kind}' — install `tamias-driver-{kind}` on PATH \
             or ship one in the package's `drivers/` dir{}",
            if shipped.is_empty() {
                String::new()
            } else {
                "; bundled drivers need \"allow_drivers\":true or TAMIAS_ALLOW_DRIVERS=1"
                    .to_string()
            }
        )));
    }
    Store::new(&env.store_root).install(&root, record)
}

/// A convenience default cache eviction window re-exported for callers that
/// embed the loop and manage their own cache.
pub const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 86_400);

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Serializes tests that mutate `TAMIAS_HOME`/`TAMIAS_ALLOW_DRIVERS`.
    static DRVENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    #[test]
    fn corrupt_cache_entry_refetches() {
        let env = env();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join("tamias.yaml"),
            "name: cc\nversion: 1.0.0\n",
        )
        .unwrap();
        std::fs::write(repo.path().join("a.txt"), "good").unwrap();
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
        assert!(dispatch(&env, &rj).is_ok());
        // poison the cache entry → the next install must reject the hit,
        // evict, refetch, and store the good bytes — never "evil".
        let entries = FetchCache::new(&env.cache_root).entries().unwrap();
        assert_eq!(entries.len(), 1);
        std::fs::write(entries[0].join("a.txt"), "evil").unwrap();
        assert!(dispatch(&env, &rj).is_ok());
        let entry = Store::new(&env.store_root).find("cc").unwrap();
        assert_eq!(
            std::fs::read_to_string(entry.dir.join("a.txt")).unwrap(),
            "good"
        );
        assert_eq!(
            dispatch(&env, r#"{"op":"verify","name":"cc"}"#).unwrap()["ok"],
            true
        );
    }

    #[test]
    fn install_gates_bundled_drivers() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
        };
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(pkg.join("drivers")).unwrap();
        std::fs::write(pkg.join("tamias.yaml"), "name: sd\nversion: 1.0.0\n").unwrap();
        std::fs::write(
            pkg.join("drivers/tamias-driver-files"),
            "#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        let r = format!("{{\"op\":\"install\",\"ref\":{:?}}}", pkg.to_str().unwrap());
        // no consent → error, no store entry, nothing registered
        assert!(dispatch(&env, &r).is_err());
        assert!(Store::new(&env.store_root).find("sd").is_err());
        assert!(!tmp.path().join("th/drivers/tamias-driver-files").exists());
        // allow_drivers param → proceeds
        let rc = format!(
            "{{\"op\":\"install\",\"ref\":{:?},\"allow_drivers\":true}}",
            pkg.to_str().unwrap()
        );
        assert!(dispatch(&env, &rc).is_ok());
        assert!(tmp.path().join("th/drivers/tamias-driver-files").is_file());
        // env consent works too
        std::env::set_var("TAMIAS_ALLOW_DRIVERS", "1");
        assert!(dispatch(&env, &r).is_ok());
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        // a plain files package is unaffected — no consent needed
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("tamias.yaml"), "name: sp\nversion: 1.0.0\n").unwrap();
        let rp = format!(
            "{{\"op\":\"install\",\"ref\":{:?}}}",
            plain.to_str().unwrap()
        );
        assert!(dispatch(&env, &rp).is_ok());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn install_unknown_kind_fails_without_entry() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
        };
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("tamias.yaml"),
            "name: su\nversion: 1.0.0\ntype: zzz-serve-no-driver\n",
        )
        .unwrap();
        std::fs::write(pkg.join("a.txt"), "a").unwrap();
        let r = format!("{{\"op\":\"install\",\"ref\":{:?}}}", pkg.to_str().unwrap());
        let err = dispatch(&env, &r).unwrap_err();
        assert!(err.to_string().contains("zzz-serve-no-driver"), "{err}");
        assert!(Store::new(&env.store_root).find("su").is_err());
        std::env::remove_var("TAMIAS_HOME");
    }

    #[test]
    fn remove_propagates_driver_failure() {
        let _g = DRVENV.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let dd = tmp.path().join("th/drivers");
        std::fs::create_dir_all(&dd).unwrap();
        let driver = dd.join("tamias-driver-srvflaky");
        std::fs::write(
            &driver,
            "#!/bin/sh\ncase \"$1\" in\n  uninstall) exit 1;;\n  *) exit 0;;\nesac\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
        };
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("tamias.yaml"),
            "name: sf\nversion: 1.0.0\ntype: srvflaky\n",
        )
        .unwrap();
        std::fs::write(pkg.join("a.txt"), "a").unwrap();
        let r = format!("{{\"op\":\"install\",\"ref\":{:?}}}", pkg.to_str().unwrap());
        assert!(dispatch(&env, &r).is_ok());
        // driver teardown fails → remove errors, entry kept
        assert!(dispatch(&env, r#"{"op":"remove","name":"sf"}"#).is_err());
        assert_eq!(
            dispatch(&env, r#"{"op":"list"}"#)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        std::env::remove_var("TAMIAS_HOME");
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
