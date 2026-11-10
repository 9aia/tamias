#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-provider — the provider lifecycle shared by `tamias` (CLI
//! verbs) and `tamias ui` (the Services pane). Everything here used to
//! live private in `tamias-cli`: resolving an installed package's
//! `provider:` block into unit/task names, host-backend state probes
//! (systemd `systemctl` / Windows `schtasks` / macOS `launchctl` via
//! [`ServiceBackend`]),
//! deploy/uninstall teardown (driver verb + declarative units +
//! shared-unit survivor re-render), the foreground `run` exec spec
//! (env_file → env → workdir, the same order the unit renders), and
//! the journal unit list `logs` tails.
//!
//! Nothing shells out to the `tamias` binary and nothing re-parses
//! CLI output: callers get typed rows/specs/reports and do their own
//! rendering (println, ratatui, serve JSON).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tamias_core::{output_with_timeout, Error, Manifest, ProviderIngress, StoreEntry};
use tamias_deploy::{expand_home, split_exec, ServiceSpec};
use tamias_deploy_launchd::LaunchdDeploy;
use tamias_deploy_systemd::{journalctl_args, LocalDeploy};
use tamias_deploy_windows::WindowsDeploy;
use tamias_store_local::Store;

/// One provider service as the Services surfaces show it — package
/// label, service name, unit base name (`<provider>-<service>`),
/// `systemctl is-enabled` state when systemd answers, and the declared
/// `exec` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceRow {
    /// Store label of the declaring package (`@scope/name@version`).
    pub package: String,
    /// `provider.services[].name`.
    pub service: String,
    /// Unit base name (no `.service` suffix).
    pub unit: String,
    /// `systemctl --user is-enabled` output; `None` when systemd can't
    /// answer (no user bus, timeout).
    pub state: Option<String>,
    /// The declared `exec` command line.
    pub exec: String,
}

/// One installed package's provider view — `None` provider means the
/// package declares no `provider:` block (only surfaced by callers
/// that asked for all packages, like `provider list --all`).
#[derive(Clone, Debug)]
pub struct ProviderPackage {
    /// Store label (`@scope/name@version`).
    pub label: String,
    /// The payload dir on disk.
    pub dir: PathBuf,
    /// The `provider:` block, when declared.
    pub provider: Option<ProviderView>,
}

/// The renderable bits of a `provider:` block.
#[derive(Clone, Debug)]
pub struct ProviderView {
    /// `provider.name`, else the scope-qualified package name — the
    /// unit-name prefix.
    pub base: String,
    /// One row per `provider.services[]` entry.
    pub services: Vec<ServiceRow>,
    /// `provider.ingress[]` — public endpoints (URLs only; hooks stay
    /// driver-side).
    pub ingress: Vec<ProviderIngress>,
    /// `provider.verbs` — advisory CLI hints.
    pub verbs: Vec<String>,
    /// `provider.health.cmd`, when declared.
    pub health: Option<String>,
}

/// What a foreground `run` needs — the resolved argv/env/workdir the
/// unit would render, minus systemd. Callers spawn it themselves
/// (`tamias run` inherits stdio; `tamias ui` pipes + streams).
#[derive(Clone, Debug)]
pub struct ExecSpec {
    /// The provider service name this spec came from.
    pub service: String,
    /// `exec` argv plus any caller-supplied trailing args.
    pub argv: Vec<String>,
    /// `env_file` lines first, inline `env` overriding — systemd's
    /// order, `~` expanded on `env_file`.
    pub env: Vec<(String, String)>,
    /// `workdir` (`~` expanded) or the payload dir.
    pub workdir: PathBuf,
}

/// A deploy/uninstall outcome split by destination stream — `lines`
/// are the normal report (stdout), `warnings` the advisory notes
/// (stderr). Both UIs print/log them verbatim.
#[derive(Clone, Debug, Default)]
pub struct DeployReport {
    /// Primary output lines, in order.
    pub lines: Vec<String>,
    /// Warning lines, in order.
    pub warnings: Vec<String>,
}

