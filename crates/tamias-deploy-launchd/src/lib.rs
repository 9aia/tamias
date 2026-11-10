#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-deploy-launchd — the `deploy` port's macOS adapter: manage a
//! **per-user launchd agent** — a plist under
//! `~/Library/LaunchAgents` driven by `launchctl`. No admin needed:
//! agents load into the caller's `gui/<uid>` domain.
//!
//! A [`ServiceSpec`] renders to artifacts under the agent dir
//! (`$HOME/Library/LaunchAgents` by default):
//!
//! - `<label>.plist` — the agent definition. `ProgramArguments` takes
//!   the `exec` argv **directly** (no wrapper needed for argv/env/
//!   workdir): `env` → `EnvironmentVariables`, `workdir` →
//!   `WorkingDirectory`, `restart`/`restart_sec` → `KeepAlive` +
//!   `ThrottleInterval`, `RunAtLoad` always true (the plist is the
//!   "enabled" artifact — `install` writes it, `enable` bootstraps it).
//!   `StandardOutPath`/`StandardErrorPath` point at log files next to
//!   the plist — launchd captures stdout/stderr for free, and the
//!   `logs` verb tails them (no journald equivalent needed).
//! - `<label>.sh` — a tiny POSIX-sh wrapper, written **only when
//!   `env_file` is set**. launchd has no env-file mechanism, so the
//!   wrapper sources the file (`set -a; . <file>; set +a`, guarded by
//!   `[ -f … ]` so a missing file stays non-fatal — systemd's
//!   `EnvironmentFile=-` semantics) and then `export`s the inline
//!   `env` entries so inline still wins. `ProgramArguments` then runs
//!   `/bin/sh <label>.sh` instead of the raw argv. Without `env_file`
//!   no wrapper is generated and `env` rides the plist dict.
//! - `<label>.stdout.log` / `<label>.stderr.log` — pre-created empty so
//!   `tail` (and launchd's own redirection) always have a target.
//!
//! The label is `com.9aia.<name>` where `<name>` is the spec's unit
//! base name (`<provider>-<service>`), sanitized to launchd's
//! conventional charset — see [`label_for`].
//!
//! ## Field mapping vs. the systemd backend
//!
//! | `ServiceSpec` | systemd | this backend |
//! | --- | --- | --- |
//! | `name` | `<name>.service` | launchd label `com.9aia.<name>` |
//! | `description` | `Description=` | **dropped** — plists have no description key (it stays in the store record) |
//! | `exec` | `ExecStart=` argv | `ProgramArguments` argv directly (or the `.sh` wrapper's `exec` line) |
//! | `env` | `Environment=` | `EnvironmentVariables` dict — or `export` lines in the wrapper when `env_file` is set |
//! | `env_file` | `EnvironmentFile=-` | sourced by the generated `.sh` wrapper — POSIX sh syntax, not systemd's `K=V` parser |
//! | `workdir` | `WorkingDirectory=` | `WorkingDirectory` plist key |
//! | `restart`/`restart_sec` | `Restart=`/`RestartSec=` | `KeepAlive` + `ThrottleInterval` — see gaps |
//! | `after` | `After=`/`Wants=` | **ignored** — launchd has no unit ordering |
//!
//! ## Honest gaps
//!
//! - **Restart vocabulary is approximate.** systemd's
//!   `on-failure`/`always`/`no` map to `KeepAlive`
//!   `{"SuccessfulExit": false}` / `true` / absent. `SuccessfulExit=
//!   false` restarts on non-zero exit *and* on crash/signal, but does
//!   not cover every systemd `on-failure` nuance (e.g. a clean exit
//!   during startup). `always` → `KeepAlive=true` restarts even after
//!   a clean `exit(0)` — matching systemd `always`, but note launchd
//!   throttles respawns (`ThrottleInterval`, default 10s). Any other
//!   `restart` value (`on-success`, `on-abnormal`, …) degrades to
//!   on-failure semantics. `restart_sec` maps to `ThrottleInterval`
//!   (integer seconds).
//! - **`env_file` is sourced by `/bin/sh`**, not parsed by the OS:
//!   `K=V`, `export K=V`, quoting, and `#` comments all work (richer
//!   than systemd's parser in some ways), but shell syntax is live —
//!   `$(…)`, backticks, and `$VAR` expand. Treat env files as trusted
//!   local data, same as a shell profile.
//! - **`description` and `after` are dropped** — plists carry no
//!   description field and launchd has no dependency graph.
//! - **`disable` persists via `launchctl disable`** (the per-domain
//!   override db), `enable` clears it — closer to systemd's
//!   enable/disable symlink semantics than a bare bootout. Re-running
//!   `enable` on a loaded agent bootouts + bootstraps, i.e. it
//!   *restarts* the service (launchctl has no enable-without-reload).
//! - **Default working directory.** With no `workdir`, launchd runs
//!   the job with cwd `/` — set `workdir` in the manifest if the
//!   payload expects `~`.
//! - Real `launchctl` execution is `#[cfg(target_os = "macos")]`-gated
//!   ([`LaunchctlRunner`] only exists there); everywhere else
//!   [`LaunchdDeploy::new`] installs an unavailable runner so verbs
//!   fail loudly instead of shelling out. Tests inject a [`Runner`]
//!   recorder on any platform; arg-builder tests are `cfg(unix)`
//!   (they resolve the gui uid via `$UID`/`id -u`).

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;

