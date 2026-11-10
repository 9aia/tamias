#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-deploy-windows — the `deploy` port's Windows adapter: manage a
//! **per-user scheduled task** through `schtasks.exe` (Task Scheduler).
//! No admin needed: tasks register under the interactive user with
//! `RunLevel=LeastPrivilege`.
//!
//! A [`ServiceSpec`] renders to two artifacts under the task dir
//! (`%LOCALAPPDATA%\tamias\tasks` by default):
//!
//! - `<name>.cmd` — a generated batch wrapper that applies the spec's
//!   environment (`env_file` via `for /f`, inline `env` as `set`
//!   lines, inline winning — systemd's order) and `cd /d`s to
//!   `workdir` before exec'ing the argv. Task Scheduler actions
//!   cannot carry env vars, so the wrapper is how env reaches the
//!   process.
//! - `<name>.xml` — the task definition imported via
//!   `schtasks /create /xml … /f`: a `LogonTrigger` (the honest analog
//!   of a systemd *user* unit starting at login), `InteractiveToken`
//!   principal, and settings tuned for a long-running service
//!   (`ExecutionTimeLimit=PT0S` — the 72h default would kill daemons;
//!   battery blocks off; `RestartOnFailure` mapped from `restart`).
//!
//! ## Field mapping vs. the systemd backend
//!
//! | `ServiceSpec` | systemd | this backend |
//! | --- | --- | --- |
//! | `name` | `<name>.service` | task name (`/tn`) |
//! | `description` | `Description=` | `<RegistrationInfo><Description>` |
//! | `exec` | `ExecStart=` argv | wrapper `.cmd` last line (cmd quoting) |
//! | `env` | `Environment=` | `set "K=V"` lines in the wrapper |
//! | `env_file` | `EnvironmentFile=-` | `for /f` loader in the wrapper — `K=V`/`#` only; quoted values keep their quotes (systemd strips them) |
//! | `workdir` | `WorkingDirectory=` | `<WorkingDirectory>` + `cd /d` in the wrapper |
//! | `restart`/`restart_sec` | `Restart=`/`RestartSec=` | `<RestartOnFailure>` — see gaps |
//! | `after` | `After=`/`Wants=` | **ignored** — Task Scheduler has no unit ordering |
//!
//! ## Honest gaps
//!
//! - **Restart count is capped.** systemd's `on-failure`/`always`
//!   retries forever; Task Scheduler's `RestartOnFailure` takes a
//!   `Count` of 1–255 — we emit `255` and document the cap.
//!   `restart="always"` also can't restart on a *clean* exit(0) —
//!   `RestartOnFailure` only fires on failure, so `always` degrades to
//!   on-failure semantics. `restart_sec` maps to `Interval`, clamped
//!   up to the schema minimum of `PT1M`.
//! - **`env_file` is read by `for /f`**, not by the OS: only `K=V`
//!   lines and `#` comments are understood; systemd-isms like
//!   `export K=V`, quoting, and variable expansion are not.
//! - **No journal.** `status`/`logs` are best-effort `schtasks /query`
//!   output — Task Scheduler keeps no per-task log; capture service
//!   logs inside the payload (or a `provider.health.cmd`) instead.
//! - **`stop` is `schtasks /end`** — it terminates the process tree
//!   root, not necessarily grandchildren the way systemd's cgroup
//!   kill does.
//! - Real `schtasks.exe` execution is `#[cfg(windows)]`-gated
//!   (`SchtasksRunner` only exists there); everywhere else
//!   [`WindowsDeploy::new`]
//!   installs an unavailable runner so verbs fail loudly instead of
//!   shelling out. Tests inject a [`Runner`] recorder on any platform.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

use tamias_core::Error;
use tamias_deploy::{expand_home, ServiceSpec};

/// The seam that runs host commands (`schtasks`) — injected for tests.
pub trait Runner: Send + Sync {
    /// Run `program args…`, erroring on a non-zero exit.
    ///
    /// # Errors
    /// Spawn/exit failures.
    fn run(&self, program: &str, args: &[&str]) -> Result<(), Error>;
}

/// The real runner — shells out to `schtasks.exe`. Only exists on
/// Windows: off-Windows there is no `schtasks` to run, and
/// [`WindowsDeploy::new`] never installs this runner there.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SchtasksRunner;

