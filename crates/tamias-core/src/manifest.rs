//! `tamias.{yaml,yml,toml}` — the package manifest. YAML is preferred
//! when several exist; TOML stays the fallback.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

/// Manifest file names tried, in order — YAML first, TOML last.
pub const MANIFEST_NAMES: [&str; 3] = ["tamias.yaml", "tamias.yml", "tamias.toml"];

/// `[A-Za-z0-9][A-Za-z0-9._-]*` — the package name/scope charset.
/// Always a single safe path component.
#[must_use]
pub fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}

/// Version-label charset — `[A-Za-z0-9][A-Za-z0-9._+-]*` with no `..`.
/// Covers semver, prerelease/build suffixes, dist-tags (`latest`), and
/// the sha short-form fallback; always a single safe path/URL component.
#[must_use]
pub fn is_valid_version(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && !s.contains("..")
}

/// The package manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Manifest {
    /// Bare package name — charset `[A-Za-z0-9][A-Za-z0-9._-]*`.
    pub name: String,
    /// Optional namespace (`@scope/name`).
    pub scope: Option<String>,
    /// Author-declared version (semver-ish label).
    pub version: Option<String>,
    /// Free-text description.
    pub description: Option<String>,
    /// Content type (`peregrin-bundle`, `myco-config`, …).
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Base packages to compose under this payload — installed store
    /// names or installable source refs. Local wins; earlier includes
    /// win over later ones.
    pub include: Vec<String>,
    /// Direct dependencies — refs this project wants installed.
    pub deps: Vec<String>,
    /// `provider:` — the declarative runtime spec a deploy driver
    /// materializes (service units, ingress/forge hooks, health probe).
    /// Absent = a plain package with nothing to deploy.
    pub provider: Option<Provider>,
}

/// `provider:` — declarative service/runtime spec. Deploy drivers turn
/// this into host resources (systemd units, forge webhooks); tamias
/// itself never interprets it beyond validation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Provider {
    /// Provider/service identity — defaults to the package name.
    pub name: Option<String>,
    /// Services the provider runs (one unit each).
    pub services: Vec<ProviderService>,
    /// Public ingress endpoints — forge hooks, tunnels.
    pub ingress: Vec<ProviderIngress>,
    /// Advisory verb hints for the CLI surface (`run`, `logs`,
    /// `health`, …) — not enforced.
    pub verbs: Vec<String>,
    /// Optional health probe.
    pub health: Option<ProviderHealth>,
}

/// One service under `provider.services` — rendered as a systemd unit
/// (or the platform equivalent) by the deploy driver.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProviderService {
    /// Unit/service name — `[A-Za-z0-9_.@-]`, no `..` (same charset the
    /// local deploy adapter enforces for unit names).
    pub name: String,
    /// `ExecStart` — the command line, an argv string the driver renders
    /// with quoting.
    pub exec: String,
    /// Optional env file (`EnvironmentFile=`).
    pub env_file: Option<String>,
    /// Inline env (`Environment=`) — names + values only; secrets stay
    /// in `env_file`/the keyring.
    pub env: std::collections::BTreeMap<String, String>,
    /// Optional working directory (`WorkingDirectory=`); `~` allowed.
    pub workdir: Option<String>,
    /// Restart policy (`Restart=`) — `on-failure` when unset.
    pub restart: Option<String>,
}

/// One public endpoint under `provider.ingress`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProviderIngress {
    /// The public URL this ingress fronts.
    pub public_url: String,
    /// The address/port the service binds (`127.0.0.1:29173`).
    pub listen: Option<String>,
    /// URL path prefix routed to the service (`/hook`).
    pub path: Option<String>,
    /// Declarative forge-hook spec consumed by driver deploy.
    pub hooks: Vec<ProviderHook>,
}

/// One forge webhook under `provider.ingress[].hooks`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProviderHook {
    /// The forge (`github`, `gitlab`, …).
    pub forge: String,
    /// `owner/repo`.
    pub repo: String,
    /// Event names (`push`, `pull_request`, …).
    pub events: Vec<String>,
}

/// `provider.health` — an optional probe the driver/CLI can run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProviderHealth {
    /// Probe command line.
    pub cmd: String,
}

/// `[A-Za-z0-9_.@-]`, no `..`, not `.`/`..` — the unit-name charset the
/// local deploy adapter enforces; a provider service name becomes a
/// unit name, so the manifest rejects anything else up front.
fn is_valid_unit_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '@' | '-'))
}