/// A `systemctl --user` lifecycle verb on a unit base name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitAction {
    /// `systemctl --user start` (or `schtasks /run` on Windows).
    Start,
    /// `systemctl --user stop` (or `schtasks /end` on Windows).
    Stop,
    /// `systemctl --user restart` (or `/end`+`/run` on Windows).
    Restart,
    /// `systemctl --user enable --now` (or `schtasks /change /enable`).
    Enable,
    /// `systemctl --user disable --now` (or `schtasks /change /disable`).
    Disable,
}

/// The per-user service backend for this host — systemd `--user`
/// units on Linux/Unix, Task Scheduler per-user tasks on Windows,
/// launchd `gui/<uid>` agents on macOS. All render the same
/// [`ServiceSpec`] from the `tamias-deploy` port; [`ServiceBackend::new`]
/// picks by `cfg!` once, so callers never branch on platform
/// themselves.
pub enum ServiceBackend {
    /// systemd user units — [`LocalDeploy`].
    Systemd(LocalDeploy),
    /// Task Scheduler tasks — [`WindowsDeploy`].
    Tasks(WindowsDeploy),
    /// launchd LaunchAgents — [`LaunchdDeploy`].
    Launchd(LaunchdDeploy),
}

impl ServiceBackend {
    /// The backend for this host — Task Scheduler on Windows, launchd
    /// on macOS, systemd everywhere else.
    #[must_use]
    pub fn new() -> Self {
        if cfg!(windows) {
            Self::Tasks(WindowsDeploy::new())
        } else if cfg!(target_os = "macos") {
            Self::Launchd(LaunchdDeploy::new())
        } else {
            Self::Systemd(LocalDeploy::new())
        }
    }

    /// Write + register `spec` — returns the rendered artifact path
    /// (unit file on systemd, task XML on Windows, agent plist on
    /// macOS).
    ///
    /// # Errors
    /// Propagates the backend's install failure.
    pub fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, Error> {
        match self {
            Self::Systemd(d) => d.install(spec),
            Self::Tasks(d) => d.install(spec),
            Self::Launchd(d) => d.install(spec),
        }
    }