use tamias_core::Error;
use tamias_deploy::{expand_home, ServiceSpec};

/// The seam that runs host commands (`launchctl`, `tail`, `id`) —
/// injected for tests.
pub trait Runner: Send + Sync {
    /// Run `program args…`, erroring on a non-zero exit.
    ///
    /// # Errors
    /// Spawn/exit failures.
    fn run(&self, program: &str, args: &[&str]) -> Result<(), Error>;
}

/// The real runner — shells out to `launchctl`/`tail`. Only exists on
/// macOS: off-macOS there is no `launchctl` to run, and
/// [`LaunchdDeploy::new`] never installs this runner there.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Default)]
pub struct LaunchctlRunner;

#[cfg(target_os = "macos")]
impl Runner for LaunchctlRunner {
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

/// Off-macOS stand-in for [`LaunchctlRunner`] — every verb errors
/// instead of exec'ing a binary that doesn't exist. Only installed by
/// [`LaunchdDeploy::new`] on non-macOS; tests inject their own
/// [`Runner`].
#[cfg(not(target_os = "macos"))]
struct UnavailableRunner;

#[cfg(not(target_os = "macos"))]
impl Runner for UnavailableRunner {
    fn run(&self, program: &str, _args: &[&str]) -> Result<(), Error> {
        Err(Error::Failed(format!(
            "{program} is only available on macOS"
        )))
    }
}

/// The macOS deployer — an agent dir (plist + wrapper + logs) plus a
/// [`Runner`].
pub struct LaunchdDeploy {
    agent_dir: PathBuf,
    runner: Box<dyn Runner>,
}

impl LaunchdDeploy {
    /// The default: `~/Library/LaunchAgents` (falling back through
    /// `HOME` to `./.tamias/LaunchAgents`) + the platform runner —
    /// [`LaunchctlRunner`] on macOS, an unavailable runner elsewhere.
    #[must_use]
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        let runner: Box<dyn Runner> = Box::new(LaunchctlRunner);
        #[cfg(not(target_os = "macos"))]
        let runner: Box<dyn Runner> = Box::new(UnavailableRunner);
        Self::with_default(agent_dir(), runner)
    }

    /// An explicit agent dir + runner (tests inject a tempdir +
    /// recorder).
    #[must_use]
    pub fn with_default(agent_dir: PathBuf, runner: Box<dyn Runner>) -> Self {
        Self { agent_dir, runner }
    }

    /// The agent dir.
    #[must_use]
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// The launchd label for a unit base name — `com.9aia.<name>`
    /// sanitized (see [`label_for`]).
    ///
    /// # Errors
    /// [`Error::Failed`] when `name` is not a valid unit base name.
    pub fn label(&self, name: &str) -> Result<String, Error> {
        label_for(name)
    }

    /// The plist path for `name` — `<agent_dir>/<label>.plist`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name.
    pub fn plist_path(&self, name: &str) -> Result<PathBuf, Error> {
        Ok(self.agent_dir.join(format!("{}.plist", label_for(name)?)))
    }

    /// The env-file wrapper script path for `name` —
    /// `<agent_dir>/<label>.sh` (only written when the spec sets
    /// `env_file`).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name.
    pub fn wrapper_path(&self, name: &str) -> Result<PathBuf, Error> {
        Ok(self.agent_dir.join(format!("{}.sh", label_for(name)?)))
    }

    /// The stdout log path for `name` — `<agent_dir>/<label>.stdout.log`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name.
    pub fn stdout_path(&self, name: &str) -> Result<PathBuf, Error> {
        Ok(self
            .agent_dir
            .join(format!("{}.stdout.log", label_for(name)?)))
    }

    /// The stderr log path for `name` — `<agent_dir>/<label>.stderr.log`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name.
    pub fn stderr_path(&self, name: &str) -> Result<PathBuf, Error> {
        Ok(self
            .agent_dir
            .join(format!("{}.stderr.log", label_for(name)?)))
    }

    /// Write the plist (+ wrapper script and empty log files when
    /// `env_file` is set / always for logs). Does **not** load the
    /// agent — `enable` bootstraps it (systemd's write-unit vs
    /// `enable --now` split).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name/spec field, an empty
    /// `exec` argv, or a write failure. On non-macOS the default
    /// runner is never reached — install is pure file rendering, so
    /// it succeeds anywhere (verbs are what fail off-macOS).
    pub fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, Error> {
        check_name(&spec.name)?;
        check_fields(spec)?;
        std::fs::create_dir_all(&self.agent_dir)
            .map_err(Error::io(format!("create {}", self.agent_dir.display())))?;
        let label = label_for(&spec.name)?;
        let needs_wrapper = spec.env_file.is_some();
        let wrapper = needs_wrapper
            .then(|| self.wrapper_path(&spec.name))
            .transpose()?;
        if let Some(wrapper) = &wrapper {
            std::fs::write(wrapper, render_wrapper(spec)?)
                .map_err(Error::io(format!("write {}", wrapper.display())))?;
            make_executable(wrapper)?;
        }
        // Pre-create the log files so `tail` and launchd's redirection
        // always have a target.
        for path in [self.stdout_path(&spec.name)?, self.stderr_path(&spec.name)?] {
            if !path.exists() {
                std::fs::write(&path, b"")
                    .map_err(Error::io(format!("write {}", path.display())))?;
            }
        }
        let plist = render_plist(
            spec,
            &label,
            wrapper.as_deref(),
            &self.stdout_path(&spec.name)?,
            &self.stderr_path(&spec.name)?,
        )?;
        let path = self.plist_path(&spec.name)?;
        std::fs::write(&path, plist).map_err(Error::io(format!("write {}", path.display())))?;
        Ok(path)
    }