/// Reject line breaks/NUL in a field that lands on one unit-file line —
/// mirrors the deploy adapter's `check_unit_fields`.
fn check_single_line(what: &str, value: &str) -> Result<(), Error> {
    if value.contains(['\n', '\r', '\0']) {
        Err(Error::Failed(format!(
            "provider {what} contains a line break or NUL"
        )))
    } else {
        Ok(())
    }
}

impl Provider {
    /// Semantic checks the schema can't express — required fields,
    /// unit-name charset, no duplicate service names.
    ///
    /// # Errors
    /// Fails on a malformed provider spec.
    fn check(&self) -> Result<(), Error> {
        if let Some(name) = &self.name {
            if !is_valid_name(name) {
                return Err(Error::Failed(format!("invalid provider name '{name}'")));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for service in &self.services {
            if !is_valid_unit_name(&service.name) {
                return Err(Error::Failed(format!(
                    "invalid provider service name '{}' (expected [A-Za-z0-9_.@-], no '..')",
                    service.name
                )));
            }
            if !seen.insert(service.name.as_str()) {
                return Err(Error::Failed(format!(
                    "duplicate provider service name '{}'",
                    service.name
                )));
            }
            if service.exec.trim().is_empty() {
                return Err(Error::Failed(format!(
                    "provider service '{}' is missing exec",
                    service.name
                )));
            }
            for (what, value) in [
                ("service exec", service.exec.as_str()),
                (
                    "service env_file",
                    service.env_file.as_deref().unwrap_or(""),
                ),
                ("service workdir", service.workdir.as_deref().unwrap_or("")),
                ("service restart", service.restart.as_deref().unwrap_or("")),
            ] {
                check_single_line(what, value)?;
            }
            for (key, value) in &service.env {
                if key.is_empty() {
                    return Err(Error::Failed(format!(
                        "provider service '{}' has an empty env name",
                        service.name
                    )));
                }
                check_single_line("service env name", key)?;
                check_single_line("service env value", value)?;
            }
        }
        for ingress in &self.ingress {
            if ingress.public_url.trim().is_empty() {
                return Err(Error::Failed(
                    "provider ingress is missing public_url".to_string(),
                ));
            }
            for (what, value) in [
                ("ingress listen", ingress.listen.as_deref().unwrap_or("")),
                ("ingress path", ingress.path.as_deref().unwrap_or("")),
            ] {
                check_single_line(what, value)?;
            }
            for hook in &ingress.hooks {
                if hook.forge.trim().is_empty() || hook.repo.trim().is_empty() {
                    return Err(Error::Failed(
                        "provider hook is missing forge or repo".to_string(),
                    ));
                }
                if hook.events.iter().any(|e| e.trim().is_empty()) {
                    return Err(Error::Failed(
                        "provider hook has an empty event name".to_string(),
                    ));
                }
            }
        }
        if self.verbs.iter().any(|v| v.trim().is_empty()) {
            return Err(Error::Failed(
                "provider verbs must be non-empty".to_string(),
            ));
        }
        if let Some(health) = &self.health {
            if health.cmd.trim().is_empty() {
                return Err(Error::Failed("provider health is missing cmd".to_string()));
            }
            check_single_line("health cmd", &health.cmd)?;
        }
        Ok(())
    }
}

impl Manifest {
    /// The manifest file inside `dir`, trying [`MANIFEST_NAMES`].
    #[must_use]
    pub fn path_in(dir: &Path) -> Option<PathBuf> {
        MANIFEST_NAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
    }

    /// Load + validate the manifest inside `dir`.
    ///
    /// # Errors
    /// Fails when no manifest exists or it doesn't validate.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let Some(path) = Self::path_in(dir) else {
            return Err(Error::Failed(format!(
                "no tamias.yaml/.yml/.toml in {}",
                dir.display()
            )));
        };
        Self::load_at(&path)
    }

