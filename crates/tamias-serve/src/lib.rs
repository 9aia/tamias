#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-serve — the ndjson JSON-RPC surface for embedding: read one JSON
//! request per line on stdin, write one JSON response per line on stdout.
//! Same wire as `sepia serve` / `cuora serve` / `myco serve`.
//!
//! Ops: `install`, `plan`, `sync`, `list`, `info`, `verify`, `remove`,
//! `search`, `publish`, `registry.list`, `registry.get`, `registry.add`,
//! `registry.remove`, `driver.list`, `driver.manifest`, `driver.query`,
//! `driver.exec`. Requests are `{"op":"…", …}`; responses are
//! `{"ok":true,"result":…}` or `{"ok":false,"error":"…"}`.
//!
//! `plan` is `tamias install --plan` — `{"op":"plan","ref":…,
//! "allow_drivers":…}` fetches/plans and reports what a real install
//! would do (includes, drivers, provider services, trust tier,
//! would-be problems) without committing or dispatching.
//!
//! `sync` reconciles the *server's* cwd project dep set against the
//! store + `tamias.lock` — the same `tamias sync` semantics
//! (`{"op":"sync","locked":…,"from_lock":…,"allow_drivers":…,
//! "dir":…}`; `dir` defaults to `.`). `from_lock: true` treats
//! `tamias.lock` itself as the spec (no manifest consult, no lock
//! rewrite). Per-dep outcomes come back in `result.deps`; a
//! bundled-driver consent refusal is an `ok:false` error like
//! `install`'s.
//!
//! `search`/`publish`/`registry.*` resolve named registry sources and
//! config writes through `tamias-registry-open` — the same code the
//! `tamias` CLI verbs use, so the wire surface can't diverge.
//!
//! `driver.*` ops (P-096) proxy installed `tamias-driver-<name>`
//! binaries so orchestrators can drive them through this wire:
//! `driver.list` reports every discovered driver (`{"drivers":[{name,
//! path, manifest|error}]}` — a per-driver `capabilities` failure
//! degrades to an `error` field, never fails the op);
//! `driver.manifest` fetches one driver's capabilities JSON;
//! `driver.query` spawns `<bin> serve`, writes ONE
//! `{"op":"query","query":…,"params":{…}}` line and returns the
//! driver's single response line verbatim — a driver-side `ok:false`
//! propagates as an op error; `driver.exec` runs `<bin> <argv…>`
//! verbatim and reports `{code, stdout, stderr, json}` (`json` is the
//! parsed stdout or null). `driver` names are content-type kinds —
//! the same `[a-z0-9._-]` charset [`tamias_core::is_valid_kind`]
//! enforces, so hostile names never reach argv or path joins.
//! `timeout_ms` (default 120s, clamped to 30min) bounds `driver.query`
//! and `driver.exec`. Driver sessions are spawn-per-call — pooled
//! sessions are a follow-up.
//!
//! **Trust boundary.** Stdin is fully trusted: `install` accepts
//! `allow_drivers:true` (bundled-driver consent), and
//! `publish`/`registry.add`/`registry.remove` mutate config and the
//! registry. Embedders must treat the pipe as a privileged channel —
//! never expose it to untrusted input.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
                    bin_dir: None,
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
        "plan" => {
            // `tamias install --plan` — the dry-run over the same
            // store/cache roots + config resolution. Reports what a
            // real install would do; commits and dispatches nothing.
            let reference = str_field(&req, "ref")?;
            let allow_drivers = req
                .get("allow_drivers")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let plan = tamias_install::plan_install(
                &tamias_install::Env::for_config(
                    env.store_root.clone(),
                    env.cache_root.clone(),
                    Some(&env_config(env)),
                ),
                reference,
                allow_drivers,
            )?;
            Ok(serde_json::to_value(&plan).map_err(|e| Error::Failed(e.to_string()))?)
        }
        "sync" => {
            // `tamias sync` — reconcile the project dep set against
            // the store + `tamias.lock`. `dir` (default `.`) is the
            // project dir; fs refs inside `deps:` still resolve
            // against the server's cwd, same as `install`.
            // `from_lock: true` swaps the spec to `tamias.lock`
            // itself (the manifest's `deps:` is not consulted).
            let allow_drivers = req
                .get("allow_drivers")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let locked = req.get("locked").and_then(Value::as_bool).unwrap_or(false);
            let from_lock = req
                .get("from_lock")
                .and_then(Value::as_bool)
                .unwrap_or(false);
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
                    bin_dir: None,
                },
                dir,
                tamias_install::Surface::Rpc,
                tamias_install::SyncOptions {
                    allow_drivers,
                    locked,
                    source: if from_lock {
                        tamias_install::SyncSource::Lock
                    } else {
                        tamias_install::SyncSource::Deps
                    },
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
                        "description": e.record.description,
                    })
                })
                .collect();
            Ok(json!(items))
        }
        "info" => {
            let name = str_field(&req, "name")?;
            let entry = Store::new(&env.store_root).find(name)?;
            // `sealed:` format when the payload manifest declares a
            // ciphertext-only pack — advisory badge, never a gate.
            let sealed = entry
                .manifest()
                .ok()
                .and_then(|m| m.sealed)
                .map(|s| s.format);
            Ok(json!({
                "label": entry.label(),
                "resolved": entry.record.resolved,
                "digest": entry.record.digest,
                "description": entry.record.description,
                "keywords": entry.record.keywords,
                "homepage": entry.record.homepage,
                "repository": entry.record.repository,
                "license": entry.record.license,
                "authors": entry.record.authors,
                "sealed": sealed,
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
            // Trust tier + signer re-derived live (same rule as the
            // CLI's `verify`) — recorded `signer` wins when present.
            let (trust, live_signer) = tamias_sign::trust_and_signer(
                &entry.record.source_ref,
                entry.record.signature.as_deref(),
                &entry.record.name,
                entry.record.scope.as_deref(),
                entry.record.version.as_deref(),
                &entry.record.digest,
                &policy,
            )?;
            let signer = entry.record.signer.clone().or(live_signer);
            Ok(json!({
                "name": report.name,
                "ok": report.ok,
                "signature": signature,
                "trust": trust.as_str(),
                "signer": signer,
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
            let id = req.get("id").and_then(Value::as_str).map(str::to_string);
            let path = tamias_registry_open::add_registry(
                env.config_file.as_deref(),
                name,
                kind,
                url,
                token_env,
                id,
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
            // `"keep_includes":true` ships the raw payload — the
            // default flattens `include:` bases read-only.
            let keep_includes = req
                .get("keep_includes")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // `"keep":<n>` — per-invocation retention override (P-024);
            // absent → `retention.keep` config.
            let keep = req.get("keep").and_then(Value::as_u64);
            let plan = tamias_registry_open::publish_plan(
                Path::new(dir),
                &env.registry_root,
                Some(&config),
                flag,
                &[config.publish.default.as_deref()],
                &env.store_root,
                keep_includes,
                keep,
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
            let outcome = plan.publish(signature.as_deref())?;
            // `pruned` — index entries the retention policy dropped
            // (P-024); payloads stay.
            let mut out = registry_entry_json(&outcome.entry);
            out["pruned"] = json!(outcome.pruned);
            Ok(out)
        }
        // --- driver proxy ops (P-096) ---
        "driver.list" => driver_list(),
        "driver.manifest" => {
            let (_name, path) = resolve_driver(&req)?;
            Ok(tamias_driver::capabilities(&path)?)
        }
        "driver.query" => {
            let (name, path) = resolve_driver(&req)?;
            let query = str_field(&req, "query")?;
            let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
            if !params.is_object() {
                return Err(Error::Failed("'params' must be an object".to_string()));
            }
            let request = json!({"op": "query", "query": query, "params": params});
            let response = driver_serve_once(&path, &request, driver_timeout(&req))
                .map_err(|e| Error::Failed(format!("driver '{name}': {e}")))?;
            // The driver's response line comes back verbatim on
            // `ok:true` — `{"ok":true,"result":{…}}` including the
            // pavo `QueryResult` envelope. A driver-side `ok:false`
            // is this op's error, with the driver's message.
            match response.get("ok").and_then(Value::as_bool) {
                Some(true) => Ok(response),
                Some(false) => Err(Error::Failed(format!(
                    "driver '{name}' query '{query}' failed: {}",
                    response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                ))),
                _ => Err(Error::Failed(format!(
                    "driver '{name}' returned a malformed response: {response}"
                ))),
            }
        }
        "driver.exec" => {
            let (name, path) = resolve_driver(&req)?;
            let argv = req
                .get("argv")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Failed("missing 'argv'".to_string()))?;
            let mut args = Vec::with_capacity(argv.len());
            for v in argv {
                args.push(
                    v.as_str()
                        .ok_or_else(|| Error::Failed("'argv' must be strings".to_string()))?
                        .to_string(),
                );
            }
            let mut cmd = std::process::Command::new(&path);
            // stdin null — the serve loop owns stdin; an exec'd
            // driver must never eat wire lines.
            cmd.args(&args).stdin(Stdio::null());
            let out = tamias_core::output_with_timeout(&mut cmd, driver_timeout(&req))
                .map_err(|e| Error::Failed(format!("driver '{name}': {e}")))?;
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            Ok(json!({
                "code": out.status.code().unwrap_or(-1),
                "stdout": stdout,
                "stderr": String::from_utf8_lossy(&out.stderr),
                "json": serde_json::from_str::<Value>(stdout.trim()).ok(),
            }))
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
        "description": entry.description,
        "keywords": entry.keywords,
        "homepage": entry.homepage,
        "repository": entry.repository,
        "license": entry.license,
        "authors": entry.authors,
        "digest": entry.digest,
        "signed": entry.signature.is_some(),
        "publisher": entry.publisher,
        "file": entry.file,
    })
}

// --- driver proxy helpers (P-096) ---

/// Default bound for `driver.query`/`driver.exec` — the same 120s the
/// driver's own lifecycle dispatch uses (`tamias-driver`'s
/// `DISPATCH_TIMEOUT` is private to that crate).
const DRIVER_OP_TIMEOUT: Duration = Duration::from_secs(120);

/// `timeout_ms` ceiling — a wire client can't park the serve loop
/// forever (or overflow the deadline arithmetic) with an absurd
/// value.
const DRIVER_OP_TIMEOUT_MAX_MS: u64 = 30 * 60 * 1000;

/// Every discovered `tamias-driver-*` binary with its capabilities
/// manifest — `{"drivers":[{name, path, manifest}]}`. A driver whose
/// `capabilities` probe fails reports `"error"` instead of `manifest`
/// and never fails the op (same tolerance `tamias drivers` has for
/// unreachable drivers).
fn driver_list() -> Result<Value, Error> {
    let drivers: Vec<Value> = tamias_driver::discovered_drivers()
        .into_iter()
        .map(|(name, path)| match tamias_driver::capabilities(&path) {
            Ok(manifest) => json!({
                "name": name,
                "path": path.display().to_string(),
                "manifest": manifest,
            }),
            Err(e) => json!({
                "name": name,
                "path": path.display().to_string(),
                "error": e.to_string(),
            }),
        })
        .collect();
    Ok(json!({"drivers": drivers}))
}

/// Resolve `"driver":"<name>"` to its binary. The name is a
/// content-type kind — [`tamias_core::is_valid_kind`] keeps hostile
/// values (`../`, separators, uppercase) away from argv/path joins;
/// [`tamias_driver::find_driver`] then does the PATH/`<tamias-home>`
/// lookup plus its capabilities-fallback scan.
fn resolve_driver(req: &Value) -> Result<(String, PathBuf), Error> {
    let name = str_field(req, "driver")?;
    if !tamias_core::is_valid_kind(name) {
        return Err(Error::Failed(format!("invalid driver name {name:?}")));
    }
    let path = tamias_driver::find_driver(name)
        .ok_or_else(|| Error::Failed(format!("no driver '{name}' found")))?;
    Ok((name.to_string(), path))
}

/// `timeout_ms` param → bounded [`Duration`] (default
/// [`DRIVER_OP_TIMEOUT`], clamped so a wire client can't park the
/// loop or overflow the deadline arithmetic).
fn driver_timeout(req: &Value) -> Duration {
    req.get("timeout_ms")
        .and_then(Value::as_u64)
        .map(|ms| Duration::from_millis(ms.clamp(1, DRIVER_OP_TIMEOUT_MAX_MS)))
        .unwrap_or(DRIVER_OP_TIMEOUT)
}

/// The `driver.query` transport: spawn `<bin> serve`, write ONE
/// request line, read ONE response line. Spawn-per-call is the v1
/// contract — pooled driver sessions are a follow-up. stdin closes
/// after the request so a well-behaved serve loop answers then exits
/// on EOF; the child is killed once the response line lands (or at
/// the deadline) so a driver that ignores EOF can't leak.
fn driver_serve_once(bin: &Path, request: &Value, timeout: Duration) -> Result<Value, Error> {
    let line = serde_json::to_string(request)
        .map_err(|e| Error::Failed(format!("encode driver request: {e}")))?;
    let mut child = std::process::Command::new(bin)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Failed(format!("spawn {} serve: {e}", bin.display())))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A dead-on-arrival child makes the write fail — that surfaces
        // as "exited without a response" below, no need to fail early.
        let _ = writeln!(stdin, "{line}");
    }
    // stderr drains on a thread so a chatty driver can't wedge the
    // wait loop on a full pipe (same rule as `output_with_timeout`).
    let stderr = child.stderr.take();
    let stderr_thr = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stderr {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    // stdout streams lines over a channel — the FIRST line is the
    // response; we don't wait for process exit (a serve loop may keep
    // running after answering).
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<String>();
    let stdout_thr = thread::spawn(move || {
        let Some(stdout) = stdout else { return };
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.trim_end().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let deadline = Instant::now() + timeout;
    let outcome = loop {
        let now = Instant::now();
        if now >= deadline {
            break Err(Error::Failed(format!(
                "{} serve timed out after {timeout:?}",
                bin.display()
            )));
        }
        match rx.recv_timeout(deadline - now) {
            Ok(line) => {
                break serde_json::from_str::<Value>(&line).map_err(|e| {
                    Error::Failed(format!("{} serve: bad response line: {e}", bin.display()))
                })
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(Error::Failed(format!(
                    "{} serve: exited without a response",
                    bin.display()
                )));
            }
        }
    };
    // Reap: kill a still-running child (the response is already in
    // hand, or the deadline hit), then collect stderr for error
    // context.
    let _ = child.kill();
    let _ = child.wait();
    let _ = stdout_thr.join();
    let stderr_bytes = stderr_thr.join().unwrap_or_default();
    match outcome {
        Ok(v) => Ok(v),
        Err(e) => {
            let tail = String::from_utf8_lossy(&stderr_bytes);
            let tail = tail.trim();
            if tail.is_empty() {
                Err(e)
            } else {
                Err(Error::Failed(format!("{e} — stderr: {tail}")))
            }
        }
    }
}

/// A convenience default cache eviction window re-exported for callers that
/// embed the loop and manage their own cache — 30 days, kept in sync with
/// `tamias_cache_local::DEFAULT_MAX_AGE` (dev-dep only here, so the
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
        // EnvGuard: `driver.list` reads PATH/TAMIAS_HOME, which
        // sibling tests mutate — freeze env for the duration.
        let _g = EnvGuard::new();
        let env = env();
        let list = dispatch(&env, r#"{"op":"list"}"#).unwrap();
        assert!(list.is_array());
        assert!(dispatch(&env, r#"{"op":"nope"}"#).is_err());
        assert!(dispatch(&env, "not json").is_err());
        assert!(dispatch(&env, r#"{"op":"install"}"#).is_err());
        // driver.list never fails — an empty discovery is `[]`.
        assert_eq!(
            dispatch(&env, r#"{"op":"driver.list"}"#).unwrap()["drivers"]
                .as_array()
                .unwrap()
                .len(),
            tamias_driver::discovered_drivers().len()
        );
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
    fn dispatch_install_deploys_declared_bins() {
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        // The serve `Env` carries no bin dir — the deploy resolves
        // `TAMIAS_BIN_DIR` like the CLI does.
        let bindir = tmp.path().join("bin");
        std::env::set_var("TAMIAS_BIN_DIR", &bindir);
        let env = env();
        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join("tamias.yaml"),
            "name: swb\nversion: 1.0.0\nbins:\n  \
             - name: swb-cli\n    shim: cli\n    driver: files\n",
        )
        .unwrap();
        std::fs::write(pkg.path().join("a.txt"), "a").unwrap();
        let req = format!(
            "{{\"op\":\"install\",\"ref\":{:?}}}",
            pkg.path().to_str().unwrap()
        );
        dispatch(&env, &req).unwrap();
        // T-027: the install op runs the same post-commit bins[] deploy.
        let shim = bindir.join("swb-cli");
        assert!(shim.is_file(), "serve install must deploy declared bins[]");
        assert!(std::fs::read_to_string(&shim)
            .unwrap()
            .contains("# tamias-bin: swb"));
        std::env::remove_var("TAMIAS_BIN_DIR");
    }

    #[test]
    fn dispatch_plan_is_a_dry_run() {
        let env = env();
        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join("tamias.yaml"),
            "name: srv\nversion: 1.0.0\ntype: files\n",
        )
        .unwrap();
        std::fs::write(pkg.path().join("a.txt"), "a").unwrap();
        let req = format!(
            "{{\"op\":\"plan\",\"ref\":{:?}}}",
            pkg.path().to_str().unwrap()
        );
        let plan = dispatch(&env, &req).unwrap();
        assert_eq!(plan["manifest"]["name"], "srv");
        assert_eq!(plan["manifest"]["version"], "1.0.0");
        assert!(plan["payload_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        assert!(plan["trust"].as_str().is_some());
        // nothing committed — the store stays empty
        assert_eq!(
            dispatch(&env, r#"{"op":"list"}"#)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(dispatch(&env, r#"{"op":"plan"}"#).is_err());
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
    fn dispatch_sync_from_lock_uses_lock_as_spec() {
        let _g = EnvGuard::new();
        let env = env();
        let tmp = tempfile::tempdir().unwrap();
        // a package fixture + a project dir holding ONLY tamias.lock
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("tamias.yaml"), "name: sw\nversion: 1.0.0\n").unwrap();
        std::fs::write(pkg.join("a.txt"), "a").unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        // seed the lock via a deps-mode sync against a scratch env is
        // overkill — install + upsert the record directly
        let seed_env = Env {
            store_root: tmp.path().join("seed-store"),
            cache_root: tmp.path().join("seed-cache"),
            ..env.clone()
        };
        let entry = tamias_install::install_with_progress(
            &tamias_install::Env {
                store_root: seed_env.store_root.clone(),
                cache_root: seed_env.cache_root.clone(),
                max_download: None,
                signing: tamias_sign::SigningPolicy::default(),
                bin_dir: None,
            },
            &pkg.display().to_string(),
            false,
            tamias_install::Surface::Rpc,
            &mut |_| {},
        )
        .unwrap();
        tamias_core::Lock::upsert_record(&proj.join("tamias.lock"), &entry.record).unwrap();
        // a manifest whose deps: would fail if consulted
        std::fs::write(proj.join("tamias.yaml"), "deps:\n  - /no/such/dep\n").unwrap();

        let dir = proj.display().to_string();
        let req = format!(r#"{{"op":"sync","from_lock":true,"dir":"{dir}"}}"#);
        let res = dispatch(&env, &req).unwrap();
        assert_eq!(res["failed"], 0);
        assert_eq!(res["deps"][0]["status"], "installed");
        assert_eq!(res["deps"][0]["dep"], pkg.display().to_string());
        // second run → current
        let res = dispatch(&env, &req).unwrap();
        assert_eq!(res["deps"][0]["status"], "current");
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
        std::env::set_var("TAMIAS_CONSENT_FILE", tmp.path().join("consented.json"));
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
        assert_eq!(results[0]["signed"], false);
        assert!(results[0]["publisher"].is_null());
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

    /// A fake `tamias-driver-*` fixture: private `PATH` (fixture dir +
    /// system dirs only, so real drivers can't leak into discovery)
    /// and a private `TAMIAS_HOME`. Returns `(env, bindir)` — call
    /// under [`EnvGuard`] after env mutations.
    #[cfg(unix)]
    fn driver_fixture(tmp: &Path) -> (Env, PathBuf) {
        let bindir = tmp.join("bin");
        let th = tmp.join("th");
        std::fs::create_dir_all(&bindir).unwrap();
        std::fs::create_dir_all(th.join("drivers")).unwrap();
        std::env::set_var("TAMIAS_HOME", &th);
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bindir.display()));
        let env = Env {
            store_root: tmp.join("store"),
            cache_root: tmp.join("cache"),
            registry_root: tmp.join("registry"),
            config_file: None,
        };
        (env, bindir)
    }

    /// Write an executable `tamias-driver-<name>` script into `dir`.
    #[cfg(unix)]
    fn write_driver(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(format!("tamias-driver-{name}"));
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The fake driver's script: `capabilities` answers a manifest,
    /// `serve` captures the request line into `$FAKE_CAPTURE` and
    /// answers `ok:false` for queries containing `boom`, `emit-json`
    /// prints a JSON doc for the `driver.exec` `json` field, anything
    /// else echoes its argv.
    #[cfg(unix)]
    const FAKE_DRIVER: &str = r#"#!/bin/sh
case "$1" in
  capabilities) printf '%s\n' '{"name":"fake","verbs":["serve"],"types":[]}' ;;
  serve)
    IFS= read -r line
    printf '%s\n' "$line" > "$FAKE_CAPTURE"
    case "$line" in
      *boom*) printf '%s\n' '{"ok":false,"error":"fake exploded"}' ;;
      *) printf '%s\n' '{"ok":true,"result":{"query":"cli.fake.op","data":{"pong":true}}}' ;;
    esac
    ;;
  emit-json) printf '%s\n' '{"code":0,"data":[1,2]}' ;;
  *) printf '%s\n' "args:$*" ;;
esac
"#;

    #[cfg(unix)]
    #[test]
    fn driver_ops_list_manifest_query_exec() {
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let (env, bindir) = driver_fixture(tmp.path());
        write_driver(&bindir, "fake", FAKE_DRIVER);
        // a driver whose capabilities probe fails → `error`, not a
        // failed op
        write_driver(&bindir, "broken", "#!/bin/sh\nexit 1\n");
        let capture = tmp.path().join("capture.txt");
        std::env::set_var("FAKE_CAPTURE", &capture);

        // list — fake reports its manifest, broken degrades to error
        let list = dispatch(&env, r#"{"op":"driver.list"}"#).unwrap();
        let drivers = list["drivers"].as_array().unwrap();
        assert_eq!(drivers.len(), 2);
        let fake = drivers.iter().find(|d| d["name"] == "fake").unwrap();
        assert_eq!(fake["manifest"]["name"], "fake");
        assert!(fake["path"]
            .as_str()
            .unwrap()
            .contains("tamias-driver-fake"));
        let broken = drivers.iter().find(|d| d["name"] == "broken").unwrap();
        assert!(broken["error"].is_string());
        assert!(broken.get("manifest").is_none());

        // manifest — the driver's capabilities JSON verbatim
        let m = dispatch(&env, r#"{"op":"driver.manifest","driver":"fake"}"#).unwrap();
        assert_eq!(m["name"], "fake");
        assert_eq!(m["verbs"][0], "serve");

        // query — the serve wire round-trips; the request line the
        // driver saw is the pavo `{"op":"query",…}` shape
        let q = dispatch(
            &env,
            r#"{"op":"driver.query","driver":"fake","query":"cli.fake.op","params":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(q["ok"], true);
        assert_eq!(q["result"]["data"]["pong"], true);
        let sent = std::fs::read_to_string(&capture).unwrap();
        assert!(sent.contains(r#""op":"query""#), "{sent}");
        assert!(sent.contains(r#""query":"cli.fake.op""#), "{sent}");
        assert!(sent.contains(r#""x":1"#), "{sent}");

        // query — a driver-side ok:false is the op's error
        let err = dispatch(
            &env,
            r#"{"op":"driver.query","driver":"fake","query":"boom","params":{}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("fake exploded"), "{err}");

        // exec — argv passthrough; `json` parses stdout when possible
        let ex = dispatch(
            &env,
            r#"{"op":"driver.exec","driver":"fake","argv":["emit-json"]}"#,
        )
        .unwrap();
        assert_eq!(ex["code"], 0);
        assert_eq!(ex["json"]["data"][0], 1);
        let ex = dispatch(
            &env,
            r#"{"op":"driver.exec","driver":"fake","argv":["run","forge","pr","list"]}"#,
        )
        .unwrap();
        assert!(ex["stdout"].as_str().unwrap().contains("run forge pr list"));
        assert!(ex["json"].is_null());
    }

    #[cfg(unix)]
    #[test]
    fn driver_ops_error_paths() {
        let _g = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let (env, bindir) = driver_fixture(tmp.path());
        write_driver(&bindir, "fake", FAKE_DRIVER);
        write_driver(
            &bindir,
            "slow",
            "#!/bin/sh\ncase \"$1\" in serve) sleep 30;; *) exit 0;; esac\n",
        );
        std::env::set_var("FAKE_CAPTURE", tmp.path().join("cap.txt"));

        // unknown / hostile / missing-field driver names
        assert!(dispatch(
            &env,
            r#"{"op":"driver.query","driver":"ghost","query":"x"}"#
        )
        .is_err());
        assert!(dispatch(
            &env,
            r#"{"op":"driver.query","driver":"../evil","query":"x"}"#
        )
        .is_err());
        assert!(dispatch(&env, r#"{"op":"driver.manifest","driver":"Ghost"}"#).is_err());
        assert!(dispatch(&env, r#"{"op":"driver.manifest"}"#).is_err());
        // missing/ill-typed query params
        assert!(dispatch(&env, r#"{"op":"driver.query","driver":"fake"}"#).is_err());
        assert!(dispatch(
            &env,
            r#"{"op":"driver.query","driver":"fake","query":"x","params":[]}"#
        )
        .is_err());
        // exec arg validation
        assert!(dispatch(&env, r#"{"op":"driver.exec","driver":"fake"}"#).is_err());
        assert!(dispatch(
            &env,
            r#"{"op":"driver.exec","driver":"fake","argv":["x",1]}"#
        )
        .is_err());
        // a driver that never answers hits the timeout bound
        let t = dispatch(
            &env,
            r#"{"op":"driver.query","driver":"slow","query":"x","timeout_ms":200}"#,
        )
        .unwrap_err();
        assert!(t.to_string().contains("timed out"), "{t}");
    }
}