#[cfg(windows)]
impl Runner for SchtasksRunner {
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

/// Off-Windows stand-in for [`SchtasksRunner`] — every verb errors
/// instead of exec'ing a binary that doesn't exist. Only installed by
/// [`WindowsDeploy::new`] on non-Windows; tests inject their own
/// [`Runner`].
#[cfg(not(windows))]
struct UnavailableRunner;

#[cfg(not(windows))]
impl Runner for UnavailableRunner {
    fn run(&self, program: &str, _args: &[&str]) -> Result<(), Error> {
        Err(Error::Failed(format!(
            "{program} is only available on Windows"
        )))
    }
}

/// The Windows deployer — a task dir (wrapper `.cmd` + task `.xml`)
/// plus a [`Runner`].
pub struct WindowsDeploy {
    task_dir: PathBuf,
    runner: Box<dyn Runner>,
}

impl WindowsDeploy {
    /// The default: `%LOCALAPPDATA%\tamias\tasks` (falling back through
    /// `USERPROFILE`/`HOME`) + the platform runner — [`SchtasksRunner`]
    /// on Windows, an unavailable runner elsewhere.
    #[must_use]
    pub fn new() -> Self {
        #[cfg(windows)]
        let runner: Box<dyn Runner> = Box::new(SchtasksRunner);
        #[cfg(not(windows))]
        let runner: Box<dyn Runner> = Box::new(UnavailableRunner);
        Self::with_default(task_dir(), runner)
    }

    /// An explicit task dir + runner (tests inject a tempdir +
    /// recorder).
    #[must_use]
    pub fn with_default(task_dir: PathBuf, runner: Box<dyn Runner>) -> Self {
        Self { task_dir, runner }
    }

    /// The task dir.
    #[must_use]
    pub fn task_dir(&self) -> &Path {
        &self.task_dir
    }

    /// The wrapper `.cmd` path for `name`.
    ///
    /// # Errors
    /// [`Error::Failed`] when `name` is not a valid task name.
    pub fn wrapper_path(&self, name: &str) -> Result<PathBuf, Error> {
        check_task_name(name)?;
        Ok(self.task_dir.join(format!("{name}.cmd")))
    }

    /// The task XML path for `name`.
    ///
    /// # Errors
    /// [`Error::Failed`] when `name` is not a valid task name.
    pub fn xml_path(&self, name: &str) -> Result<PathBuf, Error> {
        check_task_name(name)?;
        Ok(self.task_dir.join(format!("{name}.xml")))
    }

    /// Write the wrapper `.cmd` + task `.xml`, then
    /// `schtasks /create /tn <name> /xml <file> /f` (registers or
    /// overwrites). Returns the task XML path.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name/spec field, a `.cmd`
    /// quoting failure, an empty `exec` argv, or a write/`schtasks`
    /// failure. On non-Windows the default runner fails at the
    /// `schtasks` step — the rendered files are still left on disk.
    pub fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, Error> {
        check_task_name(&spec.name)?;
        check_task_fields(spec)?;
        std::fs::create_dir_all(&self.task_dir)
            .map_err(Error::io(format!("create {}", self.task_dir.display())))?;
        let wrapper = self.wrapper_path(&spec.name)?;
        std::fs::write(&wrapper, render_wrapper(spec)?)
            .map_err(Error::io(format!("write {}", wrapper.display())))?;
        let xml = self.xml_path(&spec.name)?;
        std::fs::write(&xml, render_task_xml(spec, &wrapper))
            .map_err(Error::io(format!("write {}", xml.display())))?;
        let args = create_args(&spec.name, &xml)?;
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run("schtasks", &refs)?;
        Ok(xml)
    }

    /// No-op kept for parity with the systemd backend's
    /// `daemon-reload`: `schtasks /create` registers immediately, so
    /// there is nothing to reload.
    ///
    /// # Errors
    /// Never — always `Ok`.
    pub fn daemon_reload(&self) -> Result<(), Error> {
        Ok(())
    }

    /// `schtasks /change /tn <name> /enable` — tasks are created
    /// enabled, so this is mostly a re-assert after an external
    /// disable.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid task name; propagates the
    /// runner failure.
    pub fn enable(&self, name: &str) -> Result<(), Error> {
        self.schtasks(&change_args(name, true)?)
    }

    /// `schtasks /change /tn <name> /disable`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid task name; propagates the
    /// runner failure.
    pub fn disable(&self, name: &str) -> Result<(), Error> {
        self.schtasks(&change_args(name, false)?)
    }

