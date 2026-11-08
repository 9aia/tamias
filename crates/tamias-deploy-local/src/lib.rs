#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-deploy-local — the `deploy` port's local adapter: manage a
//! **per-user service unit** (systemd on Linux; launchd on macOS). tamias
//! owns the host-service *mechanism*; a content type's provider supplies
//! the domain spec (ExecStart, env, workdir).

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
    /// `ExecStart` — the command line the service runs.
    pub exec: String,
    /// `K=V` environment entries.
    pub env: Vec<(String, String)>,
    /// Optional working directory.
    pub workdir: Option<String>,
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
    #[must_use]
    pub fn unit_path(&self, name: &str) -> PathBuf {
        self.unit_dir.join(format!("{name}.service"))
    }

    /// Write the unit file (does not enable it).
    ///
    /// # Errors
    /// Propagates write failures.
    pub fn install(&self, spec: &ServiceSpec) -> Result<PathBuf, Error> {
        std::fs::create_dir_all(&self.unit_dir)
            .map_err(Error::io(format!("create {}", self.unit_dir.display())))?;
        let path = self.unit_path(&spec.name);
        std::fs::write(&path, render_unit(spec))
            .map_err(Error::io(format!("write {}", path.display())))?;
        Ok(path)
    }

    /// `systemctl --user enable --now <unit>`.
    ///
    /// # Errors
    /// Propagates the runner failure.
    pub fn enable(&self, name: &str) -> Result<(), Error> {
        self.runner
            .run("systemctl", &["--user", "enable", "--now", &unit(name)])
    }

    /// `systemctl --user disable --now <unit>`.
    ///
    /// # Errors
    /// Propagates the runner failure.
    pub fn disable(&self, name: &str) -> Result<(), Error> {
        self.runner
            .run("systemctl", &["--user", "disable", "--now", &unit(name)])
    }

    /// `systemctl --user status <unit>`.
    ///
    /// # Errors
    /// Propagates the runner failure.
    pub fn status(&self, name: &str) -> Result<(), Error> {
        self.runner.run(
            "systemctl",
            &["--user", "status", "--no-pager", &unit(name)],
        )
    }

    /// `journalctl --user -u <unit> -n 200`.
    ///
    /// # Errors
    /// Propagates the runner failure.
    pub fn logs(&self, name: &str) -> Result<(), Error> {
        self.runner.run(
            "journalctl",
            &["--user", "-u", &unit(name), "-n", "200", "--no-pager"],
        )
    }

    /// Disable (best-effort) + remove the unit file.
    ///
    /// # Errors
    /// Propagates the unit-file removal failure.
    pub fn uninstall(&self, name: &str) -> Result<(), Error> {
        let _ = self.disable(name);
        let path = self.unit_path(name);
        if path.exists() {
            std::fs::remove_file(&path).map_err(Error::io(format!("remove {}", path.display())))?;
        }
        Ok(())
    }
}

impl Default for LocalDeploy {
    fn default() -> Self {
        Self::new()
    }
}

fn unit(name: &str) -> String {
    format!("{name}.service")
}

/// Render a systemd `.service` unit.
#[must_use]
pub fn render_unit(spec: &ServiceSpec) -> String {
    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str(&format!("Description={}\n", spec.description));
    unit.push_str("After=network.target\n\n");
    unit.push_str("[Service]\n");
    unit.push_str("Type=simple\n");
    unit.push_str(&format!("ExecStart={}\n", spec.exec));
    if let Some(workdir) = &spec.workdir {
        unit.push_str(&format!("WorkingDirectory={workdir}\n"));
    }
    for (k, v) in &spec.env {
        unit.push_str(&format!("Environment={k}={v}\n"));
    }
    unit.push_str("Restart=on-failure\n\n");
    unit.push_str("[Install]\n");
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
            exec: "/usr/bin/peregrin serve".into(),
            env: vec![(
                "PEREGRIN_HOME".into(),
                "/home/u/.local/share/peregrin".into(),
            )],
            workdir: None,
        }
    }

    #[test]
    fn renders_a_unit() {
        let text = render_unit(&spec());
        assert!(text.contains("ExecStart=/usr/bin/peregrin serve"));
        assert!(text.contains("Environment=PEREGRIN_HOME=/home/u/.local/share/peregrin"));
        assert!(text.contains("WantedBy=default.target"));
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
        assert!(!deploy.unit_path("peregrin").exists());
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
            exec: "/bin/x".into(),
            env: vec![("A".into(), "1".into())],
            workdir: Some("/w".into()),
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
