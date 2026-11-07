//! Cloud surface tests — `kind = "cf"` vaults and `tamias deploy`
//! end-to-end through the binary, against in-process mock servers
//! (the wire mock = the Worker's semantics; the CF mock = the
//! Secrets Store envelope). Values and tokens are fake by rule.

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tamias_testkit::mocks;

const FAKE: &str = "fake-value-not-a-real-secret";
const TOKEN: &str = "fake-token-not-real";
const TOKEN_ENV: &str = "TAMIAS_TEST_CF_TOKEN";

fn tamias(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tamias"));
    c.args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env(TOKEN_ENV, TOKEN)
        .env_remove("TAMIAS_CONFIG")
        .env_remove("TAMIAS_ENV")
        .env_remove("TAMIAS_ACTOR")
        .env(
            "TAMIAS_IDENTITY_FILE",
            home.join("xdg-config/tamias/identity.txt"),
        );
    c
}

fn run(home: &Path, args: &[&str]) -> Output {
    tamias(home, args).output().unwrap()
}

fn run_stdin(home: &Path, args: &[&str], input: &str) -> Output {
    let mut child = tamias(home, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn config(home: &Path, body: &str) -> PathBuf {
    let p = home.join("tamias.toml");
    std::fs::write(&p, body).unwrap();
    p
}

fn run_cfg(home: &Path, cfg: &Path, args: &[&str]) -> Output {
    let mut v = vec!["--config".to_string(), cfg.to_string_lossy().into_owned()];
    v.extend(args.iter().map(|s| s.to_string()));
    let refs: Vec<&str> = v.iter().map(String::as_str).collect();
    run(home, &refs)
}

fn init(home: &Path) {
    let o = run(home, &["init"]);
    assert!(o.status.success(), "init: {}", stderr(&o));
}

#[test]
fn cf_vault_roundtrips_through_the_wire() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    let mock = mocks::vault_wire(TOKEN);
    let cfg = config(
        home,
        &format!(
            r#"
            [vaults.cloud]
            kind = "cf"
            url = "{}"
            token_env = "{TOKEN_ENV}"

            [envs]
            prod = {{ vault = "cloud" }}
            "#,
            mock.url()
        ),
    );

    // put/get/list against the prod env → HTTP to the mock worker
    let o = run_stdin(
        home,
        &[
            "--config",
            cfg.to_str().unwrap(),
            "put",
            "tamias://prod/billing/stripe",
        ],
        &format!("{FAKE}\n"),
    );
    assert!(o.status.success(), "put: {}", stderr(&o));
    let o = run_cfg(home, &cfg, &["get", "tamias://prod/billing/stripe"]);
    assert_eq!(stdout(&o).trim_end(), FAKE);
    let o = run_cfg(home, &cfg, &["list", "--env", "prod"]);
    assert_eq!(stdout(&o).trim(), "tamias://billing/stripe");

    // env isolation holds across the wire too
    let o = run_cfg(home, &cfg, &["get", "tamias://billing/stripe"]);
    assert!(!o.status.success(), "dev keyspace must be separate");
}

#[test]
fn migrate_rekeys_rows_and_reports() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    let mock = mocks::vault_wire(TOKEN);
    // seed one plaintext-named row through a v1-style config
    let plain_cfg = config(
        home,
        &format!(
            r#"
            [vaults.cloud]
            kind = "cf"
            url = "{}"
            token_env = "{TOKEN_ENV}"

            [envs]
            dev = {{ vault = "cloud" }}
            "#,
            mock.url()
        ),
    );
    let o = run_stdin(
        home,
        &[
            "--config",
            plain_cfg.to_str().unwrap(),
            "put",
            "tamias://a/b",
        ],
        &format!("{FAKE}\n"),
    );
    assert!(o.status.success(), "put: {}", stderr(&o));

    let cfg = config(
        home,
        &format!(
            r#"
            [vaults.cloud]
            kind = "cf"
            url = "{}"
            token_env = "{TOKEN_ENV}"
            private_refs = true

            [envs]
            dev = {{ vault = "cloud" }}
            "#,
            mock.url()
        ),
    );

    // no transform flag → a pointer, not a silent no-op
    let o = run_cfg(home, &cfg, &["migrate"]);
    assert!(!o.status.success(), "{}", stderr(&o));
    assert!(stderr(&o).contains("--private-refs"), "{}", stderr(&o));

    // the rewrite: human output reports the re-keyed row
    let o = run_cfg(home, &cfg, &["migrate", "--private-refs"]);
    assert!(o.status.success(), "migrate: {}", stderr(&o));
    assert!(stderr(&o).contains("rewrote 1 ref"), "{}", stderr(&o));

    // second run is a no-op — the --json shape reports it
    let o = run_cfg(home, &cfg, &["migrate", "--private-refs", "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["migrated"][0]["vault"], "cloud");
    assert_eq!(v["migrated"][0]["already_private"], 1);

    // and the migrated row still reads end-to-end
    let o = run_cfg(home, &cfg, &["get", "tamias://a/b"]);
    assert_eq!(stdout(&o).trim_end(), FAKE);
}

#[test]
fn deploy_pushes_the_value_to_the_sink() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    run_stdin(
        home,
        &["put", "tamias://billing/stripe"],
        &format!("{FAKE}\n"),
    );

    let (mock, seen) = mocks::cf_secrets_store("acc", "sto", TOKEN);
    let cfg = config(
        home,
        &format!(
            r#"
            [deploy.prod]
            kind = "cf-secrets-store"
            account_id = "acc"
            store_id = "sto"
            token_env = "{TOKEN_ENV}"
            base_url = "{}"
            "#,
            mock.url()
        ),
    );

    // one target configured → no --target needed; --json shape
    let o = run_cfg(home, &cfg, &["deploy", "--json", "tamias://billing/stripe"]);
    assert!(o.status.success(), "deploy: {}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["target"], "prod");
    assert_eq!(v["secret_name"], "dev_billing_stripe");
    assert_eq!(v["created"], true);

    // the sink received the plaintext value (it IS a plaintext
    // store — the ciphertext boundary ends at tamias's vaults)
    let post = seen
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.method == "POST")
        .unwrap()
        .text();
    assert!(post.contains(FAKE), "sink body: {post}");

    // audit: a per-ref "deploy" event on stderr
    assert!(stderr(&o).contains("\"op\":\"deploy\""), "{}", stderr(&o));

    // explicit --target works too; second push is an update
    let o = run_cfg(
        home,
        &cfg,
        &["deploy", "tamias://billing/stripe", "--target", "prod"],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(seen.lock().unwrap().iter().any(|r| r.method == "PATCH"));

    // a ref that isn't stored fails clean with an error event
    let o = run_cfg(home, &cfg, &["deploy", "tamias://not/here"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("not stored"), "{}", stderr(&o));

    // guardrails can advisory-deny deploy like any op
    let deny = config(
        home,
        &format!(
            "[deploy.prod]\naccount_id=\"acc\"\nstore_id=\"sto\"\nbase_url=\"{}\"\n\n[guardrails]\ndeny = [\"deploy\"]\n",
            mock.url()
        ),
    );
    let o = run_cfg(home, &deny, &["deploy", "tamias://billing/stripe"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("denied by guardrails"),
        "{}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("\"op\":\"deploy\""), "{}", stderr(&o));
}

#[test]
fn check_reports_unreachable_cf_vault_not_panic() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let cfg = config(
        home,
        &format!(
            r#"
            [vaults.cloud]
            kind = "cf"
            url = "http://127.0.0.1:1"
            token_env = "{TOKEN_ENV}"

            [envs]
            prod = {{ vault = "cloud" }}
            "#,
        ),
    );
    let o = run_cfg(home, &cfg, &["check", "--json"]);
    assert!(!o.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let vaults = v["vaults"].as_array().unwrap();
    let cloud = vaults.iter().find(|x| x["name"] == "cloud").unwrap();
    assert_eq!(cloud["kind"], "cf");
    assert_eq!(cloud["ok"], false, "{v}");
    assert!(cloud["error"].is_string(), "{v}");
}

#[test]
fn cf_resolution_without_an_explicit_file_notes_the_missing_keyring() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    let mock = mocks::vault_wire(TOKEN);
    let cfg = config(
        home,
        &format!(
            r#"
            [vaults.cloud]
            kind = "cf"
            url = "{}"
            token_env = "{TOKEN_ENV}"

            [envs]
            prod = {{ vault = "cloud" }}
            "#,
            mock.url()
        ),
    );
    // no TAMIAS_IDENTITY_FILE — the cf vault's identity resolution
    // probes the session keyring, fails headless, then falls back
    // to the file: a note, not a panic.
    let mut cmd = tamias(home, &["--config", cfg.to_str().unwrap(), "list"]);
    cmd.env_remove("TAMIAS_IDENTITY_FILE");
    let o = cmd.output().unwrap();
    assert!(
        stderr(&o).contains("no session keyring reachable"),
        "{}",
        stderr(&o)
    );
}