    /// `schtasks /run /tn <name>`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid task name; propagates the
    /// runner failure.
    pub fn start(&self, name: &str) -> Result<(), Error> {
        self.schtasks(&run_args(name)?)
    }

    /// `schtasks /end /tn <name>` — terminates the task's process.
    /// Unlike systemd's cgroup stop this may leave grandchildren
    /// behind.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid task name; propagates the
    /// runner failure.
    pub fn stop(&self, name: &str) -> Result<(), Error> {
        self.schtasks(&end_args(name)?)
    }

    /// Best-effort restart: `/end` (ignored when the task isn't
    /// running — systemd `restart` starts a stopped unit too) then
    /// `/run`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid task name; propagates the
    /// `/run` failure.
    pub fn restart(&self, name: &str) -> Result<(), Error> {
        let _ = self.schtasks(&end_args(name)?);
        self.schtasks(&run_args(name)?)
    }

    /// `schtasks /query /tn <name> /v /fo list` — best-effort; the
    /// runner captures output, so this mainly surfaces "does the task
    /// exist" via the exit status.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid task name; propagates the
    /// runner failure.
    pub fn status(&self, name: &str) -> Result<(), Error> {
        self.schtasks(&query_args(name)?)
    }

    /// Best-effort status for one task — same as [`Self::status`].
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid task name; propagates the
    /// runner failure.
    pub fn logs(&self, name: &str) -> Result<(), Error> {
        self.status(name)
    }

    /// Best-effort "logs" over one or more tasks — Task Scheduler
    /// keeps no per-task log, so this runs `schtasks /query` per task
    /// name (`lines`/`follow` are accepted for signature parity and
    /// ignored). Returns an error on an empty name list.
    ///
    /// # Errors
    /// [`Error::Failed`] on an empty/invalid name list; propagates the
    /// first runner failure.
    pub fn logs_with(&self, names: &[String], _lines: u32, _follow: bool) -> Result<(), Error> {
        if names.is_empty() {
            return Err(Error::Failed("logs: no tasks given".into()));
        }
        for name in names {
            self.status(name)?;
        }
        Ok(())
    }

    /// `schtasks /delete /tn <name> /f` (best-effort — a missing task
    /// must not fail uninstall) + remove the wrapper `.cmd` and task
    /// `.xml`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid task name; propagates the file
    /// removal failure.
    pub fn uninstall(&self, name: &str) -> Result<(), Error> {
        let _ = self.schtasks(&delete_args(name)?);
        for path in [self.xml_path(name)?, self.wrapper_path(name)?] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(Error::io(format!("remove {}", path.display())))?;
            }
        }
        Ok(())
    }

    fn schtasks(&self, args: &[String]) -> Result<(), Error> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run("schtasks", &refs)
    }
}

impl Default for WindowsDeploy {
    fn default() -> Self {
        Self::new()
    }
}

/// `schtasks /create /tn <name> /xml <file> /f` — register (or
/// overwrite, `/f`) the task from the rendered XML.
///
/// # Errors
/// [`Error::Failed`] on an invalid task name.
pub fn create_args(name: &str, xml: &Path) -> Result<Vec<String>, Error> {
    check_task_name(name)?;
    Ok(vec![
        "/create".into(),
        "/tn".into(),
        name.to_string(),
        "/xml".into(),
        xml.to_string_lossy().into_owned(),
        "/f".into(),
    ])
}

/// `schtasks /change /tn <name> /enable|/disable`.
///
/// # Errors
/// [`Error::Failed`] on an invalid task name.
pub fn change_args(name: &str, enable: bool) -> Result<Vec<String>, Error> {
    check_task_name(name)?;
    Ok(vec![
        "/change".into(),
        "/tn".into(),
        name.to_string(),
        if enable { "/enable" } else { "/disable" }.into(),
    ])
}

/// `schtasks /run /tn <name>`.
///
/// # Errors
/// [`Error::Failed`] on an invalid task name.
pub fn run_args(name: &str) -> Result<Vec<String>, Error> {
    check_task_name(name)?;
    Ok(vec!["/run".into(), "/tn".into(), name.to_string()])
}

/// `schtasks /end /tn <name>`.
///
/// # Errors
/// [`Error::Failed`] on an invalid task name.
pub fn end_args(name: &str) -> Result<Vec<String>, Error> {
    check_task_name(name)?;
    Ok(vec!["/end".into(), "/tn".into(), name.to_string()])
}