    /// Best-effort reload so the host sees just-written artifacts —
    /// `systemctl --user daemon-reload` on systemd; a no-op on Windows
    /// (`schtasks /create` registers immediately).
    ///
    /// # Errors
    /// Propagates the backend's failure (never on Windows).
    pub fn daemon_reload(&self) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.daemon_reload(),
            Self::Tasks(d) => d.daemon_reload(),
            Self::Launchd(d) => d.daemon_reload(),
        }
    }

    /// Enable the service (systemd `enable --now` / `schtasks /change
    /// /enable`).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the backend
    /// failure.
    pub fn enable(&self, name: &str) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.enable(name),
            Self::Tasks(d) => d.enable(name),
            Self::Launchd(d) => d.enable(name),
        }
    }

    /// Disable the service (systemd `disable --now` / `schtasks
    /// /change /disable`).
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the backend
    /// failure.
    pub fn disable(&self, name: &str) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.disable(name),
            Self::Tasks(d) => d.disable(name),
            Self::Launchd(d) => d.disable(name),
        }
    }

    /// Start the service.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the backend
    /// failure.
    pub fn start(&self, name: &str) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.start(name),
            Self::Tasks(d) => d.start(name),
            Self::Launchd(d) => d.start(name),
        }
    }

    /// Stop the service.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the backend
    /// failure.
    pub fn stop(&self, name: &str) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.stop(name),
            Self::Tasks(d) => d.stop(name),
            Self::Launchd(d) => d.stop(name),
        }
    }

    /// Restart the service.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the backend
    /// failure.
    pub fn restart(&self, name: &str) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.restart(name),
            Self::Tasks(d) => d.restart(name),
            Self::Launchd(d) => d.restart(name),
        }
    }

    /// Service status probe.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the backend
    /// failure.
    pub fn status(&self, name: &str) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.status(name),
            Self::Tasks(d) => d.status(name),
            Self::Launchd(d) => d.status(name),
        }
    }

    /// Journal/log tail over one or more services — `journalctl` on
    /// systemd; `tail` over the agent's stdout/stderr log files on
    /// macOS launchd; best-effort `schtasks /query` snapshots on
    /// Windows (Task Scheduler keeps no per-task log; `lines`/`follow`
    /// are ignored there).
    ///
    /// # Errors
    /// [`Error::Failed`] on an empty/invalid name list; propagates the
    /// backend failure.
    pub fn logs_with(&self, names: &[String], lines: u32, follow: bool) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.logs_with(names, lines, follow),
            Self::Tasks(d) => d.logs_with(names, lines, follow),
            Self::Launchd(d) => d.logs_with(names, lines, follow),
        }
    }

    /// The `(program, argv)` a streaming surface spawns to tail logs —
    /// `journalctl` on systemd; `tail -f` over the agent's log files on
    /// macOS launchd; on Windows a `schtasks /query` snapshot
    /// (`cmd /c`-chained for several tasks; `follow`/`lines` have no
    /// meaning — there is no log stream to follow).
    ///
    /// # Errors
    /// [`Error::Failed`] on an empty/invalid name list.
    pub fn log_stream(
        &self,
        names: &[String],
        lines: u32,
        follow: bool,
    ) -> Result<(String, Vec<String>), Error> {
        match self {
            Self::Systemd(_) => Ok(("journalctl".into(), journalctl_args(names, lines, follow)?)),
            Self::Tasks(_) => windows_log_stream(names),
            Self::Launchd(d) => Ok(("tail".into(), d.tail_args(names, lines, follow)?)),
        }
    }

    /// Disable (best-effort) + remove the service's artifacts — unit
    /// file on systemd, task + wrapper/XML on Windows.
    ///
    /// # Errors
    /// [`Error::Failed`] on an invalid name; propagates the backend
    /// failure.
    pub fn uninstall(&self, name: &str) -> Result<(), Error> {
        match self {
            Self::Systemd(d) => d.uninstall(name),
            Self::Tasks(d) => d.uninstall(name),
            Self::Launchd(d) => d.uninstall(name),
        }
    }
}

impl Default for ServiceBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Windows `log_stream`: one task → `schtasks /query /tn <name> /v
/// /fo list`; several → the same queries `&`-chained under
/// `cmd.exe /c` (names are validated — no quotes — before embedding).
fn windows_log_stream(names: &[String]) -> Result<(String, Vec<String>), Error> {
    if names.is_empty() {
        return Err(Error::Failed("logs: no tasks given".into()));
    }
    for name in names {
        // Validates the name for both the single- and multi-task paths.
        tamias_deploy_windows::query_args(name)?;
    }
    if names.len() == 1 {
        return Ok((
            "schtasks".into(),
            tamias_deploy_windows::query_args(&names[0])?,
        ));
    }
    let chain = names
        .iter()
        .map(|n| format!("schtasks /query /tn \"{n}\" /v /fo list"))
        .collect::<Vec<_>>()
        .join(" & ");
    Ok(("cmd.exe".into(), vec!["/d".into(), "/c".into(), chain]))
}

