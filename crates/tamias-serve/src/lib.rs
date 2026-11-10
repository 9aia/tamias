#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-serve — the ndjson JSON-RPC surface for embedding: read one JSON
//! request per line on stdin, write one JSON response per line on stdout.
//! Same wire as `sepia serve` / `cuora serve` / `myco serve`.
//!
//! Ops: `install`, `sync`, `list`, `info`, `verify`, `remove`, `search`,
//! `publish`, `registry.list`, `registry.get`, `registry.add`,
//! `registry.remove`. Requests are `{"op":"…", …}`; responses are
//! `{"ok":true,"result":…}` or `{"ok":false,"error":"…"}`.
//!
//! `sync` reconciles the *server's* cwd project manifest
//! (`tamias.yaml` `deps:`) against the store + `tamias.lock` — the
//! same `tamias sync` semantics (`{"op":"sync","locked":…,
//! "allow_drivers":…,"dir":…}`; `dir` defaults to `.`). Per-dep
//! outcomes come back in `result.deps`; a bundled-driver consent
//! refusal is an `ok:false` error like `install`'s.
//!
//! `search`/`publish`/`registry.*` resolve named registry sources and
//! config writes through `tamias-registry-open` — the same code the
//! `tamias` CLI verbs use, so the wire surface can't diverge.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tamias_core::Error;
use tamias_store_local::Store;

/// Store + cache + registry roots, plus the config file the
/// registry-aware ops read/write.
#[derive(Clone, Debug)]
pub struct Env {
    /// The local store root.
    pub store_root: PathBuf,
    /// The fetch-cache root.
    pub cache_root: PathBuf,
    /// The default local-registry root (`TAMIAS_REGISTRY`/XDG) — the
    /// target when a `search`/`publish` `registry` value is absent.
    pub registry_root: PathBuf,
    /// Explicit config file for the registry-aware ops — `None` honors
    /// `TAMIAS_CONFIG` then `./tamias.{yml,yaml,toml}` discovery, and
    /// the file is re-read per op so a `registry.add` is visible to a
    /// later `search`/`publish` in the same session.
    pub config_file: Option<PathBuf>,
}