    /// Load + validate a manifest file; syntax dispatched on the
    /// `.yaml`/`.yml` extension (everything else parses as TOML).
    ///
    /// # Errors
    /// Fails on read, parse, or validation error.
    pub fn load_at(path: &Path) -> Result<Self, Error> {
        let manifest = Self::parse_at(path)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Load a *project* manifest file — `tamias.yaml` holding `deps:` /
    /// `include:` — same parse as [`Self::load_at`] but validated with
    /// [`Self::validate_project`], so a missing `name` is allowed: a
    /// project needn't be a package.
    ///
    /// # Errors
    /// Fails on read, parse, or validation error.
    pub fn load_project_at(path: &Path) -> Result<Self, Error> {
        let manifest = Self::parse_at(path)?;
        manifest.validate_project()?;
        Ok(manifest)
    }

    /// Parse a manifest file without validating it.
    fn parse_at(path: &Path) -> Result<Self, Error> {
        let text =
            std::fs::read_to_string(path).map_err(Error::io(format!("read {}", path.display())))?;
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml")
        );
        if is_yaml {
            serde_yaml_ng::from_str::<Self>(&text)
                .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))
        } else {
            toml::from_str(&text)
                .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))
        }
    }

    /// Semantic checks the schema can't express — a package manifest must
    /// name itself.
    ///
    /// # Errors
    /// Fails on a missing/invalid name, scope, or version.
    pub fn validate(&self) -> Result<(), Error> {
        self.check(true)
    }

    /// Semantic checks for a *project* manifest — the same field rules as
    /// [`Self::validate`] but `name` may be empty.
    ///
    /// # Errors
    /// Fails on an invalid name/scope/version.
    pub fn validate_project(&self) -> Result<(), Error> {
        self.check(false)
    }

    fn check(&self, require_name: bool) -> Result<(), Error> {
        if self.name.is_empty() {
            if require_name {
                return Err(Error::Failed(
                    "manifest is missing a package name".to_string(),
                ));
            }
        } else if !is_valid_name(&self.name) {
            return Err(Error::Failed(format!(
                "invalid package name '{}' — expected [A-Za-z0-9][A-Za-z0-9._-]*",
                self.name
            )));
        }
        if let Some(scope) = &self.scope {
            if !is_valid_name(scope) {
                return Err(Error::Failed(format!("invalid scope '{scope}'")));
            }
        }
        if let Some(version) = &self.version {
            if !is_valid_version(version) {
                return Err(Error::Failed(format!(
                    "invalid version '{version}' — expected [A-Za-z0-9][A-Za-z0-9._+-]* with no '..'"
                )));
            }
        }
        if let Some(provider) = &self.provider {
            provider.check()?;
        }
        Ok(())
    }

    /// Write this manifest to `path` as YAML.
    ///
    /// # Errors
    /// Propagates serialize + write failures.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let ctx = |e| Error::Failed(format!("serialize manifest: {e}"));
        let text = serde_yaml_ng::to_string(self).map_err(ctx)?;
        crate::write_atomic(path, text.as_bytes())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn load_yaml_toml_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tamias.toml"),
            "name = \"x\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let manifest = Manifest::load(dir.path()).unwrap();
        assert_eq!(manifest.name, "x");
        assert!(Manifest::path_in(dir.path()).is_some());
        let saved = dir.path().join("saved.yaml");
        manifest.save(&saved).unwrap();
        assert_eq!(Manifest::load_at(&saved).unwrap().name, "x");
        let empty = tempfile::tempdir().unwrap();
        assert!(Manifest::load(empty.path()).is_err());
        assert!(Manifest {
            name: "bad name".into(),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(Manifest {
            name: "x".into(),
            scope: Some("bad scope".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
        // a package manifest must name itself; a project manifest needn't
        assert!(Manifest::default().validate().is_err());
        assert!(Manifest::default().validate_project().is_ok());
        assert!(Manifest {
            name: "bad name".into(),
            ..Default::default()
        }
        .validate_project()
        .is_err());
        // nameless yaml still loads as a project manifest
        let proj = dir.path().join("proj.yaml");
        std::fs::write(&proj, "deps:\n  - other/pkg\n").unwrap();
        let project = Manifest::load_project_at(&proj).unwrap();
        assert!(project.name.is_empty());
        assert_eq!(project.deps, ["other/pkg"]);
        assert!(Manifest::load_at(&proj).is_err());
    }

    #[test]
    fn version_validation() {
        for bad in [
            "../../evil",
            "a/b",
            "a\\b",
            "..",
            ".",
            "-lead",
            ".lead",
            "",
            "1..0",
            "a b",
            "a\0b",
        ] {
            assert!(
                Manifest {
                    name: "x".into(),
                    version: Some(bad.into()),
                    ..Default::default()
                }
                .validate()
                .is_err(),
                "version {bad:?} must fail"
            );
        }
        for good in [
            "1.0.0",
            "latest",
            "0.1.0-rc.1",
            "1.0.0+build.5",
            "deadbeefcafe",
        ] {
            assert!(
                Manifest {
                    name: "x".into(),
                    version: Some(good.into()),
                    ..Default::default()
                }
                .validate()
                .is_ok(),
                "version {good:?} must pass"
            );
        }
    }

    #[test]
    fn load_at_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, ": not valid [ yaml").unwrap();
        assert!(Manifest::load_at(&path).is_err());
    }

    #[test]
    fn provider_block_parses_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tamias.yaml");
        std::fs::write(
            &path,
            r#"name: ci-gate
version: 1.0.0
provider:
  name: ci-gate
  services:
    - name: ci-gate
      exec: "peregrin campaign serve /srv/ci"
      env_file: ~/.config/peregrin/env
      env: {PORT: "29173"}
      workdir: "~"
      restart: on-failure
  ingress:
    - public_url: https://ci.example.com
      listen: 127.0.0.1:29173
      path: /hook
      hooks:
        - forge: github
          repo: 9aia/peregrin
          events: [push]
  verbs: [run, logs, health]
  health: {cmd: "curl -sf localhost:29173/health"}
"#,
        )
        .unwrap();
        let manifest = Manifest::load_at(&path).unwrap();
        let provider = manifest.provider.as_ref().unwrap();
        assert_eq!(provider.name.as_deref(), Some("ci-gate"));
        let service = &provider.services[0];
        assert_eq!(service.name, "ci-gate");
        assert_eq!(service.env_file.as_deref(), Some("~/.config/peregrin/env"));
        assert_eq!(service.env["PORT"], "29173");
        assert_eq!(service.workdir.as_deref(), Some("~"));
        assert_eq!(service.restart.as_deref(), Some("on-failure"));
        assert_eq!(provider.ingress[0].public_url, "https://ci.example.com");
        assert_eq!(provider.ingress[0].hooks[0].forge, "github");
        assert_eq!(provider.verbs, ["run", "logs", "health"]);
        assert_eq!(
            provider.health.as_ref().unwrap().cmd,
            "curl -sf localhost:29173/health"
        );
        // round-trip through save/load keeps the block
        let saved = dir.path().join("saved.yaml");
        manifest.save(&saved).unwrap();
        assert_eq!(
            Manifest::load_at(&saved).unwrap().provider,
            manifest.provider
        );
        // absent provider stays absent
        let plain = dir.path().join("plain.yaml");
        std::fs::write(&plain, "name: p\n").unwrap();
        assert!(Manifest::load_at(&plain).unwrap().provider.is_none());
    }

    #[test]
    fn provider_validation_rejects_malformed() {
        let base = || Manifest {
            name: "x".into(),
            ..Default::default()
        };
        let with_provider = |provider: Provider| Manifest {
            provider: Some(provider),
            ..base()
        };
        // a service without a name
        assert!(with_provider(Provider {
            services: vec![ProviderService {
                exec: "/bin/x".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .validate()
        .is_err());
        // bad unit-name charset / traversal
        for bad in ["a/b", "../x", "a b", ""] {
            assert!(
                with_provider(Provider {
                    services: vec![ProviderService {
                        name: bad.into(),
                        exec: "/bin/x".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .validate()
                .is_err(),
                "service name {bad:?} must fail"
            );
        }
        // missing exec
        assert!(with_provider(Provider {
            services: vec![ProviderService {
                name: "ok".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .validate()
        .is_err());
        // duplicate service names
        let svc = |name: &str| ProviderService {
            name: name.into(),
            exec: "/bin/x".into(),
            ..Default::default()
        };
        assert!(with_provider(Provider {
            services: vec![svc("dup"), svc("dup")],
            ..Default::default()
        })
        .validate()
        .is_err());
        // ingress without public_url
        assert!(with_provider(Provider {
            ingress: vec![ProviderIngress::default()],
            ..Default::default()
        })
        .validate()
        .is_err());
        // hook missing repo
        assert!(with_provider(Provider {
            ingress: vec![ProviderIngress {
                public_url: "https://x".into(),
                hooks: vec![ProviderHook {
                    forge: "github".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })
        .validate()
        .is_err());
        // unit-file injection attempts
        assert!(with_provider(Provider {
            services: vec![ProviderService {
                name: "ok".into(),
                exec: "/bin/x\nRestart=always".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .validate()
        .is_err());
        assert!(with_provider(Provider {
            health: Some(ProviderHealth {
                cmd: "true\n[Install]".into(),
            }),
            ..Default::default()
        })
        .validate()
        .is_err());
        // a minimal valid provider passes
        assert!(with_provider(Provider {
            services: vec![svc("ok@user")],
            verbs: vec!["run".into()],
            ..Default::default()
        })
        .validate()
        .is_ok());
    }

    #[test]
    fn load_error_arms() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Manifest::load_at(&dir.path().join("nope.yaml")).is_err());
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "name = = broken").unwrap();
        assert!(Manifest::load_at(&bad).is_err());
        let inv = dir.path().join("inv.yaml");
        std::fs::write(&inv, "name: \"-bad\"\n").unwrap();
        assert!(Manifest::load_at(&inv).is_err());
        let badver = dir.path().join("badver.yaml");
        std::fs::write(&badver, "name: x\nversion: \"../../evil\"\n").unwrap();
        assert!(Manifest::load_at(&badver).is_err());
    }
}