/// Best-effort `schtasks /query /tn <unit> /fo csv /nh` — bounded at
/// 3s; the CSV status field (`Ready`/`Running`/…) is the state,
/// stderr text the fallback when the task can't be queried.
#[cfg(windows)]
fn windows_unit_state(unit: &str) -> Option<String> {
    let args = tamias_deploy_windows::query_csv_args(unit).ok()?;
    let mut cmd = std::process::Command::new("schtasks");
    cmd.args(&args);
    let out = output_with_timeout(&mut cmd, Duration::from_secs(3)).ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(state) = tamias_deploy_windows::csv_status(&text) {
        return Some(state);
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    (!err.is_empty()).then_some(err)
}

/// The provider block of an installed package's manifest — `None`
/// when the payload carries no manifest or no `provider:` (a plain
/// package has nothing declarative to deploy).
fn provider_of(entry: &StoreEntry) -> Result<Option<Manifest>, Error> {
    if Manifest::path_in(&entry.dir).is_none() {
        return Ok(None);
    }
    let manifest = entry.manifest()?;
    Ok(manifest.provider.is_some().then_some(manifest))
}

/// The provider display/identity base — `provider.name`, else the
/// scope-qualified package name (shared by [`unit_name`] and
/// [`list`]).
#[must_use]
pub fn provider_base(manifest: &Manifest) -> String {
    manifest
        .provider
        .as_ref()
        .and_then(|p| p.name.clone())
        .unwrap_or_else(|| match &manifest.scope {
            Some(scope) => format!("{scope}-{}", manifest.name),
            None => manifest.name.clone(),
        })
}

/// The unit base name a provider service renders to —
/// `<provider-or-package>-<service>`. Deterministic: two packages
/// claiming the same unit name resolve last-deploy-wins on the unit
/// file.
#[must_use]
pub fn unit_name(manifest: &Manifest, service: &str) -> String {
    format!("{}-{service}", provider_base(manifest))
}

/// Render a provider service as a [`ServiceSpec`] — exec string →
/// argv via [`split_exec`], `~` expansion deferred to the renderer.
fn service_spec(
    entry: &StoreEntry,
    manifest: &Manifest,
    service: &tamias_core::ProviderService,
) -> Result<ServiceSpec, Error> {
    Ok(ServiceSpec {
        name: unit_name(manifest, &service.name),
        description: format!("{} — {}", entry.label(), service.name),
        exec: split_exec(&service.exec)?,
        env: service
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        env_file: service.env_file.clone(),
        restart: service.restart.clone(),
        restart_sec: None,
        workdir: service.workdir.clone(),
        after: vec![],
    })
}

/// Best-effort host-backend state probe — `systemctl --user
/// is-enabled <unit>` on Linux/Unix, `schtasks /query` on Windows,
/// `launchctl print gui/<uid>/<label>` on macOS; bounded at 3s, `None`
/// when the backend can't answer (no user bus, missing `schtasks`/
/// `launchctl`, timeout). Exit statuses don't map cleanly to "enabled"
/// on any backend, so the state is read from stdout (stderr as
/// fallback), not the exit status.
#[must_use]
pub fn unit_state(unit: &str) -> Option<String> {
    #[cfg(windows)]
    return windows_unit_state(unit);
    #[cfg(target_os = "macos")]
    return launchd_unit_state(unit);
    #[cfg(not(any(windows, target_os = "macos")))]
    return systemd_unit_state(unit);
}

/// `systemctl --user is-enabled <unit>` probe — `is-enabled` exits
/// non-zero for disabled/missing units, so the state is read from
/// stdout (stderr as fallback), not the exit status.
#[cfg(not(any(windows, target_os = "macos")))]
fn systemd_unit_state(unit: &str) -> Option<String> {
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(["--user", "is-enabled", unit]);
    let out = output_with_timeout(&mut cmd, Duration::from_secs(3)).ok()?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if state.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return (!err.is_empty()).then_some(err);
    }
    Some(state)
}

