#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-deploy-local — the `deploy` port's local adapter: manage a
//! **per-user service unit** (systemd on Linux; launchd on macOS is
//! documented but not yet implemented — every verb is systemd-only for
//! now). tamias owns the host-service *mechanism*; a package's
//! `provider.services[]` manifest block supplies the domain spec
//! (ExecStart argv, env, workdir), and `tamias deploy` renders it here.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::{Path, PathBuf};
use std::process::Command;

use tamias_core::Error;

/// What to deploy as a service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Unit base name (→ `<name>.service`).
    pub name: String,
    /// Short description.
    pub description: String,
    /// `ExecStart` argv — rendered with per-argument systemd quoting
    /// (see [`quote_arg`]). Parse a command-line string with
    /// [`split_exec`] first.
    pub exec: Vec<String>,
    /// `K=V` environment entries → `Environment=` lines.
    pub env: Vec<(String, String)>,
    /// `EnvironmentFile=` path — always rendered with systemd's `-`
    /// (optional-file) prefix, so a missing file doesn't fail the unit.
    /// `~` expands to `$HOME` at render time.
    pub env_file: Option<String>,
    /// `Restart=` policy — `on-failure` when `None`.
    pub restart: Option<String>,
    /// `RestartSec=` seconds — `5` when `None`.
    pub restart_sec: Option<u32>,
    /// `WorkingDirectory=` — `~` expands to `$HOME` at render time.
    pub workdir: Option<String>,
    /// Extra `After=`/`Wants=` targets beyond `network-online.target`,
    /// which is always emitted for services.
    pub after: Vec<String>,
}

/// The seam that runs host commands (`systemctl`, `journalctl`) — injected
/// for tests.
pub trait Runner: Send + Sync {
    /// Run `program args…`, erroring on a non-zero exit.
    ///
    /// # Errors
    /// Spawn/exit failures.
    fn run(&self, program: &str, args: &[&str]) -> Result<(), Error>;
}

