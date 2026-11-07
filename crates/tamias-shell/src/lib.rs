//! tamias-shell — run a command in a working directory with extra env
//! vars injected. Two modes back `tamias run`: a single string arg
//! goes through `bash -c` (shell syntax, quoting, pipes); multiple
//! argv are exec'd directly — `argv[0]` is the program, no shell
//! involved. Resolved secrets arrive as `(name, exposed-value)`
//! pairs and land only in the child's environment.
//!
//! **Children do not inherit the parent's environment** (HIGH-2):
//! a run starts from an empty env and receives only the small
//! allowlist below — so `TAMIAS_CF_TOKEN`, `CLOUDFLARE_API_TOKEN`,
//! `TAMIAS_IDENTITY_FILE`, `TAMIAS_CONFIG`, and any other parent
//! credential can never leak into a child that shouldn't hold it.

use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus};

/// Env vars a child may keep — session identity, locale, terminal,
/// and temp-dir basics a CLI tool needs. Deliberately small.
const PASS_ENV: &[&str] = &[
    // unix session
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "COLORTERM",
    "TMPDIR",
    "TEMP",
    "TMP",
    "TZ",
    "LANG",
    "PWD",
    "EDITOR",
    "VISUAL",
    "PAGER",
    // agent/display plumbing a tool may genuinely need
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    // proxies are configuration children typically need for
    // network access — they ride along deliberately
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    // windows session equivalents
    "USERNAME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "COMSPEC",
    "SYSTEMROOT",
    "PATHEXT",
    "PROGRAMDATA",
    "OS",
];

/// Env-var prefixes a child may keep (`LC_*`, `XDG_*`).
const PASS_ENV_PREFIX: &[&str] = &["LC_", "XDG_"];

/// Whether `name` survives the env scrub. Public so sibling
/// crates spawning child processes (`tamias-serve`'s PDP exec)
/// apply the *same* allowlist — one scrub policy, not two.
pub fn allowed_var(name: &str) -> bool {
    PASS_ENV.contains(&name) || PASS_ENV_PREFIX.iter().any(|p| name.starts_with(p))
}

/// Give `c` a scrubbed environment: the allowlist values copied
/// out of the parent's env, then `vars` on top. Every child tamias
/// spawns goes through this — `tamias run` (injected secrets in
/// `vars`) and `tamias serve`'s PDP exec (empty `vars`) alike — so
/// parent credentials can never leak into a subprocess.
pub fn scrub_env(c: &mut Command, vars: &[(String, String)]) {
    c.env_clear();
    for (k, v) in std::env::vars() {
        if allowed_var(&k) {
            c.env(k, v);
        }
    }
    for (k, v) in vars {
        c.env(k, v);
    }
}

/// Run `bash -c <cmd>` with `workdir` as cwd; returns the exit
/// status. Stdio is inherited — callers that need captured output
/// should use [`output`].
pub fn run(workdir: &Path, cmd: &str) -> io::Result<ExitStatus> {
    run_with_env(workdir, cmd, &[])
}

/// Like [`run`] but injects `vars` into the child environment.
/// Values arrive already exposed — this crate never sees a `Secret`.
pub fn run_with_env(
    workdir: &Path,
    cmd: &str,
    vars: &[(String, String)],
) -> io::Result<ExitStatus> {
    let mut c = Command::new("bash");
    c.current_dir(workdir).args(["-c", cmd]);
    scrub_env(&mut c, vars);
    c.status()
}

/// Exec `argv` directly — `argv[0]` is the program, no shell, no
/// quoting surprises. The `tamias run -- prog arg1 arg2` path.
/// Values arrive already exposed — this crate never sees a
/// `Secret`.
pub fn exec_with_env(
    workdir: &Path,
    argv: &[String],
    vars: &[(String, String)],
) -> io::Result<ExitStatus> {
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "exec: empty argv"))?;
    let mut c = Command::new(prog);
    c.current_dir(workdir).args(args);
    scrub_env(&mut c, vars);
    c.status()
}

/// Like [`run`] but captures stdout/stderr.
pub fn output(workdir: &Path, cmd: &str) -> io::Result<std::process::Output> {
    output_with_env(workdir, cmd, &[])
}