/// The effective config for read-side resolution (`search`/`publish`
/// routing, `install`'s download cap) — mirrors the CLI's
/// warn-and-ignore posture: a broken config warns and yields defaults
/// rather than failing every op. Management ops (`registry.*`) read
/// through `config_target` directly and propagate parse errors.
fn env_config(env: &Env) -> tamias_config::Config {
    match tamias_registry_open::load_config(env.config_file.as_deref()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("tamias-serve: warning — ignoring config file: {e}");
            tamias_config::Config::default()
        }
    }
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
            // The shared install pipeline — silent progress, `"allow_drivers"`
            // consent hints (see `tamias_install::Surface::Rpc`).
            let config = env_config(env);
            let entry = tamias_install::install(
                &tamias_install::Env {
                    store_root: env.store_root.clone(),
                    cache_root: env.cache_root.clone(),
                    // Global `limits.max_download` caps http(s)/`cf:`
                    // fetches — same resolution as the CLI's install.
                    max_download: Some(config.effective_limits(None, None).max_download),
                    // `signing.*` — package-signature verification policy.
                    signing: tamias_sign::SigningPolicy::resolve(Some(&config)),
                },
                reference,
                allow_drivers,
            )?;
            Ok(json!({
                "label": entry.label(),
                "resolved": entry.record.resolved,
                "digest": entry.record.digest,
                "signed": entry.record.signature.is_some(),
            }))
        }
        "sync" => {
            // `tamias sync` — reconcile the project manifest's `deps:`
            // against the store + `tamias.lock`. `dir` (default `.`)
            // is the project dir; fs refs inside `deps:` still resolve
            // against the server's cwd, same as `install`.
            let allow_drivers = req
                .get("allow_drivers")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let locked = req.get("locked").and_then(Value::as_bool).unwrap_or(false);
            let dir = req
                .get("dir")
                .and_then(Value::as_str)
                .map_or_else(|| Path::new("."), Path::new);
            let config = env_config(env);
            let report = tamias_install::sync_project(
                &tamias_install::Env {
                    store_root: env.store_root.clone(),
                    cache_root: env.cache_root.clone(),
                    max_download: Some(config.effective_limits(None, None).max_download),
                    signing: tamias_sign::SigningPolicy::resolve(Some(&config)),
                },
                dir,
                tamias_install::Surface::Rpc,
                tamias_install::SyncOptions {
                    allow_drivers,
                    locked,
                },
                &mut |_| {},
            )?;
            let deps: Vec<Value> = report
                .outcomes
                .iter()
                .map(|o| {
                    json!({
                        "dep": o.dep,
                        "status": o.status.as_str(),
                        "detail": o.detail,
                    })
                })
                .collect();
            Ok(json!({"deps": deps, "failed": report.failed()}))
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
            let store = Store::new(&env.store_root);
            let report = store.verify(name)?;
            // Signature status alongside the digest check — the record's
            // `signature` re-verified against `signing.allowed_signers`.
            let entry = store.find(name)?;
            let config = env_config(env);
            let policy = tamias_sign::SigningPolicy::resolve(Some(&config));
            let signature = tamias_sign::record_signature_status(
                &entry.record,
                policy.allowed_signers.as_deref(),
                &policy.namespace,
            )?
            .as_str();
            Ok(json!({
                "name": report.name,
                "ok": report.ok,
                "signature": signature,
            }))
        }
        "remove" => {
            let name = str_field(&req, "name")?;
            // The shared lightweight uninstall (`tamias_install::remove`)
            // — same teardown rule as `tamias ui`'s Remove job.
            let record = tamias_install::remove(&env.store_root, name, &mut |m| {
                eprintln!("tamias-serve: warning — {m}");
            })?;
            Ok(json!({"name": record.name}))
        }
        // --- registry sources (shared with the CLI verbs) ---
        "registry.list" => {
            let (_path, config) = tamias_registry_open::config_target(env.config_file.as_deref())?;
            let items: Vec<Value> = config
                .registries
                .iter()
                .map(|(name, reg)| tamias_registry_open::registry_json(name, reg))
                .collect();
            Ok(json!({"registries": items}))
        }
        "registry.get" => {
            let name = str_field(&req, "name")?;
            let reg = tamias_registry_open::get_registry(env.config_file.as_deref(), name)?;
            Ok(tamias_registry_open::registry_json(name, &reg))
        }
        "registry.add" => {
            let name = str_field(&req, "name")?;
            let kind = str_field(&req, "kind")?;
            let url = str_field(&req, "url")?;
            let token_env = req
                .get("token_env")
                .and_then(Value::as_str)
                .map(str::to_string);
            let path = tamias_registry_open::add_registry(
                env.config_file.as_deref(),
                name,
                kind,
                url,
                token_env,
            )?;
            Ok(json!({"name": name, "config": path.display().to_string()}))
        }
        "registry.remove" => {
            let name = str_field(&req, "name")?;
            let path = tamias_registry_open::remove_registry(env.config_file.as_deref(), name)?;
            Ok(json!({"name": name, "config": path.display().to_string()}))
        }
        "search" => {
            let query = str_field(&req, "query")?;
            let flag = req.get("registry").and_then(Value::as_str);
            let config = env_config(env);
            // Same routing as `tamias search`: flag → TAMIAS_REGISTRY →
            // search.default → publish.default → local default root —
            // the shared `search` (`tamias-registry-open`).
            let (found, _target) =
                tamias_registry_open::search(&env.registry_root, Some(&config), flag, query)?;
            Ok(json!({"results": found.iter().map(registry_entry_json).collect::<Vec<_>>()}))
        }
        "publish" => {
            let dir = str_field(&req, "dir")?;
            let flag = req.get("registry").and_then(Value::as_str);
            let config = env_config(env);
            // Same routing as `tamias publish`: flag → TAMIAS_REGISTRY →
            // publish.default → local default root — the shared
            // `publish_plan` resolution (`tamias-registry-open`).
            let plan = tamias_registry_open::publish_plan(
                Path::new(dir),
                &env.registry_root,
                Some(&config),
                flag,
                &[config.publish.default.as_deref()],
                &mut |_, _| {},
            )?;
            // `"sign":true` — ssh-keygen -Y sign the provenance anchors;
            // the key resolves like the CLI's `publish --sign`.
            let signature = if req.get("sign").and_then(Value::as_bool).unwrap_or(false) {
                let key = tamias_sign::resolve_signing_key(Some(&config), None)?;
                let policy = tamias_sign::SigningPolicy::resolve(Some(&config));
                Some(tamias_sign::sign_package(
                    &plan.root,
                    &plan.manifest,
                    &key,
                    &policy.namespace,
                )?)
            } else {
                None
            };
            let entry = plan.publish(signature.as_deref())?;
            Ok(registry_entry_json(&entry))
        }
        other => Err(Error::Failed(format!("unknown op '{other}'"))),
    }
}