/// `launchctl print gui/<uid>/<label>` probe — `print` exits non-zero
/// when the label isn't loaded; on success the dump's `state = …`
/// line (running/stopped/not running) is the state, stderr text the
/// fallback.
#[cfg(target_os = "macos")]
fn launchd_unit_state(unit: &str) -> Option<String> {
    let target = tamias_deploy_launchd::service_target(unit).ok()?;
    let mut cmd = std::process::Command::new("launchctl");
    cmd.arg("print").arg(&target);
    let out = output_with_timeout(&mut cmd, Duration::from_secs(3)).ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(state) = text.lines().find_map(|l| l.trim().strip_prefix("state = ")) {
        return Some(state.trim().to_string());
    }
    if out.status.success() {
        return Some("loaded".into());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    (!err.is_empty()).then_some(err)
}

/// Every installed package's provider view, in store order —
/// `provider list`'s data source. Packages without a `provider:`
/// block come back with `provider: None` (callers filter for
/// `--all`-style output).
///
/// # Errors
/// Propagates store listing failures; per-entry manifest problems
/// degrade to `provider: None` (same posture as the CLI).
pub fn list(store_root: &Path) -> Result<Vec<ProviderPackage>, Error> {
    let mut packages = Vec::new();
    for entry in Store::new(store_root).entries()? {
        let manifest = entry.manifest().ok();
        let provider = manifest.and_then(|m| {
            let p = m.provider.as_ref()?;
            let base = provider_base(&m);
            let services = p
                .services
                .iter()
                .map(|s| {
                    let unit = unit_name(&m, &s.name);
                    ServiceRow {
                        package: entry.label(),
                        service: s.name.clone(),
                        state: unit_state(&unit),
                        unit,
                        exec: s.exec.clone(),
                    }
                })
                .collect();
            Some(ProviderView {
                base,
                services,
                ingress: p.ingress.clone(),
                verbs: p.verbs.clone(),
                health: p.health.as_ref().map(|h| h.cmd.clone()),
            })
        });
        packages.push(ProviderPackage {
            label: entry.label(),
            dir: entry.dir.clone(),
            provider,
        });
    }
    Ok(packages)
}

/// The flat service table the Services pane selects from — one row
/// per `(package, provider service)` pair.
///
/// # Errors
/// Propagates [`list`] failures.
pub fn service_rows(store_root: &Path) -> Result<Vec<ServiceRow>, Error> {
    Ok(list(store_root)?
        .into_iter()
        .filter_map(|p| p.provider.map(|v| v.services))
        .flatten()
        .collect())
}

/// The unit base names an installed package's provider declares —
/// `logs`' unit list. Errors when the package has no `provider:`
/// block or declares no services.
///
/// # Errors
/// Store/manifest failures, or "no provider services".
pub fn units_of(entry: &StoreEntry) -> Result<Vec<String>, Error> {
    let Some(manifest) = provider_of(entry)? else {
        return Err(Error::Failed(format!(
            "{}: no provider: block",
            entry.label()
        )));
    };
    let units: Vec<String> = manifest
        .provider
        .as_ref()
        .map(|p| {
            p.services
                .iter()
                .map(|s| unit_name(&manifest, &s.name))
                .collect()
        })
        .unwrap_or_default();
    if units.is_empty() {
        return Err(Error::Failed(format!(
            "{}: provider declares no services",
            entry.label()
        )));
    }
    Ok(units)
}

/// `deploy <name>` — the installed package's provider deploy step:
/// the kind driver's `deploy` verb *and*, independently, every
/// `provider.services[]` entry rendered as a per-user systemd unit.
/// Units are written + daemon-reloaded but not enabled.
///
/// # Errors
/// Store lookup, driver dispatch, spec render, or unit write failures.
pub fn deploy(store_root: &Path, name: &str, config: Option<&Path>) -> Result<DeployReport, Error> {
    let entry = Store::new(store_root).find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    let mut report = DeployReport::default();
    let driver_ran =
        tamias_driver::dispatch(kind, "deploy", &entry.dir, &entry.record.name, config)?;
    if driver_ran {
        report.lines.push(format!("deployed {}", entry.label()));
    }
    let mut deployed = false;
    if let Some(manifest) = provider_of(&entry)? {
        let deploy = ServiceBackend::new();
        if let Some(provider) = &manifest.provider {
            for service in &provider.services {
                let spec = service_spec(&entry, &manifest, service)?;
                let path = deploy.install(&spec)?;
                deployed = true;
                report.lines.push(format!(
                    "unit {} → {} (enable: tamias service enable {})",
                    spec.name,
                    path.display(),
                    spec.name
                ));
            }
        }
        if deployed {
            // Best-effort: a missing user bus must not fail the deploy —
            // the unit files are already on disk.
            let _ = deploy.daemon_reload();
        }
        // Ingress hooks stay manual: `public_url` fronts a tunnel the
        // operator confirms first — auto-registering forge hooks at
        // deploy time would point repos at whatever URL happens to be
        // declared.
        let hooks: usize = manifest
            .provider
            .as_ref()
            .map(|p| p.ingress.iter().map(|i| i.hooks.len()).sum())
            .unwrap_or(0);
        if hooks > 0 {
            report.warnings.push(format!(
                "{} ingress hook(s) declared — review with `tamias driver exec {} hooks register --dry-run`, then drop --dry-run to register",
                hooks,
                entry.label()
            ));
        }
    }
    if !driver_ran && !deployed {
        report.lines.push(format!(
            "{}: nothing to deploy (type {kind})",
            entry.label()
        ));
    }
    Ok(report)
}

/// Of `names` (unit base names a just-uninstalled package declared),
/// the ones *other* still-installed providers also declare — unit →
/// `(spec, declaring package label)`, first declarer in sorted store
/// order wins. A `None` spec means the survivor's `exec` won't parse:
/// the unit is left untouched. Store/manifest read failures degrade
/// to "not declared" so a broken sibling never blocks an uninstall.
fn surviving_units(
    store_root: &Path,
    names: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> std::collections::BTreeMap<String, Option<(ServiceSpec, String)>> {
    let mut out = std::collections::BTreeMap::new();
    if names.is_empty() {
        return out;
    }
    let entries = match Store::new(store_root).entries() {
        Ok(entries) => entries,
        Err(e) => {
            warnings.push(format!("could not rescan store for shared units: {e}"));
            return out;
        }
    };
    for entry in entries {
        let manifest = match provider_of(&entry) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => continue,
            Err(e) => {
                warnings.push(format!(
                    "skipping {} in shared-unit scan: {e}",
                    entry.label()
                ));
                continue;
            }
        };
        let Some(provider) = manifest.provider.as_ref() else {
            continue;
        };
        for service in &provider.services {
            let unit = unit_name(&manifest, &service.name);
            if !names.contains(&unit) || out.contains_key(&unit) {
                continue;
            }
            match service_spec(&entry, &manifest, service) {
                Ok(spec) => {
                    out.insert(unit, Some((spec, entry.label())));
                }
                Err(e) => {
                    warnings.push(format!(
                        "{} service '{}': bad exec for shared unit {unit}: {e}",
                        entry.label(),
                        service.name
                    ));
                    out.insert(unit, None);
                }
            }
        }
    }
    out
}

/// `uninstall <name>` — provider teardown + drop the store entry:
/// driver `uninstall` first (a failing driver keeps the store entry),
/// then declarative units are removed unless a *surviving* installed
/// provider still declares the same unit name — that unit is
/// re-rendered from the survivor's spec instead.
///
/// # Errors
/// Store lookup, driver dispatch, or store removal failures.
pub fn uninstall(store_root: &Path, name: &str) -> Result<DeployReport, Error> {
    let store = Store::new(store_root);
    let entry = store.find(name)?;
    let kind = entry.record.kind.as_deref().unwrap_or("files");
    let mut report = DeployReport::default();
    // Capture the provider spec before teardown — the store entry is
    // dropped below, and the manifest is needed both to remove this
    // package's units and to tell whether a *surviving* provider still
    // declares them (unit names can be shared: several packages may
    // render the same `<provider>-<service>` unit).
    let manifest = provider_of(&entry)?;
    let unit_names: Vec<String> = manifest
        .as_ref()
        .map(|m| {
            m.provider
                .as_ref()
                .map(|p| p.services.iter().map(|s| unit_name(m, &s.name)).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    // Teardown runs before the entry is dropped: a failing driver keeps
    // the store entry so the uninstall can be retried instead of
    // leaking provider-side resources under a false "uninstalled".
    if !tamias_driver::dispatch(kind, "uninstall", &entry.dir, &entry.record.name, None)?
        && kind != "files"
    {
        report.warnings.push(format!(
            "no driver for type '{kind}'; provider resources may remain"
        ));
    }
    let record = store.remove(name)?;
    // Declarative provider services are tamias-native — remove their
    // units too, unless a surviving installed provider still declares
    // the same unit name. Units are best-effort per name: a failure
    // warns but doesn't fail the uninstall — the store entry is
    // already dropped.
    let deploy = ServiceBackend::new();
    let survivors = surviving_units(
        store_root,
        &unit_names.iter().cloned().collect::<BTreeSet<_>>(),
        &mut report.warnings,
    );
    for unit in &unit_names {
        match survivors.get(unit) {
            Some(Some((spec, label))) => match deploy.install(spec) {
                Ok(path) => report.lines.push(format!(
                    "unit {unit} is shared — re-rendered from {label} → {}",
                    path.display()
                )),
                Err(e) => report
                    .warnings
                    .push(format!("could not refresh shared unit {unit}: {e}")),
            },
            Some(None) => report.warnings.push(format!(
                "unit {unit} is still declared but its surviving spec is invalid; left untouched"
            )),
            None => {
                if let Err(e) = deploy.uninstall(unit) {
                    report
                        .warnings
                        .push(format!("could not remove unit {unit}: {e}"));
                }
            }
        }
    }
    if !unit_names.is_empty() {
        // Best-effort: a missing user bus must not fail the uninstall —
        // the unit files are already written/removed.
        let _ = deploy.daemon_reload();
    }
    report
        .lines
        .push(format!("uninstalled {}@{}", record.name, record.resolved));
    Ok(report)
}

/// Strip one layer of matching surrounding quotes from an env_file
/// value — systemd unquotes `KEY="a b"`/`KEY='a b'` the same way.
#[must_use]
pub fn unquote_env(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

/// `run <pkg> [service] [-- args…]`'s resolved exec — the provider
/// service's argv (plus `extra`), env (`env_file` parsed as `K=V`
/// lines, inline `env` overriding — the same order systemd applies),
/// and workdir (`~` expanded, else the payload dir). This is the
/// no-systemd counterpart of the rendered unit.
///
/// # Errors
/// Store lookup, missing `provider:`/service name, bad `exec` split,
/// or unreadable `env_file`.
pub fn exec_spec(
    store_root: &Path,
    name: &str,
    service: Option<&str>,
    extra: &[String],
) -> Result<ExecSpec, Error> {
    let entry = Store::new(store_root).find(name)?;
    let manifest = provider_of(&entry)?.ok_or_else(|| {
        Error::Failed(format!(
            "{}: no provider: block — `run` needs provider.services",
            entry.label()
        ))
    })?;
    let provider = manifest
        .provider
        .as_ref()
        .ok_or_else(|| Error::Failed(format!("{}: no provider: block", entry.label())))?;
    let names = || {
        provider
            .services
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let svc = match service {
        Some(s) => provider
            .services
            .iter()
            .find(|x| x.name == s)
            .ok_or_else(|| {
                Error::Failed(format!(
                    "{}: no provider service '{s}' (have: {})",
                    entry.label(),
                    names()
                ))
            })?,
        None => match provider.services.as_slice() {
            [] => {
                return Err(Error::Failed(format!(
                    "{}: provider declares no services",
                    entry.label()
                )));
            }
            [one] => one,
            many => {
                return Err(Error::Failed(format!(
                    "{}: provider declares {} services — pass one: {}",
                    entry.label(),
                    many.len(),
                    names()
                )));
            }
        },
    };
    let mut argv = split_exec(&svc.exec)?;
    argv.extend(extra.iter().cloned());
    // env_file first, inline env overrides — systemd's order.
    let mut vars: Vec<(String, String)> = Vec::new();
    if let Some(env_file) = &svc.env_file {
        let path = PathBuf::from(expand_home(env_file));
        // The unit renders `EnvironmentFile=-` (optional) — a missing
        // file is not an error here either.
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(Error::io(format!("read {}", path.display())))?;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                match line.split_once('=') {
                    Some((k, v)) => vars.push((k.trim().to_string(), unquote_env(v.trim()))),
                    None => {
                        eprintln!(
                            "tamias: warning — ignoring malformed env_file line in {}",
                            path.display()
                        );
                    }
                }
            }
        }
    }
    vars.extend(svc.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let workdir = svc
        .workdir
        .as_deref()
        .map(expand_home)
        .map(PathBuf::from)
        .unwrap_or_else(|| entry.dir.clone());
    Ok(ExecSpec {
        service: svc.name.clone(),
        argv,
        env: vars,
        workdir,
    })
}

/// A host-backend lifecycle verb on a unit/task base name —
/// `tamias service enable|disable` equivalents plus start/stop/
/// restart for the interactive surfaces.
///
/// # Errors
/// [`Error::Failed`] on an invalid unit name; propagates the backend
/// failure.
pub fn unit_action(unit: &str, action: UnitAction) -> Result<(), Error> {
    let deploy = ServiceBackend::new();
    match action {
        UnitAction::Start => deploy.start(unit),
        UnitAction::Stop => deploy.stop(unit),
        UnitAction::Restart => deploy.restart(unit),
        UnitAction::Enable => deploy.enable(unit),
        UnitAction::Disable => deploy.disable(unit),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn unit_name_uses_provider_name_then_scope() {
        let m = Manifest {
            name: "gate".into(),
            scope: Some("ci".into()),
            provider: Some(tamias_core::Provider {
                name: Some("prov".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(provider_base(&m), "prov");
        assert_eq!(unit_name(&m, "web"), "prov-web");
        let no_provider_name = Manifest {
            name: "gate".into(),
            scope: Some("ci".into()),
            provider: Some(tamias_core::Provider::default()),
            ..Default::default()
        };
        assert_eq!(provider_base(&no_provider_name), "ci-gate");
        assert_eq!(unit_name(&no_provider_name, "web"), "ci-gate-web");
    }

    #[test]
    fn unquote_env_strips_matching_quotes() {
        assert_eq!(unquote_env("\"a b\""), "a b");
        assert_eq!(unquote_env("'a b'"), "a b");
        assert_eq!(unquote_env("plain"), "plain");
        assert_eq!(unquote_env("\"unclosed"), "\"unclosed");
        assert_eq!(unquote_env("\"\""), "");
    }

    #[test]
    fn list_and_rows_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list(tmp.path()).unwrap().is_empty());
        assert!(service_rows(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn exec_spec_rejects_missing_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("store"));
        let payload = tmp.path().join("pkg");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("tamias.yaml"), "name: w\nversion: 1.0.0\n").unwrap();
        std::fs::write(payload.join("a.txt"), "a").unwrap();
        store
            .install(
                &payload,
                tamias_core::StoreRecord {
                    name: "w".into(),
                    scope: None,
                    version: Some("1.0.0".into()),
                    kind: Some("files".into()),
                    source_ref: "./pkg".into(),
                    resolved: "sha256:0123456789abcdef".into(),
                    digest: String::new(),
                    signature: None,
                    installed_at: 0,
                },
            )
            .unwrap();
        // no provider: block → the run spec errors like the CLI
        assert!(exec_spec(&store_root(tmp.path()), "w", None, &[]).is_err());
    }

    fn store_root(tmp: &Path) -> PathBuf {
        tmp.join("store")
    }
}
