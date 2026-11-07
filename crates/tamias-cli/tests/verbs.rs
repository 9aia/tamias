//! CLI surface tests — guardrails, `run` exec modes, `env`
//! quoting, `--json` shapes, `check` findings, audit outcomes.
//! Temp-HOME isolated; every value is fake by rule.

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FAKE: &str = "fake-value-not-a-real-secret";

fn tamias(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tamias"));
    c.args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
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

fn json_stdout(o: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(o))
        .unwrap_or_else(|e| panic!("stdout not json: {e}\n{}", stdout(o)))
}

/// Write `body` as an tamias.toml in the temp home under `name`.
fn config_named(home: &Path, name: &str, body: &str) -> PathBuf {
    let p = home.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// Write `body` as `tamias.toml` in the temp home.
fn config(home: &Path, body: &str) -> PathBuf {
    config_named(home, "tamias.toml", body)
}

/// The `{"error": …}` line of stderr (audit events are the other
/// lines — parse the last).
fn stderr_json(o: &Output) -> serde_json::Value {
    let err = stderr(o);
    let last = err.lines().last().unwrap_or_default();
    serde_json::from_str(last).unwrap_or_else(|e| panic!("stderr last line not json: {e}\n{err}"))
}

fn with_config<'a>(cfg: &'a Path, args: &'a [&'a str]) -> Vec<String> {
    let mut v = vec!["--config".to_string(), cfg.to_string_lossy().into_owned()];
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

fn run_cfg(home: &Path, cfg: &Path, args: &[&str]) -> Output {
    let argv = with_config(cfg, args);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    run(home, &refs)
}

fn init(home: &Path) {
    let o = run(home, &["init"]);
    assert!(o.status.success(), "init: {}", stderr(&o));
}

#[test]
fn guardrails_deny_ops_advisedly() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    run_stdin(
        home,
        &["put", "tamias://billing/stripe"],
        &format!("{FAKE}\n"),
    );

    let cfg = config(
        home,
        r#"
        [guardrails]
        deny = ["get", "put"]
        "#,
    );

    // get is denied — exit non-zero, clear stderr, denied event
    let o = run_cfg(home, &cfg, &["get", "tamias://billing/stripe"]);
    assert!(!o.status.success());
    let err = stderr(&o);
    assert!(err.contains("op 'get' denied by guardrails"), "{err}");
    assert!(err.contains("\"outcome\":\"denied\""), "{err}");
    assert!(err.contains("\"op\":\"get\""), "{err}");
    // the value never reached stdout
    assert!(!stdout(&o).contains(FAKE));

    // --json error shape on a denied op
    let o = run_cfg(home, &cfg, &["get", "--json", "tamias://billing/stripe"]);
    assert!(!o.status.success());
    let e = stderr_json(&o);
    assert!(e["error"].as_str().unwrap().contains("denied"), "{e}");

    // put is denied too — before stdin is even read
    let o = run_cfg(home, &cfg, &["put", "tamias://x/y"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("denied by guardrails"));

    // run stays allowed — the secret still injects
    let o = run_cfg(
        home,
        &cfg,
        &[
            "run",
            "-s",
            "V=tamias://billing/stripe",
            "--",
            "printenv",
            "V",
        ],
    );
    assert!(o.status.success(), "run: {}", stderr(&o));
    assert_eq!(stdout(&o).trim_end(), FAKE);
}

#[test]
fn run_execs_argv_without_a_shell() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    run_stdin(
        home,
        &["put", "tamias://billing/stripe"],
        &format!("{FAKE}\n"),
    );

    // argv mode: metacharacters are literal — `$FAKE` prints as-is,
    // a `;` doesn't split commands. bash -c would expand/inject.
    let o = run(
        home,
        &[
            "run",
            "-s",
            "V=tamias://billing/stripe",
            "--",
            "printf",
            "%s",
            "$V; echo PWNED",
        ],
    );
    assert!(o.status.success(), "argv run: {}", stderr(&o));
    assert_eq!(stdout(&o), "$V; echo PWNED");

    // argv mode: missing binary is an tamias error, not a shell 127
    // (two args → exec mode; one arg would be `bash -c`)
    let o = run(home, &["run", "--", "definitely-not-a-binary-xyz", "arg"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("\"outcome\":\"error\""),
        "{}",
        stderr(&o)
    );

    // shell mode: a single string arg still goes through bash -c
    let o = run(
        home,
        &["run", "-s", "V=tamias://billing/stripe", "--", "printenv V"],
    );
    assert!(o.status.success(), "shell run: {}", stderr(&o));
    assert_eq!(stdout(&o).trim_end(), FAKE);

    // child exit code propagates (and is not an tamias error event)
    let o = run(home, &["run", "--", "false"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        !stderr(&o).contains("\"outcome\":\"error\""),
        "child failure is not an tamias failure: {}",
        stderr(&o)
    );

    // two -s refs → exactly two materialization events with op run
    run_stdin(home, &["put", "tamias://db/url"], "fake-db\n");
    let o = run(
        home,
        &[
            "run",
            "-s",
            "A=tamias://billing/stripe",
            "-s",
            "B=tamias://db/url",
            "--",
            "true",
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let run_events = stderr(&o)
        .lines()
        .filter(|l| l.contains("\"op\":\"run\""))
        .count();
    assert_eq!(run_events, 2, "{}", stderr(&o));
}

#[test]
fn env_output_is_shell_quote_safe() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // nasty value: single quote, dollar, backtick — must round-trip
    let nasty = "a'b $HOME `id`";
    run_stdin(home, &["put", "tamias://nasty/one"], &format!("{nasty}\n"));
    let o = run(home, &["env", "-s", "V=tamias://nasty/one"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let line = stdout(&o).trim_end().to_string();
    assert_eq!(line, "export V='a'\\''b $HOME `id`'");
    // and it actually evals back to the same value — pass the
    // export line as a positional arg so no quoting layer can
    // re-expand it
    let o = Command::new("bash")
        .args(["-c", "eval \"$1\"; printf %s \"$V\"", "eval", &line])
        .output()
        .unwrap();
    assert!(o.status.success(), "eval: {}", stderr(&o));
    assert_eq!(stdout(&o), nasty);
}

#[test]
fn json_shapes_are_stable() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();

    // init
    let o = run(home, &["init", "--json"]);
    let v = json_stdout(&o);
    assert_eq!(v["created"], true);
    assert!(v["recipient"].as_str().unwrap().starts_with("age1"));
    assert!(v["identity"].is_string() && v["vault"].is_string(), "{v}");

    // put → {ok, ref, env, version, created_at, rotated_at}
    let o = run_stdin(
        home,
        &["put", "--json", "tamias://billing/stripe"],
        &format!("{FAKE}\n"),
    );
    let v = json_stdout(&o);
    assert_eq!(v["ok"], true);
    assert_eq!(v["ref"], "tamias://billing/stripe");
    assert_eq!(v["env"], "dev");
    assert_eq!(v["version"], 1);
    assert!(v["value"].is_null(), "put never echoes the value: {v}");

    // get → {ref, env, value, version, …}
    let o = run(home, &["get", "--json", "tamias://billing/stripe"]);
    let v = json_stdout(&o);
    assert_eq!(v["value"], FAKE);
    assert_eq!(v["ref"], "tamias://billing/stripe");
    assert_eq!(v["env"], "dev");

    // list → array of {ref, env, version, …} — never values
    let o = run(home, &["list", "--json"]);
    let v = json_stdout(&o);
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["ref"], "tamias://billing/stripe");
    assert!(arr[0].get("value").is_none(), "list never carries values");

    // env --json → {VAR: value} — documented value-exposing surface
    let o = run(home, &["env", "--json", "-s", "V=tamias://billing/stripe"]);
    let v = json_stdout(&o);
    assert_eq!(v["V"], FAKE);

    // rotate --json
    let o = run_stdin(
        home,
        &["rotate", "--json", "tamias://billing/stripe"],
        "fake-v2\n",
    );
    let v = json_stdout(&o);
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], 2);
    assert!(v["rotated_at"].is_string(), "{v}");

    // rm --json → {ok, removed, ref, env}
    let o = run(home, &["rm", "--json", "tamias://billing/stripe"]);
    let v = json_stdout(&o);
    assert_eq!(v["ok"], true);
    assert_eq!(v["removed"], true);

    // errors on --json → {"error": …} on stderr, non-zero exit
    let o = run(home, &["get", "--json", "tamias://billing/stripe"]);
    assert!(!o.status.success());
    let e = stderr_json(&o);
    assert!(e["error"].as_str().unwrap().contains("not stored"), "{e}");

    // rm-miss is an error too
    let o = run(home, &["rm", "--json", "tamias://billing/stripe"]);
    assert!(!o.status.success());
    let e = stderr_json(&o);
    assert!(e["error"].is_string(), "{e}");
}

#[test]
fn check_reports_findings_and_exit_codes() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();

    // before init: the local vault isn't readable → findings
    let cfg = config(
        home,
        "[secrets]\nSTRIPE_KEY = \"tamias://billing/stripe\"\n",
    );
    let o = run_cfg(home, &cfg, &["check", "--json"]);
    assert!(!o.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(!v["errors"].as_array().unwrap().is_empty(), "{v}");
    let vaults = v["vaults"].as_array().unwrap();
    assert!(vaults.iter().any(|x| x["ok"] == false), "{v}");

    // after init: vault readable but declared secret unresolved
    init(home);
    let o = run_cfg(home, &cfg, &["check", "--json"]);
    assert!(!o.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["secrets"]["STRIPE_KEY"]["resolves"], false);
    assert!(
        v["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("STRIPE_KEY")),
        "{v}"
    );

    // store it → check is green
    run_stdin(
        home,
        &["put", "tamias://billing/stripe"],
        &format!("{FAKE}\n"),
    );
    let o = run_cfg(home, &cfg, &["check"]);
    assert!(o.status.success(), "check: {}", stderr(&o));

    // a route to an undeclared vault is a finding
    let bad = config_named(
        home,
        "tamias-bad.toml",
        "[envs]\nprod = { vault = \"nowhere\" }\n",
    );
    let o = run_cfg(home, &bad, &["check"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("unconfigured vault"), "{}", stderr(&o));
}

#[test]
fn env_pin_and_declared_surface() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);

    // declared surface in config: STRIPE_KEY injects without -s
    let cfg = config(
        home,
        "[secrets]\nSTRIPE_KEY = \"tamias://billing/stripe\"\n",
    );
    run_stdin(
        home,
        &["put", "tamias://billing/stripe"],
        &format!("{FAKE}\n"),
    );
    let o = run_cfg(home, &cfg, &["run", "--", "printenv", "STRIPE_KEY"]);
    assert!(o.status.success(), "declared run: {}", stderr(&o));
    assert_eq!(stdout(&o).trim_end(), FAKE);

    // a declared ref that isn't stored fails the whole run — no
    // partial injection
    let cfg2 = config_named(
        home,
        "tamias-missing.toml",
        "[secrets]\nSTRIPE_KEY = \"tamias://billing/stripe\"\nMISSING = \"tamias://not/here\"\n",
    );
    let o = run_cfg(home, &cfg2, &["run", "--", "printenv", "STRIPE_KEY"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("MISSING"), "{}", stderr(&o));

    // env pin: tamias://prod/token resolves under prod regardless
    run_stdin(home, &["put", "tamias://prod/token"], "fake-prod\n");
    let o = run_cfg(
        home,
        &cfg,
        &["run", "-s", "P=tamias://prod/token", "--", "printenv", "P"],
    );
    assert!(o.status.success(), "pin run: {}", stderr(&o));
    assert_eq!(stdout(&o).trim_end(), "fake-prod");
    // audit records the pinned env
    assert!(stderr(&o).contains("\"env\":\"prod\""), "{}", stderr(&o));
}