    /// No-op kept for parity with the other backends: launchd has no
    /// daemon-reload — `bootstrap` reads the plist at load time, and
    /// `enable` re-bootstraps after edits.
    ///
    /// # Errors
    /// Never — always `Ok`.
    pub fn daemon_reload(&self) -> Result<(), Error> {
        Ok(())
    }

    /// `launchctl enable <target>` (clears a persisted disabled
    /// override, best-effort), `launchctl bootout <target>`
    /// (best-effort — unloads a loaded agent so `bootstrap` below
    /// can't fail "already loaded"), then `launchctl bootstrap
    /// gui/<uid> <plist>`.
    ///
    /// Re-enabling an already-loaded agent **restarts** it — launchd
    /// has no enable-without-reload (documented gap).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name or a missing plist path;
    /// propagates the `bootstrap` failure.
    pub fn enable(&self, name: &str) -> Result<(), Error> {
        let _ = self.launchctl(&enable_args(name)?);
        let _ = self.launchctl(&bootout_args(name)?);
        self.launchctl(&bootstrap_args(name, &self.plist_path(name)?)?)
    }

    /// `launchctl bootout gui/<uid>/<label>` (best-effort — a service
    /// that was never bootstrapped must not fail disable) then
    /// `launchctl disable gui/<uid>/<label>` — the persistent
    /// per-domain override, so the agent stays off across logins
    /// (systemd `disable` parity).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the
    /// `launchctl disable` failure.
    pub fn disable(&self, name: &str) -> Result<(), Error> {
        let _ = self.launchctl(&bootout_args(name)?);
        self.launchctl(&disable_args(name)?)
    }

    /// `launchctl kickstart gui/<uid>/<label>` — start the agent.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the runner
    /// failure.
    pub fn start(&self, name: &str) -> Result<(), Error> {
        self.launchctl(&kickstart_args(name, false)?)
    }

    /// `launchctl kill TERM gui/<uid>/<label>`.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the runner
    /// failure.
    pub fn stop(&self, name: &str) -> Result<(), Error> {
        self.launchctl(&kill_args(name)?)
    }

    /// `launchctl kickstart -k gui/<uid>/<label>` — kill the running
    /// instance (if any) and start fresh.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the runner
    /// failure.
    pub fn restart(&self, name: &str) -> Result<(), Error> {
        self.launchctl(&kickstart_args(name, true)?)
    }

    /// `launchctl print gui/<uid>/<label>` — the agent's full state
    /// dump; exits non-zero when the label isn't loaded.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name or uid resolution;
    /// propagates the runner failure.
    pub fn status(&self, name: &str) -> Result<(), Error> {
        self.launchctl(&print_args(name)?)
    }

    /// Best-effort status for one agent — same as [`Self::status`].
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the runner
    /// failure.
    pub fn logs(&self, name: &str) -> Result<(), Error> {
        self.logs_with(&[name.to_string()], 200, false)
    }

    /// `tail -n <lines> [-f] <stdout.log> <stderr.log>…` over one or
    /// more agents — launchd writes the log files itself, so unlike
    /// systemd there is no journal to query, just files to tail.
    /// `follow` adds `-f` (blocks like `journalctl -f`).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid/empty name list or a
    /// non-existent log file; propagates the runner failure.
    pub fn logs_with(&self, names: &[String], lines: u32, follow: bool) -> Result<(), Error> {
        let args = self.tail_args(names, lines, follow)?;
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run("tail", &refs)
    }

    /// The `tail` argv [`Self::logs_with`] runs — also what interactive
    /// surfaces spawn directly for a streaming log view.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid/empty name list.
    pub fn tail_args(
        &self,
        names: &[String],
        lines: u32,
        follow: bool,
    ) -> Result<Vec<String>, Error> {
        if names.is_empty() {
            return Err(Error::Failed("logs: no agents given".into()));
        }
        let mut args = vec!["-n".into(), lines.to_string()];
        if follow {
            args.push("-f".into());
        }
        for name in names {
            args.push(self.stdout_path(name)?.to_string_lossy().into_owned());
            args.push(self.stderr_path(name)?.to_string_lossy().into_owned());
        }
        Ok(args)
    }