/// Like [`run_with_env`] but captures stdout/stderr.
pub fn output_with_env(
    workdir: &Path,
    cmd: &str,
    vars: &[(String, String)],
) -> io::Result<std::process::Output> {
    let mut c = Command::new("bash");
    c.current_dir(workdir).args(["-c", cmd]);
    scrub_env(&mut c, vars);
    c.output()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tamias-shell-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap_or_default();
        d
    }

    #[test]
    fn runs_in_worktree() {
        let d = tmpdir("run");
        let st = run(&d, "exit 0").unwrap();
        assert!(st.success());
        let st = run(&d, "exit 3").unwrap();
        assert_eq!(st.code(), Some(3));
        // a workdir that doesn't exist fails the spawn, no panic
        assert!(run(Path::new("/nonexistent-dir-xyz"), "exit 0").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn captures_output() {
        let d = tmpdir("out");
        let out = output(&d, "echo hi; pwd").unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("hi"));
        assert!(text.contains(d.to_string_lossy().as_ref()));
        assert!(output(Path::new("/nonexistent-dir-xyz"), "echo hi").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn injects_env_vars() {
        let d = tmpdir("env");
        let vars = [("TAMIAS_TEST_VAR".to_string(), "fake-injected".to_string())];
        let out = output_with_env(&d, "echo $TAMIAS_TEST_VAR", &vars).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "fake-injected");
        // and the var didn't leak into *our* env
        assert!(std::env::var("TAMIAS_TEST_VAR").is_err());
        assert!(output_with_env(Path::new("/nonexistent-dir-xyz"), "echo x", &vars).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn children_do_not_inherit_parent_credentials() {
        // HIGH-2: the parent's env is scrubbed — an TAMIAS_* var or
        // an arbitrary credential must not ride into the child,
        // while allowlisted session vars and injected vars do.
        let d = tmpdir("scrub");
        temp_env::with_vars(
            [
                ("TAMIAS_CF_TOKEN", Some("fake-parent-token")),
                ("TAMIAS_IDENTITY_FILE", Some("/tmp/fake-id")),
                ("TAMIAS_CONFIG", Some("/tmp/fake-tamias.toml")),
                ("PARENT_CREDENTIAL_XYZ", Some("fake-cred")),
                ("TAMIAS_TEST_SESSION_XDG_RUN", Some("kept")),
            ],
            || {
                let vars = [("INJECTED".to_string(), "fake-injected".to_string())];
                let out = output_with_env(
                    &d,
                    "for v in TAMIAS_CF_TOKEN TAMIAS_IDENTITY_FILE TAMIAS_CONFIG \
                     PARENT_CREDENTIAL_XYZ; do \
                     if printenv $v >/dev/null 2>&1; then echo \"leak:$v\"; fi; \
                     done; \
                     printenv HOME >/dev/null && echo ok-home; \
                     printenv PATH >/dev/null && echo ok-path; \
                     printenv INJECTED",
                    &vars,
                )
                .unwrap();
                let text = String::from_utf8_lossy(&out.stdout);
                assert!(!text.contains("leak:"), "{text}");
                assert!(text.contains("ok-home"), "{text}");
                assert!(text.contains("ok-path"), "{text}");
                assert!(text.contains("fake-injected"), "{text}");
                // argv mode scrubs identically
                let st = exec_with_env(
                    &d,
                    &["printenv".to_string(), "TAMIAS_CF_TOKEN".to_string()],
                    &[],
                )
                .unwrap();
                assert!(!st.success(), "printenv must fail on a scrubbed var");
            },
        );
        // XDG_/LC_ prefixes and the allowlist table itself
        assert!(allowed_var("XDG_CONFIG_HOME"));
        assert!(allowed_var("LC_ALL"));
        assert!(allowed_var("PATH"));
        assert!(!allowed_var("AWS_SECRET_ACCESS_KEY"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn exec_argv_needs_no_shell() {
        let d = tmpdir("exec");
        let vars = [("TAMIAS_TEST_VAR".to_string(), "fake-via-argv".to_string())];
        // `printenv VAR` exits 0 and prints the value; with a shell
        // the quoting would matter — argv mode doesn't care.
        let st = exec_with_env(
            &d,
            &["printenv".to_string(), "TAMIAS_TEST_VAR".to_string()],
            &vars,
        )
        .unwrap();
        assert!(st.success());
        // missing program → io error, not a shell message
        assert!(exec_with_env(&d, &["definitely-not-a-binary".to_string()], &[]).is_err());
        assert!(exec_with_env(&d, &[], &[]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