/// `schtasks /delete /tn <name> /f`.
///
/// # Errors
/// [`Error::Failed`] on an invalid task name.
pub fn delete_args(name: &str) -> Result<Vec<String>, Error> {
    check_task_name(name)?;
    Ok(vec![
        "/delete".into(),
        "/tn".into(),
        name.to_string(),
        "/f".into(),
    ])
}

/// `schtasks /query /tn <name> /v /fo list` — verbose human-readable
/// status.
///
/// # Errors
/// [`Error::Failed`] on an invalid task name.
pub fn query_args(name: &str) -> Result<Vec<String>, Error> {
    check_task_name(name)?;
    Ok(vec![
        "/query".into(),
        "/tn".into(),
        name.to_string(),
        "/v".into(),
        "/fo".into(),
        "list".into(),
    ])
}

/// `schtasks /query /tn <name> /fo csv /nh` — one CSV row, no header;
/// the last field is the status (`Ready`, `Running`, …). Used by
/// `provider list`'s state probe.
///
/// # Errors
/// [`Error::Failed`] on an invalid task name.
pub fn query_csv_args(name: &str) -> Result<Vec<String>, Error> {
    check_task_name(name)?;
    Ok(vec![
        "/query".into(),
        "/tn".into(),
        name.to_string(),
        "/fo".into(),
        "csv".into(),
        "/nh".into(),
    ])
}

/// Pull the status field out of `query_csv_args` output — CSV columns
/// are `TaskName,Next Run Time,Status`; quoted fields, status last.
/// Returns `None` on empty/unparseable output (a line without a comma
/// is an error message, not CSV).
#[must_use]
pub fn csv_status(output: &str) -> Option<String> {
    let line = output
        .lines()
        .find(|l| !l.trim().is_empty() && l.contains(','))?;
    let last = line.rsplit(',').next()?.trim().trim_matches('"').trim();
    (!last.is_empty()).then(|| last.to_string())
}

/// A valid task name: non-empty, no path separators or Task
/// Scheduler-hostile characters (`\ / : * ? " < > |`), no control
/// chars, no `..`, no leading/trailing whitespace. Spaces are legal —
/// the name travels as a single argv element, never through a shell.
fn check_task_name(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name == name.trim()
        && name != "."
        && name != ".."
        && !name.contains("..")
        && name
            .chars()
            .all(|c| !c.is_control() && !"\\/:*?\"<>|".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "invalid task name '{name}' (no separators, '..', or \\ / : * ? \" < > |)"
        )))
    }
}