    /// Best-effort `bootout` + clear-any-disabled-override + remove
    /// the plist, wrapper, and log files. A missing agent must not
    /// fail uninstall.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the file
    /// removal failure.
    pub fn uninstall(&self, name: &str) -> Result<(), Error> {
        let _ = self.launchctl(&bootout_args(name)?);
        // Clear a persisted `launchctl disable` override so a future
        // package reusing this label isn't born disabled.
        let _ = self.launchctl(&enable_args(name)?);
        for path in [
            self.plist_path(name)?,
            self.wrapper_path(name)?,
            self.stdout_path(name)?,
            self.stderr_path(name)?,
        ] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(Error::io(format!("remove {}", path.display())))?;
            }
        }
        Ok(())
    }

    fn launchctl(&self, args: &[String]) -> Result<(), Error> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run("launchctl", &refs)
    }
}

impl Default for LaunchdDeploy {
    fn default() -> Self {
        Self::new()
    }
}

/// The launchd label for a unit base name: `com.9aia.<name>` with the
/// name sanitized to launchd's conventional charset (`[A-Za-z0-9._-]`
/// — `@`, the only non-conventional char the unit-name grammar allows,
/// becomes `-`). Sanitization is deterministic but **not injective**:
/// `a@b` and `a-b` collide on `com.9aia.a-b` — unit names are
/// provider-derived and don't contain `@` in practice.
///
/// # Errors
/// [`Error::Failed`] when `name` is not a valid unit base name (see
/// [`check_name`]).
pub fn label_for(name: &str) -> Result<String, Error> {
    check_name(name)?;
    let mut label = String::from("com.9aia.");
    for c in name.chars() {
        label.push(if c == '@' { '-' } else { c });
    }
    Ok(label)
}

/// The `gui/<uid>` launchd domain for the current user — `$UID` when
/// set and numeric, else `id -u` (unix only). Needed for every
/// service-target arg (`gui/<uid>/<label>`).
///
/// # Errors
/// [`Error::Failed`] when no uid can be determined (non-unix without
/// `$UID` — unreachable in practice: launchd verbs only run on
/// macOS).
pub fn gui_domain() -> Result<String, Error> {
    if let Ok(uid) = std::env::var("UID") {
        if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
            return Ok(format!("gui/{uid}"));
        }
    }
    #[cfg(unix)]
    {
        let out = Command::new("id")
            .arg("-u")
            .output()
            .map_err(Error::io("id -u"))?;
        let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
            return Ok(format!("gui/{uid}"));
        }
    }
    Err(Error::Failed(
        "cannot determine user id for the gui domain (set $UID)".into(),
    ))
}

/// The full launchd service target `gui/<uid>/<label>` for a unit
/// base name — what `print`/`kickstart`/`bootout`/`kill` take.
///
/// # Errors
/// [`Error::Failed`] on an invalid name or uid resolution failure.
pub fn service_target(name: &str) -> Result<String, Error> {
    Ok(format!("{}/{}", gui_domain()?, label_for(name)?))
}

/// `launchctl bootstrap gui/<uid> <plist>` — load the agent (and, with
/// `RunAtLoad`, start it).
///
/// # Errors
/// [`Error::Failed`] on uid resolution failure.
pub fn bootstrap_args(_name: &str, plist: &Path) -> Result<Vec<String>, Error> {
    // `_name` kept for symmetry with the other builders — the plist
    // path already encodes the label.
    Ok(vec![
        "bootstrap".into(),
        gui_domain()?,
        plist.to_string_lossy().into_owned(),
    ])
}

/// `launchctl bootout gui/<uid>/<label>` — unload the agent.
///
/// # Errors
/// [`Error::Failed`] on an invalid name or uid resolution failure.
pub fn bootout_args(name: &str) -> Result<Vec<String>, Error> {
    Ok(vec!["bootout".into(), service_target(name)?])
}

/// `launchctl enable gui/<uid>/<label>` — clear the persisted disabled
/// override.
///
/// # Errors
/// [`Error::Failed`] on an invalid name or uid resolution failure.
pub fn enable_args(name: &str) -> Result<Vec<String>, Error> {
    Ok(vec!["enable".into(), service_target(name)?])
}

/// `launchctl disable gui/<uid>/<label>` — persist the disabled
/// override (survives re-login, unlike a bare bootout).
///
/// # Errors
/// [`Error::Failed`] on an invalid name or uid resolution failure.
pub fn disable_args(name: &str) -> Result<Vec<String>, Error> {
    Ok(vec!["disable".into(), service_target(name)?])
}

/// `launchctl kickstart [-k] gui/<uid>/<label>` — start the agent;
/// `-k` kills a running instance first (restart).
///
/// # Errors
/// [`Error::Failed`] on an invalid name or uid resolution failure.
pub fn kickstart_args(name: &str, kill: bool) -> Result<Vec<String>, Error> {
    let mut args = vec!["kickstart".to_string()];
    if kill {
        args.push("-k".into());
    }
    args.push(service_target(name)?);
    Ok(args)
}

/// `launchctl kill TERM gui/<uid>/<label>` — SIGTERM the agent.
///
/// # Errors
/// [`Error::Failed`] on an invalid name or uid resolution failure.
pub fn kill_args(name: &str) -> Result<Vec<String>, Error> {
    Ok(vec!["kill".into(), "TERM".into(), service_target(name)?])
}

