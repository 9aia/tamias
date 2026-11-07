//! Edge cases — non-json output arms, error exits, tty-only code
//! paths (via `script`'s pty), and failure modes a unit test can't
//! reach. Temp-HOME isolated; values are fake by rule.

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

fn run_stdin(home: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = tamias(home, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// Run under a pty via `script` — stdin goes in as terminal input.
fn run_pty(home: &Path, args: &[&str], input: &[u8]) -> Output {
    run_pty_prelude(home, args, input, "")
}

fn run_pty_prelude(home: &Path, args: &[&str], input: &[u8], prelude: &str) -> Output {
    let tamias = env!("CARGO_BIN_EXE_tamias");
    // `env -i` scrubs everything — pass LLVM_PROFILE_FILE through so
    // a coverage build still lands the child's .profraw in llvm-cov's
    // target dir instead of a stray default_*.profraw next to the
    // test binary.
    let prof = std::env::var("LLVM_PROFILE_FILE")
        .map(|v| format!(" LLVM_PROFILE_FILE='{v}'"))
        .unwrap_or_default();
    let cmdline = format!(
        "{prelude}exec env -i HOME='{}' XDG_CONFIG_HOME='{}' XDG_DATA_HOME='{}' TAMIAS_IDENTITY_FILE='{}' TERM=xterm{prof} '{}' {}",
        home.display(),
        home.join("xdg-config").display(),
        home.join("xdg-data").display(),
        home.join("xdg-config/tamias/identity.txt").display(),
        tamias,
        args.join(" ")
    );
    let mut child = Command::new("script")
        .args(["-qec", &cmdline, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if !prelude.is_empty() {
        // give the prelude's poke time to land before the keys
        std::thread::sleep(std::time::Duration::from_millis(1800));
    }
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// Same, but fires SIGWINCH at the child once the TUI is up —
/// crossterm turns it into an `Event::Resize`, which exercises the
/// event loop's non-Key arm, and the delayed input also leaves a
/// few idle `poll` ticks on the clock.
fn run_pty_winch(home: &Path, args: &[&str], input: &[u8]) -> Output {
    // `$$` inside the parens is the `sh -c` pid — the same pid the
    // `exec` hands to tamias. Two pokes for startup-timing slack.
    let prelude = "(sleep 1; kill -WINCH $$; sleep 0.4; kill -WINCH $$) & ";
    run_pty_prelude(home, args, input, prelude)
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn config(home: &Path, name: &str, body: &str) -> PathBuf {
    let p = home.join(name);
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
    assert!(run(home, &["init"]).status.success());
}

#[test]
fn introspection_verbs_run_before_config_load() {
    let home = tempfile::tempdir().unwrap();
    // none of these need a config — they work in a bare HOME
    let o = run(home.path(), &["manifest"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("\"tamias\""));
    let o = run(home.path(), &["schema"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("vaults"), "{}", stdout(&o));
    let o = run(home.path(), &["gen-cli-docs"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("tamias"), "{}", stdout(&o));
}

#[test]
fn load_failures_audit_and_fail_clean() {
    let home = tempfile::tempdir().unwrap();
    // explicit --config missing → error + error audit event
    let o = run(
        home.path(),
        &["--config", "/nonexistent/tamias.toml", "list"],
    );
    assert!(!o.status.success());
    let e = stderr(&o);
    assert!(e.contains("\"outcome\":\"error\""), "{e}");
    assert!(e.contains("\"op\":\"list\""), "{e}");
    // same but --json → {"error":…} on stderr
    let o = run(
        home.path(),
        &["--config", "/nonexistent/tamias.toml", "--json", "list"],
    );
    assert!(!o.status.success());
    // a config that exists but won't parse → same path
    let bad = config(home.path(), "bad.toml", "[[[ not toml");
    let o = run(home.path(), &["--config", bad.to_str().unwrap(), "list"]);
    assert!(!o.status.success());
    // and with a ref arg — the fallback audit still records it raw
    let o = run(
        home.path(),
        &["--config", "/nonexistent.toml", "get", "not-a-ref"],
    );
    assert!(!o.status.success());
    assert!(stderr(&o).contains("not-a-ref"), "{}", stderr(&o));
}

#[test]
fn bad_ref_args_fail_before_touching_the_vault() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    for args in [
        vec!["get", "not-a-ref"],
        vec!["rm", "not-a-ref"],
        vec!["deploy", "not-a-ref"],
    ] {
        let o = run(home, &args);
        assert!(!o.status.success(), "{args:?}");
        let e = stderr(&o);
        // the error audit event carries the raw unparseable ref
        assert!(e.contains("\"outcome\":\"error\""), "{args:?}: {e}");
        assert!(e.contains("not-a-ref"), "{args:?}: {e}");
    }
    // put/rotate are denied-free too — bad ref fails at parse
    let o = run_stdin(home, &["put", "not-a-ref"], b"fake\n");
    assert!(!o.status.success());
    let o = run_stdin(home, &["rotate", "not-a-ref"], b"fake\n");
    assert!(!o.status.success());
    // a denied op on an unparseable ref → denied event with raw ref
    let cfg = config(home, "tamias.toml", "[guardrails]\ndeny = [\"get\"]\n");
    let o = run_cfg(home, &cfg, &["get", "not-a-ref"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("\"outcome\":\"denied\""));
    assert!(stderr(&o).contains("not-a-ref"));
}

#[test]
fn non_json_verbs_print_for_humans() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // second init is not "generated" — the quiet else arm
    let o = run(home, &["init"]);
    assert!(o.status.success());
    assert!(!stderr(&o).contains("(generated)"), "{}", stderr(&o));

    run_stdin(home, &["put", "tamias://billing/stripe"], FAKE.as_bytes());
    let o = run(home, &["list"]);
    assert_eq!(stdout(&o).trim(), "tamias://billing/stripe");
    // rm non-json
    let o = run(home, &["rm", "tamias://billing/stripe"]);
    assert!(o.status.success());
    assert!(stderr(&o).contains("removed"), "{}", stderr(&o));
    // rm-miss non-json is an error
    let o = run(home, &["rm", "tamias://billing/stripe"]);
    assert!(!o.status.success());
    // put + rotate non-json print "(vN)"
    run_stdin(home, &["put", "tamias://a/b"], b"fake-a\n");
    let o = run_stdin(home, &["rotate", "tamias://a/b"], b"fake-a2\n");
    assert!(o.status.success());
    assert!(stderr(&o).contains("(v2)"), "{}", stderr(&o));
    // list --json shows the rotated_at stamp
    let o = run(home, &["list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(v[0]["rotated_at"].is_string(), "{v}");
    // rotate of a missing ref → NotFound, clean error
    let o = run_stdin(home, &["rotate", "tamias://no/here"], b"fake\n");
    assert!(!o.status.success());
}

#[test]
fn verbs_fail_clean_when_the_vault_cant_open() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    // dev routes to a vault of an unknown kind → open errors
    let cfg = config(
        home,
        "tamias.toml",
        "[vaults.bogus]\nkind = \"zzz\"\n\n[envs]\ndev = \"bogus\"\n",
    );
    for args in [
        vec!["list"],
        vec!["list", "--json"],
        vec!["get", "tamias://x/y"],
        vec!["rm", "tamias://x/y"],
        vec!["init"],
    ] {
        let o = run_cfg(home, &cfg, &args);
        assert!(!o.status.success(), "{args:?}");
    }
    let o = run_cfg(home, &cfg, &["deploy", "tamias://x/y", "--target", "t"]);
    assert!(!o.status.success());
}

#[test]
fn read_secret_rejects_non_utf8_and_empty_stdin() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // invalid utf-8 on the pipe → a read error, not a panic
    let o = run_stdin(home, &["put", "tamias://x/y"], b"\xff\xfe");
    assert!(!o.status.success());
    // empty stdin → "empty secret"
    let o = run_stdin(home, &["put", "tamias://x/y"], b"");
    assert!(!o.status.success());
    let o = run_stdin(home, &["put", "tamias://x/y"], b"\n");
    assert!(!o.status.success());
}

#[test]
fn run_and_env_validate_pairs_and_resolutions() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // -s without '=' → the pair parse error
    let o = run(home, &["run", "-s", "BADPAIR", "--", "true"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("VAR=tamias://ref"), "{}", stderr(&o));
    // -s with an unparseable ref → resolve error
    let o = run(home, &["run", "-s", "V=not-a-ref", "--", "true"]);
    assert!(!o.status.success());
    // a declared-but-unstored ref → the whole run refuses
    let o = run(home, &["run", "-s", "V=tamias://not/stored", "--", "true"]);
    assert!(!o.status.success());
    // same for `env`
    let o = run(home, &["env", "-s", "BADPAIR"]);
    assert!(!o.status.success());
    let o = run(home, &["env", "-s", "V=tamias://not/stored"]);
    assert!(!o.status.success());
}

#[test]
fn run_without_bash_on_path_is_a_clean_error() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // bash -c mode with no bash on PATH → spawn fails, tamias errors
    let o = tamias(home, &["run", "--", "true"])
        .env("PATH", "/nonexistent-bin")
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("\"outcome\":\"error\""),
        "{}",
        stderr(&o)
    );
}

#[test]
fn env_writes_fail_cleanly_on_a_full_device() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    run_stdin(home, &["put", "tamias://x/y"], FAKE.as_bytes());
    if !Path::new("/dev/full").exists() {
        return; // no /dev/full on this platform — nothing to test
    }
    let full = std::fs::File::options()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let o = tamias(home, &["env", "-s", "V=tamias://x/y"])
        .stdout(Stdio::from(full))
        .output()
        .unwrap();
    assert!(!o.status.success(), "writes to /dev/full must fail");
}

#[test]
fn a_deleted_cwd_is_a_clean_error_not_a_panic() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // real ciphertext in a normal-path vault first
    let vault_file = home.join("real.age");
    let id_file = home.join("xdg-config/tamias/identity.txt");
    let cfg_a = config(
        home,
        "a.toml",
        &format!(
            "[vaults.local]\npath = {:?}\nidentity = {:?}\n",
            vault_file, id_file
        ),
    );
    let o = run_stdin(
        home,
        &["--config", cfg_a.to_str().unwrap(), "put", "tamias://x/y"],
        FAKE.as_bytes(),
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let ciphertext = std::fs::read(&vault_file).unwrap();
    // now the same identity but the vault path is a fifo — the
    // child's vault read blocks until our writer feeds it
    let fifo = home.join("vault.fifo");
    assert!(Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let cfg_b = config(
        home,
        "b.toml",
        &format!(
            "[vaults.local]\npath = {:?}\nidentity = {:?}\n",
            fifo, id_file
        ),
    );
    let writer = std::thread::spawn(move || {
        std::fs::write(&fifo, &ciphertext).unwrap();
    });
    let wd = home.join("workdir");
    std::fs::create_dir(&wd).unwrap();
    let child = tamias(
        home,
        &[
            "--config",
            cfg_b.to_str().unwrap(),
            "run",
            "-s",
            "V=tamias://x/y",
            "--",
            "true",
        ],
    )
    .current_dir(&wd)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    // the child is blocked reading the fifo — delete its cwd now,
    // so `current_dir` fails when the fifo finally feeds it
    std::fs::remove_dir_all(&wd).unwrap();
    let o = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(!o.status.success(), "deleted cwd must error, got ok");
}

#[test]
fn check_prints_findings_for_humans() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let cfg = config(
        home,
        "tamias.toml",
        "[secrets]\nMISSING = \"tamias://not/here\"\nBAD-NAME = \"tamias://x/y\"\n\n\
         [envs]\nprod = \"nowhere\"\n\n\
         [guardrails]\ndefault = \"deny\"\nallow = [\"run\", \"check\"]\ndeny = [\"get\"]\n",
    );
    let o = run_cfg(home, &cfg, &["check"]);
    assert!(!o.status.success());
    let e = stderr(&o);
    assert!(e.contains("config:"), "{e}");
    assert!(e.contains("not readable"), "{e}");
    assert!(e.contains("NOT STORED"), "{e}");
    assert!(e.contains("guardrails:"), "{e}");
    assert!(e.contains("problem:"), "{e}");

    // --json carries the guardrails table — both default arms
    let o = run_cfg(home, &cfg, &["check", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["guardrails"]["default"], "deny");
    let cfg2 = config(
        home,
        "b.toml",
        "[guardrails]\ndefault = \"allow\"\nallow = [\"run\"]\n",
    );
    let o = run_cfg(home, &cfg2, &["check", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["guardrails"]["default"], "allow");
    assert_eq!(v["guardrails"]["allow"][0], "run");
}

#[test]
fn deploy_prints_created_then_updated() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    run_stdin(home, &["put", "tamias://x/y"], FAKE.as_bytes());
    let (mock, _seen) = tamias_testkit::mocks::cf_secrets_store("a", "s", "t");
    let cfg = config(
        home,
        "tamias.toml",
        &format!(
            "[deploy.prod]\naccount_id = \"a\"\nstore_id = \"s\"\nbase_url = \"{}\"\n\n\
             [envs]\ndev = \"local\"\n",
            mock.url()
        ),
    );
    temp_env::with_var("CLOUDFLARE_API_TOKEN", Some("t"), || {
        let o = run_cfg(home, &cfg, &["deploy", "tamias://x/y"]);
        assert!(o.status.success(), "{}", stderr(&o));
        assert!(stderr(&o).contains("created"), "{}", stderr(&o));
        let o = run_cfg(home, &cfg, &["deploy", "tamias://x/y"]);
        assert!(o.status.success());
        assert!(stderr(&o).contains("updated"), "{}", stderr(&o));
    });
}

#[test]
fn ui_declines_a_non_tty_and_runs_on_a_pty() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    run_stdin(home, &["put", "tamias://x/y"], FAKE.as_bytes());
    // piped stdout → the TUI declines up front
    let o = run(home, &["ui"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("terminal"), "{}", stderr(&o));
    // a pty → the browser opens and 'q' quits clean (pty merges
    // stdout+stderr — the audit event lands in script's stdout).
    // 'x' first so a key event *isn't* consumed by the quit arm —
    // covers the loop's no-op-key iteration before 'q' breaks it;
    // the winch adds a Resize event for the non-Key arm.
    let o = run_pty_winch(home, &["ui"], b"xq");
    assert!(o.status.success(), "{}", stderr(&o));
    let all = format!("{}{}", stdout(&o), stderr(&o));
    assert!(all.contains("\"op\":\"ui\""), "{all}");
}

#[test]
fn audit_fallback_covers_every_actor_and_ref_arm() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    // no --actor, TAMIAS_ACTOR, or USER → the "anon" fallback; a
    // parseable ref → the normalized-path arm of the audit record
    let o = tamias(
        home,
        &["--config", "/nonexistent.toml", "get", "tamias://x/y"],
    )
    .env_remove("USER")
    .env_remove("LOGNAME")
    .output()
    .unwrap();
    assert!(!o.status.success());
    let e = stderr(&o);
    assert!(e.contains("\"actor\":\"anon\""), "{e}");
    assert!(e.contains("x/y"), "{e}");
}

#[test]
fn put_fails_clean_when_the_vault_cant_save() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    let id_file = home.join("xdg-config/tamias/identity.txt");
    let dir = home.join("ro");
    std::fs::create_dir(&dir).unwrap();
    let cfg = config(
        home,
        "ro.toml",
        &format!(
            "[vaults.local]\npath = {:?}\nidentity = {:?}\n",
            dir.join("vault.age"),
            id_file
        ),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    }
    // stdin is fine — the failure is in the vault write past `?`
    let o = run_stdin(
        home,
        &["--config", cfg.to_str().unwrap(), "put", "tamias://x/y"],
        b"fake\n",
    );
    assert!(!o.status.success());
}

#[test]
fn list_json_fails_when_a_record_read_dies() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    let id_file = home.join("xdg-config/tamias/identity.txt");
    // list answers with a ref, the per-ref get 500s — the failure
    // lands on `list_records`' `?`, not `list`'s
    let mock = tamias_testkit::httpmock::HttpMock::new(|req| {
        if req.path == "/v1/dev" {
            tamias_testkit::httpmock::Response::json(200, r#"{"refs":["x/y"]}"#)
        } else {
            tamias_testkit::httpmock::Response::json(500, r#"{"error":"boom"}"#)
        }
    });
    let cfg = config(
        home,
        "cf.toml",
        &format!(
            "[vaults.cloud]\nkind = \"cf\"\nurl = \"{}\"\ntoken_env = \"T\"\nidentity = {:?}\n\n\
             [envs]\ndev = \"cloud\"\n",
            mock.url(),
            id_file
        ),
    );
    let o = tamias(home, &["--config", cfg.to_str().unwrap(), "list", "--json"])
        .env("T", "fake")
        .output()
        .unwrap();
    assert!(!o.status.success(), "{}", stdout(&o));
}

#[test]
fn env_and_run_reject_invalid_var_names() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    // -s with a non-env-var name → the ad-hoc pair check fires
    let o = run(home, &["env", "-s", "BAD VAR=tamias://x/y"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("invalid env var name"),
        "{}",
        stderr(&o)
    );
    // a [secrets] key that isn't a valid env var name → the declared
    // surface check fires inside resolve_for_injection
    let cfg = config(
        home,
        "tamias.toml",
        "[secrets]\n\"BAD KEY\" = \"tamias://x/y\"\n",
    );
    let o = run_cfg(home, &cfg, &["env"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("not a valid env var name"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn deploy_requires_an_unambiguous_target() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    // no [deploy.*] tables at all → "no targets configured"
    let cfg = config(home, "tamias.toml", "[envs]\ndev = \"local\"\n");
    let o = run_cfg(home, &cfg, &["deploy", "tamias://x/y"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("no [deploy.*] targets"),
        "{}",
        stderr(&o)
    );
    // two targets and no --target → "multiple deploy targets"
    let cfg = config(
        home,
        "tamias.toml",
        "[deploy.a]\nkind = \"cf-secrets-store\"\naccount_id = \"a\"\nstore_id = \"s\"\n\n\
         [deploy.b]\nkind = \"cf-secrets-store\"\naccount_id = \"a\"\nstore_id = \"s\"\n\n\
         [envs]\ndev = \"local\"\n",
    );
    let o = run_cfg(home, &cfg, &["deploy", "tamias://x/y"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("multiple deploy targets"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn rotate_fails_clean_on_empty_stdin() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // valid ref, unreadable secret → read_secret's `?`, not NotStored
    let o = run_stdin(home, &["rotate", "tamias://x/y"], b"");
    assert!(!o.status.success());
    let o = run_stdin(home, &["rotate", "tamias://x/y"], b"\xff\xfe");
    assert!(!o.status.success());
}

#[test]
fn put_fails_clean_when_the_tty_prompt_cant_read() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // a pty fed only VEOF (Ctrl-D) — the /dev/tty prompt read hits
    // EOF and fails outright (raw EOF on script's stdin doesn't
    // reach the pty; the character does)
    let o = run_pty(home, &["put", "tamias://x/y"], b"\x04");
    assert!(!o.status.success(), "{}", stderr(&o));
}

#[test]
fn tty_paths_get_a_prompt_and_a_note() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    init(home);
    // put on a pty → the hidden /dev/tty prompt path
    let o = run_pty(home, &["put", "tamias://x/y"], b"fake-pty-value\n");
    assert!(o.status.success(), "{}", stderr(&o));
    // get on a pty → the "printed to terminal" note
    let o = run_pty(home, &["get", "tamias://x/y"], b"");
    assert!(o.status.success(), "{}", stderr(&o));
    let all = format!("{}{}", stdout(&o), stderr(&o));
    assert!(all.contains("fake-pty-value"), "{all}");
    assert!(all.contains("note: secret value"), "{all}");
}