/// Reject spec fields that could break out of the `.cmd` wrapper or
/// the task XML — a `\n`/`\r`/NUL lands as extra batch lines or XML
/// content; a `"` in an env value or path terminates the `set "…"`
/// quoting early.
fn check_task_fields(spec: &ServiceSpec) -> Result<(), Error> {
    if spec.exec.is_empty() {
        return Err(Error::Failed(format!(
            "task '{}' has an empty exec argv",
            spec.name
        )));
    }
    let bad = |what: &str, v: &str| {
        if v.contains(['\n', '\r', '\0']) {
            Some(Error::Failed(format!(
                "task {what} contains a line break or NUL"
            )))
        } else {
            None
        }
    };
    let quoted = |what: &str, v: &str| {
        if v.contains('"') {
            Some(Error::Failed(format!(
                "task {what} contains a double quote (unsupported in the .cmd wrapper)"
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
        // lands in the wrapper/XML.
        let expanded = expand_home(v);
        if let Some(e) = bad(what, &expanded).or_else(|| quoted(what, &expanded)) {
            return Err(e);
        }
    }
    if let Some(e) = bad("restart", spec.restart.as_deref().unwrap_or("")) {
        return Err(e);
    }
    for (k, v) in &spec.env {
        if let Some(e) = bad("env key", k)
            .or_else(|| bad("env value", v))
            .or_else(|| quoted("env key", k))
            .or_else(|| quoted("env value", v))
        {
            return Err(e);
        }
    }
    // `spec.after` is intentionally not validated: no Windows backend
    // renders it (documented gap — no ordering primitive).
    Ok(())
}

/// Escape `%` for batch-file context — `%%` collapses to a literal `%`
/// when cmd.exe parses a `.cmd`, so user data can't smuggle `%VAR%`
/// expansions into the wrapper.
fn pct(s: &str) -> String {
    s.replace('%', "%%")
}

/// Quote one argv element for the wrapper's final command line —
/// cmd.exe rules, not POSIX: `%` is percent-escaped; an arg with
/// whitespace or cmd metacharacters is wrapped in `"…"`; a literal
/// `"` cannot be represented and is an error (documented gap).
///
/// # Errors
/// [`Error::Failed`] when `arg` contains a double quote.
fn cmd_quote(arg: &str) -> Result<String, Error> {
    if arg.contains('"') {
        return Err(Error::Failed(format!(
            "exec argument {arg:?} contains a double quote (unsupported by cmd.exe)"
        )));
    }
    let esc = pct(arg);
    if esc
        .chars()
        .any(|c| c.is_whitespace() || "&|<>^()".contains(c))
    {
        Ok(format!("\"{esc}\""))
    } else {
        Ok(esc)
    }
}

/// Render the wrapper `.cmd` — env first (`env_file` via `for /f`,
/// then inline `env` overriding, systemd's order), `cd /d` to
/// `workdir`, then the quoted argv, then `exit /b %errorlevel%`.
///
/// # Errors
/// [`Error::Failed`] on an empty `exec` argv or an argument
/// [`cmd_quote`] rejects.
pub fn render_wrapper(spec: &ServiceSpec) -> Result<String, Error> {
    let mut cmd = String::new();
    cmd.push_str("@echo off\r\n");
    cmd.push_str("setlocal\r\n");
    cmd.push_str(&format!(
        "rem tamias task '{}' — generated; do not edit\r\n",
        spec.name.replace(['\r', '\n'], " ")
    ));
    if !spec.description.is_empty() {
        cmd.push_str(&format!(
            "rem {}\r\n",
            spec.description.replace(['\r', '\n'], " ")
        ));
    }
    if let Some(env_file) = &spec.env_file {
        // `for /f` over a quoted path needs `usebackq`; `eol=#` skips
        // comments; `tokens=1,* delims==` splits K=V keeping '=' in V.
        cmd.push_str(&format!(
            "for /F \"usebackq eol=# tokens=1,* delims==\" %%A in (\"{}\") do set \"%%A=%%B\"\r\n",
            pct(&expand_home(env_file))
        ));
    }
    for (k, v) in &spec.env {
        cmd.push_str(&format!("set \"{}={}\"\r\n", pct(k), pct(v)));
    }
    if let Some(workdir) = &spec.workdir {
        cmd.push_str(&format!("cd /d \"{}\"\r\n", pct(&expand_home(workdir))));
    }
    let mut argv = spec.exec.iter();
    let Some(prog) = argv.next() else {
        return Err(Error::Failed(format!(
            "task '{}' has an empty exec argv",
            spec.name
        )));
    };
    let mut line = cmd_quote(&expand_home(prog))?;
    for arg in argv {
        line.push(' ');
        line.push_str(&cmd_quote(&expand_home(arg))?);
    }
    cmd.push_str(&line);
    cmd.push_str("\r\nexit /b %errorlevel%\r\n");
    Ok(cmd)
}

/// XML-escape element text (`&`, `<`, `>` — quotes are legal in
/// content and stay literal).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The restart delay as an ISO-8601 duration — the schema floor is
/// `PT1M`, so sub-minute `restart_sec` clamps up.
fn restart_interval(restart_sec: Option<u32>) -> String {
    let secs = restart_sec.unwrap_or(5).max(60);
    format!("PT{secs}S")
}

/// Whether the spec asks for restart-on-failure — mirrors systemd's
/// default (`on-failure` when unset); `"no"` disables, anything else
/// (including `"always"`, which can only approximate) enables.
fn restart_on_failure(spec: &ServiceSpec) -> bool {
    spec.restart.as_deref() != Some("no")
}

/// Render the Task Scheduler task XML — a `LogonTrigger`
/// (per-user service ≈ start at logon), `InteractiveToken` +
/// `LeastPrivilege` principal (no admin), service-tuned settings
/// (`ExecutionTimeLimit=PT0S` — the 72h default would kill daemons —
/// battery blocks off, `RestartOnFailure` from `restart`/
/// `restart_sec`), and an `Exec` action running `cmd.exe /d /c` on the
/// wrapper `.cmd`.
///
/// # Errors
/// Never fails on spec content — callers run [`check_task_fields`]
/// first; paths are XML-escaped.
#[must_use]
pub fn render_task_xml(spec: &ServiceSpec, wrapper: &Path) -> String {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n");
    xml.push_str("<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n");
    xml.push_str("  <RegistrationInfo>\r\n");
    xml.push_str(&format!(
        "    <Description>{}</Description>\r\n",
        xml_escape(&spec.description)
    ));
    xml.push_str("  </RegistrationInfo>\r\n");
    xml.push_str("  <Triggers>\r\n");
    xml.push_str("    <LogonTrigger>\r\n      <Enabled>true</Enabled>\r\n    </LogonTrigger>\r\n");
    xml.push_str("  </Triggers>\r\n");
    xml.push_str("  <Principals>\r\n    <Principal id=\"Author\">\r\n");
    xml.push_str("      <LogonType>InteractiveToken</LogonType>\r\n");
    xml.push_str("      <RunLevel>LeastPrivilege</RunLevel>\r\n");
    xml.push_str("    </Principal>\r\n  </Principals>\r\n");
    xml.push_str("  <Settings>\r\n");
    xml.push_str("    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\r\n");
    xml.push_str("    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\r\n");
    xml.push_str("    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\r\n");
    xml.push_str("    <AllowHardTerminate>true</AllowHardTerminate>\r\n");
    xml.push_str("    <StartWhenAvailable>true</StartWhenAvailable>\r\n");
    xml.push_str("    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\r\n");
    xml.push_str("    <Enabled>true</Enabled>\r\n");
    xml.push_str("    <Hidden>false</Hidden>\r\n");
    // PT0S = no time limit — the schema default (72h) would kill
    // long-running provider services.
    xml.push_str("    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\r\n");
    xml.push_str("    <Priority>7</Priority>\r\n");
    if restart_on_failure(spec) {
        xml.push_str("    <RestartOnFailure>\r\n");
        xml.push_str(&format!(
            "      <Interval>{}</Interval>\r\n",
            restart_interval(spec.restart_sec)
        ));
        // systemd retries unboundedly; the schema caps Count at 255.
        xml.push_str("      <Count>255</Count>\r\n");
        xml.push_str("    </RestartOnFailure>\r\n");
    }
    xml.push_str("  </Settings>\r\n");
    xml.push_str("  <Actions Context=\"Author\">\r\n    <Exec>\r\n");
    xml.push_str("      <Command>cmd.exe</Command>\r\n");
    xml.push_str(&format!(
        "      <Arguments>/d /c \"{}\"</Arguments>\r\n",
        xml_escape(&wrapper.to_string_lossy())
    ));
    if let Some(workdir) = &spec.workdir {
        xml.push_str(&format!(
            "      <WorkingDirectory>{}</WorkingDirectory>\r\n",
            xml_escape(&expand_home(workdir))
        ));
    }
    xml.push_str("    </Exec>\r\n  </Actions>\r\n</Task>\r\n");
    xml
}

/// `%LOCALAPPDATA%\tamias\tasks` → `%USERPROFILE%\AppData\Local\…` →
/// `$HOME/.local/share/…` → `./.tamias/tasks`.
fn task_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("tamias/tasks"))
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|h| PathBuf::from(h).join("AppData/Local/tamias/tasks"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/tamias/tasks"))
        })
        .unwrap_or_else(|| PathBuf::from(".tamias/tasks"))
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
            exec: vec!["C:\\tools\\peregrin.exe".into(), "serve".into()],
            env: vec![("PEREGRIN_HOME".into(), "C:\\data\\peregrin".into())],
            env_file: None,
            restart: None,
            restart_sec: None,
            workdir: None,
            after: vec![],
        }
    }

    #[test]
    fn renders_wrapper_env_and_command() {
        let text = render_wrapper(&spec()).unwrap();
        assert!(text.starts_with("@echo off\r\n"));
        assert!(text.contains("set \"PEREGRIN_HOME=C:\\data\\peregrin\""));
        assert!(text.contains("C:\\tools\\peregrin.exe serve"));
        assert!(text.ends_with("exit /b %errorlevel%\r\n"));
    }

    #[test]
    fn wrapper_loads_env_file_before_inline_env() {
        let spec = ServiceSpec {
            env_file: Some("~/.config/x/env".into()),
            env: vec![("A".into(), "inline".into())],
            ..spec()
        };
        let text = render_wrapper(&spec).unwrap();
        let home = std::env::var("HOME").unwrap_or_default();
        let for_at = text.find("for /F").unwrap();
        let set_at = text.find("set \"A=inline\"").unwrap();
        assert!(for_at < set_at, "env_file must load before inline env");
        assert!(text.contains(&format!("in (\"{home}/.config/x/env\")")));
    }

    #[test]
    fn wrapper_quotes_and_escapes_cmd_metachars() {
        let spec = ServiceSpec {
            exec: vec![
                "C:\\Program Files\\tool.exe".into(),
                "two words".into(),
                "a&b|c<d>e^f(g)".into(),
                "100%".into(),
            ],
            ..spec()
        };
        let text = render_wrapper(&spec).unwrap();
        assert!(text.contains("\"C:\\Program Files\\tool.exe\""));
        assert!(text.contains("\"two words\""));
        assert!(text.contains("\"a&b|c<d>e^f(g)\""));
        // `%` → `%%` so the batch parser can't expand it
        assert!(text.contains("100%%"));
    }

    #[test]
    fn cmd_quote_rejects_double_quote() {
        assert!(render_wrapper(&ServiceSpec {
            exec: vec!["prog".into(), "say \"hi\"".into()],
            ..spec()
        })
        .is_err());
        assert_eq!(cmd_quote("plain").unwrap(), "plain");
        assert_eq!(cmd_quote("a b").unwrap(), "\"a b\"");
    }

    #[test]
    fn renders_task_xml_logon_trigger_and_settings() {
        let xml = render_task_xml(&spec(), Path::new("C:\\tamias\\tasks\\peregrin.cmd"));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<Description>peregrin node</Description>"));
        assert!(xml.contains("<Command>cmd.exe</Command>"));
        assert!(xml.contains("/d /c \"C:\\tamias\\tasks\\peregrin.cmd\""));
        // default restart = on-failure parity → RestartOnFailure, 5s→PT1M floor
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("<Interval>PT60S</Interval>"));
        assert!(xml.contains("<Count>255</Count>"));
    }

    #[test]
    fn xml_restart_mapping() {
        let no = ServiceSpec {
            restart: Some("no".into()),
            ..spec()
        };
        assert!(!render_task_xml(&no, Path::new("x.cmd")).contains("RestartOnFailure"));
        let always = ServiceSpec {
            restart: Some("always".into()),
            restart_sec: Some(120),
            ..spec()
        };
        let xml = render_task_xml(&always, Path::new("x.cmd"));
        assert!(xml.contains("<Interval>PT120S</Interval>"));
        // sub-minute clamps to the schema floor
        let fast = ServiceSpec {
            restart_sec: Some(5),
            ..spec()
        };
        assert!(render_task_xml(&fast, Path::new("x.cmd")).contains("<Interval>PT60S</Interval>"));
    }

    #[test]
    fn xml_escapes_description_and_workdir() {
        let spec = ServiceSpec {
            description: "a <b> & \"c\"".into(),
            workdir: Some("C:\\work & play".into()),
            ..spec()
        };
        let xml = render_task_xml(&spec, Path::new("x.cmd"));
        assert!(xml.contains("a &lt;b&gt; &amp; \"c\""));
        assert!(xml.contains("<WorkingDirectory>C:\\work &amp; play</WorkingDirectory>"));
    }

    #[test]
    fn schtasks_arg_builders() {
        let xml = Path::new("C:\\tamias\\tasks\\p.xml");
        assert_eq!(
            create_args("p", xml).unwrap(),
            [
                "/create",
                "/tn",
                "p",
                "/xml",
                "C:\\tamias\\tasks\\p.xml",
                "/f"
            ]
        );
        assert_eq!(
            change_args("p", true).unwrap(),
            ["/change", "/tn", "p", "/enable"]
        );
        assert_eq!(change_args("p", false).unwrap()[3], "/disable");
        assert_eq!(run_args("p").unwrap(), ["/run", "/tn", "p"]);
        assert_eq!(end_args("p").unwrap(), ["/end", "/tn", "p"]);
        assert_eq!(delete_args("p").unwrap(), ["/delete", "/tn", "p", "/f"]);
        assert_eq!(
            query_args("p").unwrap(),
            ["/query", "/tn", "p", "/v", "/fo", "list"]
        );
        assert_eq!(
            query_csv_args("p").unwrap(),
            ["/query", "/tn", "p", "/fo", "csv", "/nh"]
        );
        for bad in ["../x", "a/b", "a\\b", "..", "", "a:b", "x\ny"] {
            assert!(create_args(bad, xml).is_err(), "name {bad:?} accepted");
            assert!(run_args(bad).is_err());
            assert!(delete_args(bad).is_err());
        }
        assert!(create_args("my task", xml).is_ok(), "spaces are legal");
    }

    #[test]
    fn csv_status_parses_last_field() {
        assert_eq!(
            csv_status("\"my task\",\"9/10/2026 1:00:00 AM\",\"Ready\"\n"),
            Some("Ready".into())
        );
        assert_eq!(csv_status(""), None);
        assert_eq!(csv_status("   \n"), None);
    }

    #[test]
    fn install_enable_uninstall_flow() {
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            WindowsDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        let xml = deploy.install(&spec()).unwrap();
        assert!(xml.is_file());
        assert!(deploy.wrapper_path("peregrin").unwrap().is_file());
        deploy.enable("peregrin").unwrap();
        deploy.disable("peregrin").unwrap();
        deploy.start("peregrin").unwrap();
        deploy.stop("peregrin").unwrap();
        deploy.restart("peregrin").unwrap();
        deploy.status("peregrin").unwrap();
        deploy.logs_with(&["peregrin".into()], 50, true).unwrap();
        assert!(deploy.logs_with(&[], 50, false).is_err());
        deploy.uninstall("peregrin").unwrap();
        assert!(!deploy.xml_path("peregrin").unwrap().exists());
        assert!(!deploy.wrapper_path("peregrin").unwrap().exists());
        deploy.daemon_reload().unwrap();
        assert_eq!(deploy.task_dir(), dir.path());
    }

    #[test]
    fn rejects_bad_names_and_fields() {
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            WindowsDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        for bad in [
            "../escape",
            "a/b",
            "a\\b",
            "..",
            ".",
            "",
            "a:b",
            " lead",
            "trail ",
        ] {
            assert!(deploy
                .install(&ServiceSpec {
                    name: bad.into(),
                    ..spec()
                })
                .is_err());
            assert!(deploy.wrapper_path(bad).is_err());
            assert!(deploy.enable(bad).is_err());
            assert!(deploy.uninstall(bad).is_err());
        }
        for evil in [
            ServiceSpec {
                description: "d\n<evil>".into(),
                ..spec()
            },
            ServiceSpec {
                exec: vec!["prog".into(), "arg\nset X=1".into()],
                ..spec()
            },
            ServiceSpec {
                env: vec![("A\nB".into(), "1".into())],
                ..spec()
            },
            ServiceSpec {
                env: vec![("A".into(), "1\r\nset X=1".into())],
                ..spec()
            },
            ServiceSpec {
                env: vec![("A".into(), "has \"quote\"".into())],
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
                exec: vec![],
                ..spec()
            },
        ] {
            assert!(deploy.install(&evil).is_err());
        }
    }

    struct FailRunner;
    impl Runner for FailRunner {
        fn run(&self, _program: &str, _args: &[&str]) -> Result<(), Error> {
            Err(Error::Failed("boom".into()))
        }
    }

    #[test]
    fn runner_errors_propagate() {
        let dir = tempfile::tempdir().unwrap();
        let fail = WindowsDeploy::with_default(dir.path().to_path_buf(), Box::new(FailRunner));
        assert!(fail.enable("p").is_err());
        assert!(fail.install(&spec()).is_err());
        // uninstall is best-effort on the schtasks side but still
        // removes files
        fail.uninstall("peregrin").unwrap();
    }

    #[test]
    fn default_constructor_and_unavailable_runner() {
        let deploy = WindowsDeploy::new();
        assert!(deploy.task_dir().to_str().is_some());
        #[cfg(not(windows))]
        assert!(deploy.enable("nonexistent-task-xyz").is_err());
        let _ = WindowsDeploy::default();
    }

    #[test]
    fn expand_home_in_exec_and_workdir() {
        let home = std::env::var("HOME").unwrap_or_default();
        let spec = ServiceSpec {
            exec: vec!["~/bin/tool".into(), "serve".into()],
            workdir: Some("~/srv".into()),
            ..spec()
        };
        let text = render_wrapper(&spec).unwrap();
        assert!(text.contains(&format!("cd /d \"{home}/srv\"")));
        assert!(text.contains(&format!("{home}/bin/tool serve")));
        let xml = render_task_xml(&spec, Path::new("x.cmd"));
        assert!(xml.contains(&format!("<WorkingDirectory>{home}/srv</WorkingDirectory>")));
    }
}