/// `launchctl print gui/<uid>/<label>` — state dump.
///
/// # Errors
/// [`Error::Failed`] on an invalid name or uid resolution failure.
pub fn print_args(name: &str) -> Result<Vec<String>, Error> {
    Ok(vec!["print".into(), service_target(name)?])
}

/// A valid unit base name: `[A-Za-z0-9_.@-]`, no separators, no `..`,
/// not `.`/`..` — same grammar the systemd backend enforces (the name
/// becomes both a filename component via the label and a launchctl
/// argument).
fn check_name(name: &str) -> Result<(), Error> {
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

/// Reject spec fields that could break out of the plist XML or the
/// generated shell wrapper — a `\n`/`\r`/NUL lands as extra plist
/// content or extra shell lines.
fn check_fields(spec: &ServiceSpec) -> Result<(), Error> {
    if spec.exec.is_empty() {
        return Err(Error::Failed(format!(
            "agent '{}' has an empty exec argv",
            spec.name
        )));
    }
    let bad = |what: &str, v: &str| {
        if v.contains(['\n', '\r', '\0']) {
            Some(Error::Failed(format!(
                "agent {what} contains a line break or NUL"
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
        // lands in the plist/wrapper.
        if let Some(e) = bad(what, &expand_home(v)) {
            return Err(e);
        }
    }
    if let Some(e) = bad("restart", spec.restart.as_deref().unwrap_or("")) {
        return Err(e);
    }
    for (k, v) in &spec.env {
        if let Some(e) = bad("env key", k).or_else(|| bad("env value", v)) {
            return Err(e);
        }
    }
    // `spec.after` is intentionally not validated: no launchd backend
    // renders it (documented gap — no ordering primitive).
    Ok(())
}

/// Quote one word for the generated POSIX-sh wrapper — bare when the
/// arg is shell-safe, else single-quoted with the `'\''` idiom. Any
/// byte sequence without a line break/NUL is representable (both are
/// rejected upstream by [`check_fields`]).
fn sh_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '%' | '+' | ',')
        })
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// `chmod 0755` on unix so `/bin/sh <wrapper>` can exec it — launchd
/// invokes the script via `/bin/sh` explicitly so the bit is
/// belt-and-braces (and lets users run it by hand). No-op elsewhere.
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(Error::io(format!("stat {}", path.display())))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(Error::io(format!("chmod {}", path.display())))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), Error> {
    Ok(())
}

/// Render the env-file wrapper — a tiny POSIX-sh script:
///
/// ```sh
/// #!/bin/sh
/// # tamias launchd agent '<name>' — generated; do not edit
/// if [ -f '<env_file>' ]; then
///     set -a
///     . '<env_file>'
///     set +a
/// fi
/// export K='V'   # inline env, after env_file — inline wins (systemd order)
/// exec '<argv…>'
/// ```
///
/// `set -a` exports everything the sourced file assigns; the `[ -f ]`
/// guard keeps a missing env_file non-fatal (`EnvironmentFile=-`
/// parity). `workdir` is *not* `cd`'d here — the plist's
/// `WorkingDirectory` already sets the job's cwd.
///
/// # Errors
/// [`Error::Failed`] on an empty `exec` argv.
pub fn render_wrapper(spec: &ServiceSpec) -> Result<String, Error> {
    let mut sh = String::new();
    sh.push_str("#!/bin/sh\n");
    sh.push_str(&format!(
        "# tamias launchd agent '{}' — generated; do not edit\n",
        spec.name.replace(['\r', '\n'], " ")
    ));
    if !spec.description.is_empty() {
        sh.push_str(&format!(
            "# {}\n",
            spec.description.replace(['\r', '\n'], " ")
        ));
    }
    if let Some(env_file) = &spec.env_file {
        let path = sh_quote(&expand_home(env_file));
        sh.push_str(&format!("if [ -f {path} ]; then\n"));
        sh.push_str("    set -a\n");
        sh.push_str(&format!("    . {path}\n"));
        sh.push_str("    set +a\n");
        sh.push_str("fi\n");
    }
    for (k, v) in &spec.env {
        // Keys must be shell identifiers to ride `export` — anything
        // else is rejected here (plist mode would tolerate it, but a
        // wrapper can't express it).
        if !k
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(Error::Failed(format!(
                "env key '{k}' is not a shell identifier (required when env_file is set)"
            )));
        }
        sh.push_str(&format!("export {k}={}\n", sh_quote(v)));
    }
    let mut argv = spec.exec.iter();
    let Some(prog) = argv.next() else {
        return Err(Error::Failed(format!(
            "agent '{}' has an empty exec argv",
            spec.name
        )));
    };
    let mut line = sh_quote(&expand_home(prog));
    for arg in argv {
        line.push(' ');
        line.push_str(&sh_quote(&expand_home(arg)));
    }
    sh.push_str(&format!("exec {line}\n"));
    Ok(sh)
}

/// XML-escape element text (`&`, `<`, `>` — quotes are legal in
/// content and stay literal).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Whether the spec asks for restart — mirrors systemd's default
/// (`on-failure` when unset): `"no"`/`"never"`/`"false"`/`"off"`
/// disable, `"always"`/`"true"` map to `KeepAlive=true`, everything
/// else (including `"on-failure"` and unknown values) degrades to
/// `KeepAlive={"SuccessfulExit": false}`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Restart {
    /// No `KeepAlive` key.
    Off,
    /// `KeepAlive` = `true` — restart regardless of exit status.
    Always,
    /// `KeepAlive` = `{SuccessfulExit: false}` — restart on failure.
    OnFailure,
}