/// The real runner — shells the host tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<(), Error> {
        let out = Command::new(program)
            .args(args)
            .output()
            .map_err(Error::io(format!("{program} {}", args.join(" "))))?;
        if !out.status.success() {
            return Err(Error::Failed(format!(
                "{program} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

/// The local deployer — a unit dir plus a [`Runner`].
pub struct LocalDeploy {
    unit_dir: PathBuf,
    runner: Box<dyn Runner>,
}

impl LocalDeploy {
    /// The default: `~/.config/systemd/user` + [`SystemRunner`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_default(unit_dir(), Box::new(SystemRunner))
    }

    /// An explicit unit dir + runner (tests inject a tempdir + recorder).
    #[must_use]
    pub fn with_default(unit_dir: PathBuf, runner: Box<dyn Runner>) -> Self {
        Self { unit_dir, runner }
    }

    /// The unit dir.
    #[must_use]
    pub fn unit_dir(&self) -> &Path {
        &self.unit_dir
    }

    /// The unit file path for `name`.
    ///
    /// # Errors
    /// [`Error::Failed`] when `name` is not a valid unit base name —
    /// the name becomes a filename, so traversal (`../`) and separators
    /// are rejected.
    pub fn unit_path(&self, name: &str) -> Result<PathBuf, Error> {
        check_unit_name(name)?;
        Ok(self.unit_dir.join(format!("{name}.service")))
    }

    /// Write the unit file (does not enable it).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid unit name, an empty `exec` argv,
    /// or a field containing a line break/NUL (unit-file injection);
    /// propagates write failures.
    pub fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, Error> {
        check_unit_name(&spec.name)?;
        check_unit_fields(spec)?;
        std::fs::create_dir_all(&self.unit_dir)
            .map_err(Error::io(format!("create {}", self.unit_dir.display())))?;
        let path = self.unit_path(&spec.name)?;
        std::fs::write(&path, render_unit(spec))
            .map_err(Error::io(format!("write {}", path.display())))?;
        Ok(path)
    }

    /// `systemctl --user daemon-reload` — call after writing/removing
    /// unit files so systemd sees them. Best-effort by convention:
    /// callers that can't tolerate a missing user bus ignore the error.
    ///
    /// # Errors
    /// [`Error::Failed`] when `systemctl` fails.
    pub fn daemon_reload(&self) -> Result<(), Error> {
        self.runner.run("systemctl", &["--user", "daemon-reload"])
    }

    /// `systemctl --user enable --now <unit>` (best-effort daemon-reload
    /// first, so a just-written unit resolves).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid unit name; propagates the runner
    /// failure.
    pub fn enable(&self, name: &str) -> Result<(), Error> {
        let _ = self.daemon_reload();
        self.runner
            .run("systemctl", &["--user", "enable", "--now", &unit(name)?])
    }

    /// `systemctl --user disable --now <unit>`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid unit name; propagates the runner
    /// failure.
    pub fn disable(&self, name: &str) -> Result<(), Error> {
        self.runner
            .run("systemctl", &["--user", "disable", "--now", &unit(name)?])
    }

    /// `systemctl --user status <unit>`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid unit name; propagates the runner
    /// failure.
    pub fn status(&self, name: &str) -> Result<(), Error> {
        self.runner.run(
            "systemctl",
            &["--user", "status", "--no-pager", &unit(name)?],
        )
    }

    /// `journalctl --user -u <unit> -n 200`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid unit name; propagates the runner
    /// failure.
    pub fn logs(&self, name: &str) -> Result<(), Error> {
        self.runner.run(
            "journalctl",
            &["--user", "-u", &unit(name)?, "-n", "200", "--no-pager"],
        )
    }

    /// Disable (best-effort) + remove the unit file + daemon-reload
    /// (best-effort).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid unit name; propagates the unit-file
    /// removal failure.
    pub fn uninstall(&self, name: &str) -> Result<(), Error> {
        let _ = self.disable(name);
        let path = self.unit_path(name)?;
        if path.exists() {
            std::fs::remove_file(&path).map_err(Error::io(format!("remove {}", path.display())))?;
        }
        let _ = self.daemon_reload();
        Ok(())
    }
}

impl Default for LocalDeploy {
    fn default() -> Self {
        Self::new()
    }
}

/// A valid unit base name: `[A-Za-z0-9_.@-]`, no separators, no `..`,
/// not `.`/`..`. The name becomes both a filename (`<name>.service`)
/// and a `systemctl` argument, so anything outside this set is rejected.
fn check_unit_name(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '@' | '-'));
    if ok {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "invalid unit name '{name}' (expected [A-Za-z0-9_.@-], no '..')"
        )))
    }
}

/// Reject spec fields that could smuggle extra unit directives — a
/// `\n`/`\r`/NUL in any rendered field (including every `exec` argv
/// element) would inject lines into the unit file.
fn check_unit_fields(spec: &ServiceSpec) -> Result<(), Error> {
    if spec.exec.is_empty() {
        return Err(Error::Failed(format!(
            "unit '{}' has an empty exec argv",
            spec.name
        )));
    }
    let bad = |what: &str, v: &str| {
        if v.contains(['\n', '\r', '\0']) {
            Some(Error::Failed(format!(
                "unit {what} contains a line break or NUL"
            )))
        } else {
            None
        }
    };
    for (what, v) in [
        ("name", spec.name.as_str()),
        ("description", spec.description.as_str()),
    ] {
        if let Some(e) = bad(what, v) {
            return Err(e);
        }
    }
    for (i, arg) in spec.exec.iter().enumerate() {
        if let Some(e) = bad(&format!("exec argv[{i}]"), arg) {
            return Err(e);
        }
    }
    for (what, v) in [
        ("env_file", spec.env_file.as_deref().unwrap_or("")),
        ("workdir", spec.workdir.as_deref().unwrap_or("")),
    ] {
        // Check the *rendered* (post-`~`-expansion) value — that's what
        // lands on the unit-file line.
        if let Some(e) = bad(what, &expand_home(v)) {
            return Err(e);
        }
    }
    if let Some(e) = bad("restart", spec.restart.as_deref().unwrap_or("")) {
        return Err(e);
    }
    for target in &spec.after {
        if let Some(e) = bad("after target", target) {
            return Err(e);
        }
    }
    for (k, v) in &spec.env {
        if let Some(e) = bad("env key", k).or_else(|| bad("env value", v)) {
            return Err(e);
        }
    }
    Ok(())
}