/// One registry index entry as a JSON object — the `search`/`publish`
/// result shape (same fields the CLI's `--json` prints).
fn registry_entry_json(entry: &tamias_registry::RegistryEntry) -> Value {
    json!({
        "label": entry.label(),
        "name": entry.name,
        "scope": entry.scope,
        "version": entry.version,
        "kind": entry.kind,
        "digest": entry.digest,
        "signed": entry.signature.is_some(),
        "file": entry.file,
    })
}

/// A convenience default cache eviction window re-exported for callers that
/// embed the loop and manage their own cache — 30 days, kept in sync with
/// [`tamias_cache_local::DEFAULT_MAX_AGE`] (dev-dep only here, so the
/// value is spelled out rather than linked).
pub const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 86_400);

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tamias_cache_local::FetchCache;

    use super::*;

    /// Serializes tests that mutate process env (`TAMIAS_HOME`,
    /// `TAMIAS_ALLOW_DRIVERS`, `TAMIAS_REGISTRY`, config env). `cargo
    /// test` runs tests on threads in one process, so mutations must
    /// be serialized *and* restored even on panic.
    static DRVENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard for [`DRVENV`]: restores the process environment and
    /// working directory on drop — panic-safe. Poison is ignored: an
    /// unwinding sibling already restored everything via its `Drop`.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        cwd: std::path::PathBuf,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = DRVENV.lock().unwrap_or_else(|e| e.into_inner());
            let env = std::env::vars_os().collect();
            let cwd = std::env::current_dir().unwrap();
            Self {
                _lock: lock,
                env,
                cwd,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, _) in std::env::vars_os() {
                if !self.env.iter().any(|(sk, _)| *sk == k) {
                    std::env::remove_var(&k);
                }
            }
            for (k, v) in &self.env {
                std::env::set_var(k, v);
            }
            let _ = std::env::set_current_dir(&self.cwd);
        }
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        Env {
            store_root: dir.path().join("store"),
            cache_root: dir.path().join("cache"),
            registry_root: dir.path().join("registry"),
            config_file: None,
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
    fn dispatch_sync_reconciles_cwd_project() {
        let _g = EnvGuard::new();
        let env = env();
        let tmp = tempfile::tempdir().unwrap();
        // a package fixture + a project manifest depending on it
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("tamias.yaml"), "name: sw\nversion: 1.0.0\n").unwrap();
        std::fs::write(pkg.join("a.txt"), "a").unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("tamias.yaml"),
            format!("deps:\n  - {}\n", pkg.display()),
        )
        .unwrap();
        std::env::set_current_dir(&proj).unwrap();
        let res = dispatch(&env, r#"{"op":"sync"}"#).unwrap();
        assert_eq!(res["failed"], 0);
        assert_eq!(res["deps"][0]["status"], "installed");
        // locked now agrees — the dep is current
        let res = dispatch(&env, r#"{"op":"sync","locked":true}"#).unwrap();
        assert_eq!(res["deps"][0]["status"], "current");
        // an explicit dir override works too, and a missing manifest errs
        assert!(dispatch(&env, r#"{"op":"sync","dir":"/no/such/dir"}"#).is_err());
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
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
            registry_root: tmp.path().join("registry"),
            config_file: None,
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
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("TAMIAS_HOME", tmp.path().join("th"));
        std::env::remove_var("TAMIAS_ALLOW_DRIVERS");
        let env = Env {
            store_root: tmp.path().join("store"),
            cache_root: tmp.path().join("cache"),
            registry_root: tmp.path().join("registry"),
            config_file: None,
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
        let _g = EnvGuard::new();
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
            registry_root: tmp.path().join("registry"),
            config_file: None,
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

    /// An `Env` whose `registry.*` ops read/write `tmp/tamias.yml` —
    /// no `TAMIAS_CONFIG`/cwd dependence.
    fn registry_env(tmp: &std::path::Path) -> Env {
        Env {
            store_root: tmp.join("store"),
            cache_root: tmp.join("cache"),
            registry_root: tmp.join("reg-root"),
            config_file: Some(tmp.join("tamias.yml")),
        }
    }

    #[test]
    fn registry_ops_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let env = registry_env(tmp.path());
        // empty to start
        let list = dispatch(&env, r#"{"op":"registry.list"}"#).unwrap();
        assert_eq!(list["registries"].as_array().unwrap().len(), 0);
        // add → list → get → remove
        let reg_dir = tmp.path().join("reg").display().to_string();
        let add = format!(
            "{{\"op\":\"registry.add\",\"name\":\"mine\",\"kind\":\"local\",\"url\":{reg_dir:?},\"token_env\":\"MY_REG_TOKEN\"}}"
        );
        let res = dispatch(&env, &add).unwrap();
        assert_eq!(res["name"], "mine");
        let list = dispatch(&env, r#"{"op":"registry.list"}"#).unwrap();
        let regs = list["registries"].as_array().unwrap();
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0]["name"], "mine");
        assert_eq!(regs[0]["effective_kind"], "local");
        // the env var *name* is exposed; no token value exists to leak
        assert_eq!(regs[0]["token_env"], "MY_REG_TOKEN");
        let get = dispatch(&env, r#"{"op":"registry.get","name":"mine"}"#).unwrap();
        assert_eq!(get["name"], "mine");
        assert_eq!(get["url"], reg_dir);
        // the file on disk round-tripped through the YAML renderer
        let text = std::fs::read_to_string(tmp.path().join("tamias.yml")).unwrap();
        assert!(
            text.contains("registries:") && text.contains("mine"),
            "{text}"
        );
        // duplicate add + unknown get/remove error
        assert!(dispatch(&env, &add).is_err());
        assert!(dispatch(&env, r#"{"op":"registry.get","name":"ghost"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"registry.remove","name":"ghost"}"#).is_err());
        let rem = dispatch(&env, r#"{"op":"registry.remove","name":"mine"}"#).unwrap();
        assert_eq!(rem["name"], "mine");
        assert!(dispatch(&env, r#"{"op":"registry.get","name":"mine"}"#).is_err());
    }

    #[test]
    fn registry_op_error_arms() {
        let tmp = tempfile::tempdir().unwrap();
        let env = registry_env(tmp.path());
        assert!(dispatch(&env, r#"{"op":"registry.get"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"registry.add","name":"x","kind":"local"}"#).is_err());
        assert!(dispatch(
            &env,
            r#"{"op":"registry.add","name":"x","kind":"bogus","url":"/y"}"#
        )
        .is_err());
        assert!(dispatch(
            &env,
            r#"{"op":"registry.add","name":"x","kind":"http","url":"/y"}"#
        )
        .is_err());
        assert!(dispatch(&env, r#"{"op":"registry.remove"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"search"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"publish"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"publish","dir":"/nonexistent-xyz"}"#).is_err());
    }

    #[test]
    fn publish_search_route_by_name_and_raw_ref() {
        let _g = EnvGuard::new();
        let saved = std::env::var_os("TAMIAS_REGISTRY");
        std::env::remove_var("TAMIAS_REGISTRY");
        let tmp = tempfile::tempdir().unwrap();
        let env = registry_env(tmp.path());
        // a payload package + a named local registry
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("tamias.yaml"), "name: wpkg\nversion: 1.0.0\n").unwrap();
        std::fs::write(pkg.join("a.txt"), "a").unwrap();
        let mine_reg = tmp.path().join("mine-reg");
        let add = format!(
            "{{\"op\":\"registry.add\",\"name\":\"mine\",\"kind\":\"local\",\"url\":{:?}}}",
            mine_reg.display().to_string()
        );
        assert!(dispatch(&env, &add).is_ok());
        // publish by registry *name*
        let pub_by_name = format!(
            "{{\"op\":\"publish\",\"dir\":{:?},\"registry\":\"mine\"}}",
            pkg.display().to_string()
        );
        let published = dispatch(&env, &pub_by_name).unwrap();
        assert_eq!(published["name"], "wpkg");
        assert!(mine_reg.join("index.json").is_file());
        // search by name → the published entry
        let found = dispatch(&env, r#"{"op":"search","query":"wpkg","registry":"mine"}"#).unwrap();
        let results = found["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], "wpkg");
        assert_eq!(results[0]["version"], "1.0.0");
        // search by raw dir ref → same registry, same result
        let raw = mine_reg.display().to_string();
        let by_ref = format!("{{\"op\":\"search\",\"query\":\"wpkg\",\"registry\":{raw:?}}}");
        let found = dispatch(&env, &by_ref).unwrap();
        assert_eq!(found["results"].as_array().unwrap().len(), 1);
        // no registry → the env default root: the bare publish landed
        // there, so the bare search finds that copy (not "mine"'s)
        let bare = format!(
            "{{\"op\":\"publish\",\"dir\":{:?}}}",
            pkg.display().to_string()
        );
        assert!(dispatch(&env, &bare).is_ok());
        assert!(env.registry_root.join("index.json").is_file());
        let found = dispatch(&env, r#"{"op":"search","query":"wpkg"}"#).unwrap();
        assert_eq!(found["results"].as_array().unwrap().len(), 1);
        match saved {
            Some(v) => std::env::set_var("TAMIAS_REGISTRY", v),
            None => std::env::remove_var("TAMIAS_REGISTRY"),
        }
    }
}