fn restart_mode(spec: &ServiceSpec) -> Restart {
    match spec.restart.as_deref() {
        Some("no") | Some("never") | Some("false") | Some("off") => Restart::Off,
        Some("always") | Some("true") => Restart::Always,
        _ => Restart::OnFailure,
    }
}

/// Render the launchd agent plist.
///
/// - `Label` — the sanitized `com.9aia.<name>` label.
/// - `ProgramArguments` — the `exec` argv directly, or
///   `[/bin/sh, <wrapper>]` when `env_file` forced a wrapper (then the
///   wrapper also owns `env`; the plist's `EnvironmentVariables` is
///   omitted so env_file→inline ordering stays systemd's).
/// - `EnvironmentVariables` — the `env` dict (only without `env_file`).
/// - `WorkingDirectory`, `RunAtLoad`, `KeepAlive`,
///   `ThrottleInterval`, `StandardOutPath`/`StandardErrorPath`.
///
/// `description` and `after` have no plist analog and are dropped
/// (documented gaps).
///
/// # Errors
/// [`Error::Failed`] on an empty `exec` argv or an env key that isn't
/// a shell identifier while a wrapper is rendered.
pub fn render_plist(
    spec: &ServiceSpec,
    label: &str,
    wrapper: Option<&Path>,
    stdout: &Path,
    stderr: &Path,
) -> Result<String, Error> {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    xml.push_str("<plist version=\"1.0\">\n<dict>\n");
    xml.push_str(&format!(
        "  <key>Label</key>\n  <string>{}</string>\n",
        xml_escape(label)
    ));
    xml.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    let argv: Vec<String> = match wrapper {
        Some(w) => vec!["/bin/sh".to_string(), w.to_string_lossy().into_owned()],
        None => spec.exec.iter().map(|a| expand_home(a)).collect(),
    };
    if argv.is_empty() {
        return Err(Error::Failed(format!(
            "agent '{}' has an empty exec argv",
            spec.name
        )));
    }
    for arg in &argv {
        xml.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }
    xml.push_str("  </array>\n");
    // env rides the plist dict only when there is no env_file wrapper —
    // with a wrapper, the script sources env_file then exports inline
    // env, preserving systemd's env_file→env ordering.
    if wrapper.is_none() && !spec.env.is_empty() {
        xml.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (k, v) in &spec.env {
            xml.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        xml.push_str("  </dict>\n");
    }
    if let Some(workdir) = &spec.workdir {
        xml.push_str(&format!(
            "  <key>WorkingDirectory</key>\n  <string>{}</string>\n",
            xml_escape(&expand_home(workdir))
        ));
    }
    xml.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    match restart_mode(spec) {
        Restart::Off => {}
        Restart::Always => {
            xml.push_str("  <key>KeepAlive</key>\n  <true/>\n");
        }
        Restart::OnFailure => {
            xml.push_str("  <key>KeepAlive</key>\n  <dict>\n");
            xml.push_str("    <key>SuccessfulExit</key>\n    <false/>\n");
            xml.push_str("  </dict>\n");
        }
    }
    if restart_mode(spec) != Restart::Off {
        // launchd's default throttle is 10s; restart_sec overrides.
        xml.push_str(&format!(
            "  <key>ThrottleInterval</key>\n  <integer>{}</integer>\n",
            spec.restart_sec.unwrap_or(10)
        ));
    }
    xml.push_str(&format!(
        "  <key>StandardOutPath</key>\n  <string>{}</string>\n",
        xml_escape(&stdout.to_string_lossy())
    ));
    xml.push_str(&format!(
        "  <key>StandardErrorPath</key>\n  <string>{}</string>\n",
        xml_escape(&stderr.to_string_lossy())
    ));
    xml.push_str("</dict>\n</plist>\n");
    Ok(xml)
}

/// `$HOME/Library/LaunchAgents` → `./.tamias/LaunchAgents`.
fn agent_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library/LaunchAgents"))
        .unwrap_or_else(|| PathBuf::from(".tamias/LaunchAgents"))
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
            name: "peregrin-node".into(),
            description: "peregrin node".into(),
            exec: vec!["/opt/peregrin/bin/peregrin".into(), "serve".into()],
            env: vec![("PEREGRIN_HOME".into(), "/data/peregrin".into())],
            env_file: None,
            restart: None,
            restart_sec: None,
            workdir: None,
            after: vec![],
        }
    }

    #[test]
    fn label_is_reverse_dns_and_sanitized() {
        assert_eq!(
            label_for("peregrin-node").unwrap(),
            "com.9aia.peregrin-node"
        );
        assert_eq!(label_for("ci-gate@web").unwrap(), "com.9aia.ci-gate-web");
        for bad in ["", ".", "..", "a..b", "a/b", "a b", "a\nb"] {
            assert!(label_for(bad).is_err(), "name {bad:?} accepted");
        }
        assert!(label_for("-").is_ok());
        assert!(label_for("my_svc.v2").is_ok());
    }

    #[test]
    fn renders_plist_direct_argv_env_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            LaunchdDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        let plist = deploy.install(&spec()).unwrap();
        assert!(plist.is_file());
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(text.contains("<key>Label</key>\n  <string>com.9aia.peregrin-node</string>"));
        assert!(text.contains("<string>/opt/peregrin/bin/peregrin</string>"));
        assert!(text.contains("<string>serve</string>"));
        assert!(text.contains("<key>PEREGRIN_HOME</key>"));
        assert!(text.contains("<string>/data/peregrin</string>"));
        assert!(text.contains("<key>RunAtLoad</key>\n  <true/>"));
        // default restart = on-failure parity
        assert!(text.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(text.contains("<key>ThrottleInterval</key>\n  <integer>10</integer>"));
        assert!(text.contains("com.9aia.peregrin-node.stdout.log"));
        assert!(text.contains("com.9aia.peregrin-node.stderr.log"));
        assert!(!deploy.wrapper_path("peregrin-node").unwrap().exists());
        // log files pre-created for tail
        assert!(deploy.stdout_path("peregrin-node").unwrap().is_file());
        assert!(deploy.stderr_path("peregrin-node").unwrap().is_file());
    }

    #[test]
    fn plist_restart_mapping() {
        let no = ServiceSpec {
            restart: Some("no".into()),
            ..spec()
        };
        let xml = render_plist(&no, "com.9aia.x", None, Path::new("o"), Path::new("e")).unwrap();
        assert!(!xml.contains("KeepAlive"));
        assert!(!xml.contains("ThrottleInterval"));

        let always = ServiceSpec {
            restart: Some("always".into()),
            restart_sec: Some(30),
            ..spec()
        };
        let xml =
            render_plist(&always, "com.9aia.x", None, Path::new("o"), Path::new("e")).unwrap();
        assert!(xml.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(xml.contains("<integer>30</integer>"));

        let onfail = ServiceSpec {
            restart: Some("on-failure".into()),
            restart_sec: Some(3),
            ..spec()
        };
        let xml =
            render_plist(&onfail, "com.9aia.x", None, Path::new("o"), Path::new("e")).unwrap();
        assert!(xml.contains("SuccessfulExit"));
        assert!(xml.contains("<integer>3</integer>"));
    }

    #[test]
    fn env_file_renders_wrapper_and_omits_plist_env() {
        let home = std::env::var("HOME").unwrap_or_default();
        let spec = ServiceSpec {
            env_file: Some("~/.config/peregrin/env".into()),
            env: vec![("INLINE".into(), "wins".into())],
            workdir: Some("~/srv".into()),
            exec: vec!["~/bin/tool".into(), "two words".into(), "it's".into()],
            ..spec()
        };
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            LaunchdDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        deploy.install(&spec).unwrap();
        let wrapper = deploy.wrapper_path("peregrin-node").unwrap();
        assert!(wrapper.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let sh = std::fs::read_to_string(&wrapper).unwrap();
        let file_line = sh.find(&format!("{home}/.config/peregrin/env")).unwrap();
        let export_at = sh.find("export INLINE=").unwrap();
        let exec_at = sh.find("exec ").unwrap();
        assert!(
            file_line < export_at,
            "env_file must load before inline env"
        );
        assert!(export_at < exec_at);
        // safe words stay bare; anything else single-quotes ('\'' idiom)
        assert!(sh.contains("export INLINE=wins"));
        assert!(sh.contains(&format!("exec {home}/bin/tool 'two words' 'it'\\''s'")));
        let plist_text =
            std::fs::read_to_string(deploy.plist_path("peregrin-node").unwrap()).unwrap();
        assert!(plist_text.contains("<string>/bin/sh</string>"));
        assert!(plist_text.contains("peregrin-node.sh"));
        // env moved into the wrapper — no EnvironmentVariables dict
        assert!(!plist_text.contains("EnvironmentVariables"));
        assert!(plist_text.contains(&format!("<string>{home}/srv</string>")));
    }

    #[test]
    fn wrapper_rejects_non_identifier_env_keys() {
        let bad = ServiceSpec {
            env_file: Some("/tmp/e".into()),
            env: vec![("BAD-KEY".into(), "1".into())],
            ..spec()
        };
        assert!(render_wrapper(&bad).is_err());
        // same key is fine without env_file (plist dict carries it)
        let ok = ServiceSpec {
            env: vec![("BAD-KEY".into(), "1".into())],
            ..spec()
        };
        assert!(render_plist(&ok, "l", None, Path::new("o"), Path::new("e")).is_ok());
    }

    #[test]
    fn plist_escapes_xml_and_expands_home_in_exec() {
        let home = std::env::var("HOME").unwrap_or_default();
        let spec = ServiceSpec {
            description: "a <b> & \"c\"".into(),
            exec: vec!["~/bin/tool".into(), "a&b".into()],
            workdir: Some("/w & dir".into()),
            ..spec()
        };
        let xml = render_plist(
            &spec,
            "com.9aia.x",
            None,
            Path::new("/o & out"),
            Path::new("/e"),
        )
        .unwrap();
        assert!(xml.contains("<string>a&amp;b</string>"));
        assert!(xml.contains(&format!("<string>{home}/bin/tool</string>")));
        assert!(xml.contains("<string>/w &amp; dir</string>"));
        assert!(xml.contains("/o &amp; out"));
    }

    #[test]
    #[cfg(unix)]
    fn launchctl_arg_builders() {
        let plist = Path::new("/home/u/Library/LaunchAgents/com.9aia.p.plist");
        let boot = bootstrap_args("p", plist).unwrap();
        assert_eq!(boot[0], "bootstrap");
        assert!(boot[1].starts_with("gui/"));
        assert_eq!(boot[2], plist.to_string_lossy());
        let target = service_target("p").unwrap();
        assert!(target.starts_with("gui/"));
        assert!(target.ends_with("/com.9aia.p"));
        assert_eq!(
            bootout_args("p").unwrap(),
            ["bootout".to_string(), target.clone()]
        );
        assert_eq!(
            enable_args("p").unwrap(),
            ["enable".to_string(), target.clone()]
        );
        assert_eq!(
            disable_args("p").unwrap(),
            ["disable".to_string(), target.clone()]
        );
        assert_eq!(
            kickstart_args("p", true).unwrap(),
            ["kickstart".to_string(), "-k".to_string(), target.clone()]
        );
        assert_eq!(
            kickstart_args("p", false).unwrap(),
            ["kickstart".to_string(), target.clone()]
        );
        assert_eq!(
            kill_args("p").unwrap(),
            ["kill".to_string(), "TERM".to_string(), target.clone()]
        );
        assert_eq!(print_args("p").unwrap(), ["print".to_string(), target]);
        for bad in ["../x", "a/b", "a b", "..", "", "a:b", "x\ny"] {
            assert!(bootout_args(bad).is_err(), "name {bad:?} accepted");
            assert!(print_args(bad).is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn install_enable_uninstall_flow() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::default();
        let deploy = LaunchdDeploy::with_default(dir.path().to_path_buf(), Box::new(rec));
        let plist = deploy.install(&spec()).unwrap();
        assert!(plist.is_file());
        deploy.enable("peregrin-node").unwrap();
        deploy.disable("peregrin-node").unwrap();
        deploy.start("peregrin-node").unwrap();
        deploy.stop("peregrin-node").unwrap();
        deploy.restart("peregrin-node").unwrap();
        deploy.status("peregrin-node").unwrap();
        deploy
            .logs_with(&["peregrin-node".into()], 50, true)
            .unwrap();
        assert!(deploy.logs_with(&[], 50, false).is_err());
        deploy.uninstall("peregrin-node").unwrap();
        assert!(!deploy.plist_path("peregrin-node").unwrap().exists());
        assert!(!deploy.stdout_path("peregrin-node").unwrap().exists());
        deploy.daemon_reload().unwrap();
        assert_eq!(deploy.agent_dir(), dir.path());
    }

    #[test]
    fn rejects_bad_names_and_fields() {
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            LaunchdDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        for bad in ["../escape", "a/b", "..", ".", "", "a:b", "a b", "a\nb"] {
            assert!(deploy
                .install(&ServiceSpec {
                    name: bad.into(),
                    ..spec()
                })
                .is_err());
            assert!(deploy.plist_path(bad).is_err());
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
    #[cfg(unix)]
    fn runner_errors_propagate() {
        let dir = tempfile::tempdir().unwrap();
        let fail = LaunchdDeploy::with_default(dir.path().to_path_buf(), Box::new(FailRunner));
        // enable's enable/bootout steps are best-effort; bootstrap fails
        assert!(fail.enable("p").is_err());
        assert!(fail.disable("p").is_err());
        assert!(fail.start("p").is_err());
        assert!(fail.logs_with(&["p".into()], 10, false).is_err());
        // install is pure rendering — no runner involved
        fail.install(&spec()).unwrap();
        // uninstall is best-effort on the launchctl side but still
        // removes files
        fail.uninstall("peregrin-node").unwrap();
    }

    #[test]
    fn default_constructor_and_unavailable_runner() {
        let deploy = LaunchdDeploy::new();
        assert!(deploy.agent_dir().to_str().is_some());
        #[cfg(not(target_os = "macos"))]
        assert!(deploy.status("nonexistent-agent-xyz").is_err());
        let _ = LaunchdDeploy::default();
    }

    #[test]
    fn tail_args_shape() {
        let dir = tempfile::tempdir().unwrap();
        let deploy =
            LaunchdDeploy::with_default(dir.path().to_path_buf(), Box::new(Recorder::default()));
        let args = deploy.tail_args(&["a-b".into()], 25, true).unwrap();
        assert_eq!(args[0], "-n");
        assert_eq!(args[1], "25");
        assert_eq!(args[2], "-f");
        assert!(args[3].ends_with("com.9aia.a-b.stdout.log"));
        assert!(args[4].ends_with("com.9aia.a-b.stderr.log"));
        let nofollow = deploy.tail_args(&["a-b".into()], 25, false).unwrap();
        assert_eq!(nofollow.len(), 4);
        assert!(deploy.tail_args(&["bad name".into()], 1, false).is_err());
        assert!(deploy.tail_args(&[], 1, false).is_err());
    }
}