fn unit(name: &str) -> Result<String, Error> {
    check_unit_name(name)?;
    Ok(format!("{name}.service"))
}

/// Strip characters that would break out of a single unit-file line —
/// defense in depth for callers that render without field validation.
fn unit_field(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\n' | '\r' | '\0'))
        .collect()
}

/// Expand a leading `~` (or `~/…`) to `$HOME` — the manifest `workdir` /
/// `env_file` fields allow `~`, and YAML needs it quoted (`"~"`) since a
/// bare `~` parses as null. `~user` forms and a missing `HOME` pass
/// through unchanged.
#[must_use]
pub fn expand_home(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|_| path.to_string());
    }
    path.to_string()
}

/// Split a command-line string into argv — a deliberately small
/// POSIX-shell subset, no external deps:
///
/// - unquoted whitespace separates arguments;
/// - `'…'` is literal (no escapes);
/// - `"…"` is literal except `\"` → `"` and `\\` → `\`;
/// - `\c` outside quotes yields `c` (so `\ ` escapes a space);
/// - unterminated quotes are an error; empty/whitespace input yields an
///   empty argv (callers reject it — a unit needs an `ExecStart`).
///
/// `#` comments, `$VAR`, globs, and `;`/`|`/`&` are *not* special — the
/// string is data, never re-parsed by a shell.
///
/// # Errors
/// [`Error::Failed`] on an unterminated quote.
pub fn split_exec(exec: &str) -> Result<Vec<String>, Error> {
    #[derive(PartialEq)]
    enum State {
        Bare,
        Single,
        Double,
    }
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut state = State::Bare;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            State::Bare => match c {
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        argv.push(std::mem::take(&mut cur));
                    }
                }
                '\'' => state = State::Single,
                '"' => state = State::Double,
                '\\' => {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    } else {
                        cur.push('\\');
                    }
                }
                _ => cur.push(c),
            },
            State::Single => {
                if c == '\'' {
                    state = State::Bare;
                } else {
                    cur.push(c);
                }
            }
            State::Double => match c {
                '"' => state = State::Bare,
                '\\' => match chars.peek() {
                    Some('"' | '\\') => cur.push(chars.next().unwrap_or('\\')),
                    _ => cur.push('\\'),
                },
                _ => cur.push(c),
            },
        }
    }
    if state != State::Bare {
        return Err(Error::Failed(format!(
            "unterminated quote in exec '{exec}'"
        )));
    }
    if !cur.is_empty() {
        argv.push(cur);
    }
    Ok(argv)
}

/// systemd escapes ExecStart args by double-quoting (never
/// single-quoting) — same rule as hypha-systemd's renderer.
fn quote_arg(arg: &str) -> String {
    if arg
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

/// Render a systemd `.service` unit.
///
/// `[Unit]` carries `After=`/`Wants=` for `network-online.target` plus
/// any `spec.after` extras; `[Service]` renders `Type=simple`, the
/// quoted `ExecStart` argv, an optional `EnvironmentFile=-<path>`,
/// `WorkingDirectory=`, `Environment=` lines, and
/// `Restart=`/`RestartSec=`; `[Install]` targets `default.target`.
#[must_use]
pub fn render_unit(spec: &ServiceSpec) -> String {
    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str(&format!("Description={}\n", unit_field(&spec.description)));
    unit.push_str("After=network-online.target");
    for target in &spec.after {
        unit.push_str(&format!(" {}", unit_field(target)));
    }
    unit.push('\n');
    unit.push_str("Wants=network-online.target\n\n");
    unit.push_str("[Service]\n");
    unit.push_str("Type=simple\n");
    if let Some(env_file) = &spec.env_file {
        unit.push_str(&format!(
            "EnvironmentFile=-{}\n",
            unit_field(&expand_home(env_file))
        ));
    }
    unit.push_str(&format!(
        "ExecStart={}\n",
        spec.exec
            .iter()
            .map(|a| quote_arg(&unit_field(a)))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    if let Some(workdir) = &spec.workdir {
        unit.push_str(&format!(
            "WorkingDirectory={}\n",
            unit_field(&expand_home(workdir))
        ));
    }
    for (k, v) in &spec.env {
        unit.push_str(&format!(
            "Environment={}={}\n",
            unit_field(k),
            unit_field(v)
        ));
    }
    unit.push_str(&format!(
        "Restart={}\n",
        unit_field(spec.restart.as_deref().unwrap_or("on-failure"))
    ));
    unit.push_str(&format!("RestartSec={}\n", spec.restart_sec.unwrap_or(5)));
    unit.push_str("\n[Install]\n");
    unit.push_str("WantedBy=default.target\n");
    unit
}

/// `$XDG_CONFIG_HOME/systemd/user` → `~/.config/systemd/user`.
fn unit_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(|d| PathBuf::from(d).join("systemd/user"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/systemd/user")))
        .unwrap_or_else(|| PathBuf::from(".config/systemd/user"))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);
    impl Runner for Recorder {
        fn run(&self, program: &str, args: &[&str]) -> Result<(), Error> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            Ok(())
        }
    }

    fn spec() -> ServiceSpec {
        ServiceSpec {
            name: "peregrin".into(),
            description: "peregrin node".into(),
            exec: split_exec("/usr/bin/peregrin serve").unwrap(),
            env: vec![(
                "PEREGRIN_HOME".into(),
                "/home/u/.local/share/peregrin".into(),
            )],
            env_file: None,
            restart: None,
            restart_sec: None,
            workdir: None,
            after: vec![],
        }
    }

    #[test]
    fn renders_a_unit() {
        let text = render_unit(&spec());
        assert!(text.contains("ExecStart=/usr/bin/peregrin serve"));
        assert!(text.contains("Environment=PEREGRIN_HOME=/home/u/.local/share/peregrin"));
        assert!(text.contains("WantedBy=default.target"));
        assert!(text.contains("After=network-online.target"));
        assert!(text.contains("Wants=network-online.target"));
        assert!(text.contains("Restart=on-failure"));
        assert!(text.contains("RestartSec=5"));
    }

    #[test]
    fn renders_env_file_workdir_and_extras() {
        let spec = ServiceSpec {
            env_file: Some("~/.config/x/env".into()),
            workdir: Some("~/srv".into()),
            restart: Some("always".into()),
            restart_sec: Some(2),
            after: vec!["postgresql.service".into()],
            ..spec()
        };
        let text = render_unit(&spec);
        let home = std::env::var("HOME").unwrap_or_default();
        assert!(text.contains(&format!("EnvironmentFile=-{home}/.config/x/env")));
        assert!(text.contains(&format!("WorkingDirectory={home}/srv")));
        assert!(text.contains("Restart=always"));
        assert!(text.contains("RestartSec=2"));
        assert!(text.contains("After=network-online.target postgresql.service"));
    }

    #[test]
    fn quotes_argv_with_spaces_and_quotes() {
        let spec = ServiceSpec {
            exec: vec![
                "/usr/bin/tool".into(),
                "two words".into(),
                "say \"hi\"".into(),
                "back\\slash".into(),
            ],
            ..spec()
        };
        let text = render_unit(&spec);
        // `back\slash` carries no whitespace/quote → unquoted, verbatim.
        assert!(
            text.contains("ExecStart=/usr/bin/tool \"two words\" \"say \\\"hi\\\"\" back\\slash")
        );
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("a'b"), "\"a'b\"");
    }

    #[test]
    fn split_exec_quotes_and_escapes() {
        assert_eq!(
            split_exec("prog 'a b' \"c d\" e\\ f").unwrap(),
            ["prog", "a b", "c d", "e f"]
        );
        assert_eq!(split_exec("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_exec("a'b'c\"d\"e").unwrap(), ["abcde"]);
        assert_eq!(split_exec("\"a\\\"b\"").unwrap(), ["a\"b"]);
        assert_eq!(split_exec("'a\\b'").unwrap(), ["a\\b"]);
        assert!(split_exec("unclosed '").is_err());
        assert!(split_exec("unclosed \"").is_err());
    }

    #[test]
    fn expand_home_cases() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/x"), format!("{home}/x"));
        assert_eq!(expand_home("~other/x"), "~other/x");
        assert_eq!(expand_home("/abs"), "/abs");
    }

    #[test]
    fn install_enable_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            LocalDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        let path = deploy.install(&spec()).unwrap();
        assert!(path.is_file());
        deploy.enable("peregrin").unwrap();
        deploy.uninstall("peregrin").unwrap();
        assert!(!deploy.unit_path("peregrin").unwrap().exists());
        deploy.daemon_reload().unwrap();
    }

    #[test]
    fn rejects_unit_name_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            LocalDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        for bad in [
            "../escape",
            "a/b",
            "..",
            ".",
            "",
            "a b",
            "x\nExecStart=/bin/evil",
        ] {
            assert!(deploy.unit_path(bad).is_err(), "name {bad:?} accepted");
            assert!(deploy.enable(bad).is_err(), "enable {bad:?} accepted");
            assert!(deploy.uninstall(bad).is_err(), "uninstall {bad:?} accepted");
        }
        // nothing escaped the unit dir
        assert!(!dir.path().parent().unwrap().join("escape.service").exists());
    }

    #[test]
    fn rejects_newline_injection_in_spec() {
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            LocalDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        for evil in [
            ServiceSpec {
                description: "d\nExecStart=/bin/evil".into(),
                ..spec()
            },
            ServiceSpec {
                exec: vec!["/bin/x".into(), "arg\nRestart=always".into()],
                ..spec()
            },
            ServiceSpec {
                env: vec![("A\nB".into(), "1".into())],
                ..spec()
            },
            ServiceSpec {
                env: vec![("A".into(), "1\r\n[Install]".into())],
                ..spec()
            },
            ServiceSpec {
                workdir: Some("/w\nX=y".into()),
                ..spec()
            },
            ServiceSpec {
                env_file: Some("/e\nX=y".into()),
                ..spec()
            },
            ServiceSpec {
                restart: Some("always\n[Install]".into()),
                ..spec()
            },
            ServiceSpec {
                after: vec!["net\nWants=x".into()],
                ..spec()
            },
            ServiceSpec {
                exec: vec![],
                ..spec()
            },
        ] {
            assert!(deploy.install(&evil).is_err());
        }
        // render_unit alone still cannot emit an injected directive
        let text = render_unit(&ServiceSpec {
            description: "d\n[Service]".into(),
            exec: vec!["/bin/x\r\nRestart=always".into()],
            ..spec()
        });
        assert!(!text.contains("d\n"));
        assert_eq!(text.matches("Restart=on-failure").count(), 1);
    }

    struct FailRunner;
    impl Runner for FailRunner {
        fn run(&self, _program: &str, _args: &[&str]) -> Result<(), Error> {
            Err(Error::Failed("boom".into()))
        }
    }

    #[test]
    fn error_arm_and_extra_verbs() {
        let dir = tempfile::tempdir().unwrap();
        let fail = LocalDeploy::with_default(dir.path().to_path_buf(), Box::new(FailRunner));
        assert!(fail.enable("p").is_err());
        assert!(fail.daemon_reload().is_err());
        let rec =
            LocalDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        rec.status("p").unwrap();
        rec.logs("p").unwrap();
        rec.disable("p").unwrap();
        rec.uninstall("missing").unwrap();
        assert_eq!(rec.unit_dir(), dir.path());
    }

    #[test]
    fn default_uses_system_runner() {
        let deploy = LocalDeploy::new();
        assert!(deploy.unit_dir().to_str().is_some());
        assert!(deploy.enable("nonexistent-unit-xyz").is_err());
    }

    #[test]
    fn render_with_workdir_and_env() {
        let spec = ServiceSpec {
            name: "x".into(),
            description: "d".into(),
            exec: vec!["/bin/x".into()],
            env: vec![("A".into(), "1".into())],
            env_file: None,
            restart: None,
            restart_sec: None,
            workdir: Some("/w".into()),
            after: vec![],
        };
        let text = render_unit(&spec);
        assert!(text.contains("WorkingDirectory=/w"));
        assert!(text.contains("Environment=A=1"));
    }

    #[test]
    fn default_uses_new() {
        let _ = LocalDeploy::default();
    }
}
