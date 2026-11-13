//! `tamias.{yaml,yml,toml}` — the package manifest. YAML is preferred
//! when several exist; TOML stays the fallback.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sealed::Sealed;
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

/// Content-type charset — `[a-z0-9][a-z0-9._-]*` with no `..`. The
/// kind becomes `tamias-driver-<kind>` in PATH/home lookups, so it
/// must stay a single safe path component (no `/`, no separators, no
/// traversal).
#[must_use]
pub fn is_valid_kind(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
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
    ///
    /// Applied on install (`tamias-install` resolves each entry —
    /// store name → that entry's payload dir, ref → a recursive
    /// install — then [`crate::overlay::overlay`]s bases under the
    /// local payload; the store record's digest covers the *composed*
    /// tree and the manifest rides along unchanged for provenance) and
    /// flattened at publish/pack time (`tamias-publish`, read-only —
    /// `--keep-includes` ships the raw payload instead).
    pub include: Vec<String>,
    /// Direct dependencies — refs this project wants installed.
    /// Recorded by `tamias add` (and the TUI deps form) into the
    /// project manifest; `tamias.lock` pins each resolved record.
    /// `tamias sync` (CLI/serve/TUI) reconciles this set against the
    /// store + lock — install-if-missing-or-drifted, never removes.
    pub deps: Vec<String>,
    /// `provider:` — the declarative runtime spec a deploy driver
    /// materializes (service units, ingress/forge hooks, health probe).
    /// Absent = a plain package with nothing to deploy.
    pub provider: Option<Provider>,
    /// `bins:` — executable surfaces the package declares (P-023).
    /// `tamias bin deploy` materializes them into the bin dir; `tamias
    /// install` deploys them by default. Pure data — tamias never
    /// learns a product's CLI shape.
    pub bins: Vec<BinDecl>,
    /// `sealed:` — the pack is **ciphertext-only** (tamias ADR-0004 /
    /// org P-027): every payload file is age-encrypted and tamias
    /// carries it opaquely — digest, verify, and sign work on bytes
    /// unchanged; there is no decrypt path in the engine. Orthogonal
    /// to `type:` (`type: secrets` is a cuora-driver convention, not a
    /// requirement). Sealed packs reject `include:`/`bins:`/`provider:`
    /// — composition, bin deploys, and service specs over ciphertext
    /// are meaningless; `deps:` stays allowed.
    pub sealed: Option<Sealed>,
}

/// One `bins[]` entry — a command name plus *how* it materializes.
///
/// Exactly one of `shim`/`payload` is set:
///
/// - `shim: "cli"` + `driver: <kind>` — a generated front-end: tamias
///   writes a small script into the bin dir that execs
///   `tamias bin exec <label> <name> "$@"`, which resolves the kind
///   driver (`tamias-driver-<kind>`, falling back through capabilities
///   — e.g. a generic `tamias-driver-cli`) and dispatches its `cli`
///   verb with the package's `--dir`/`--name`. The shim is a script,
///   not a compiled helper: `tamias` itself is the helper binary, so
///   driver discovery stays dynamic (PATH + `<tamias-home>/drivers/`,
///   including drivers the package itself just installed).
/// - `payload: tools/bin/<triple>/<file>` — a real binary shipped in
///   the payload. The `<triple>` segment selects the host platform
///   (`tamias_driver::target_triple`); declaring another triple's
///   path resolves to the host's dir at deploy time, and a payload
///   whose `tools/bin/` ships platform dirs but none for this host
///   fails the install (same rule as `drivers/bin/`). A path without
///   a triple dir (`tools/bin/<file>`) is a portable payload bin.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct BinDecl {
    /// Command name on `PATH` — `[A-Za-z0-9][A-Za-z0-9._-]*`, never
    /// `tamias` itself (reserved for the real binary).
    pub name: String,
    /// Shim kind — only `"cli"` (generated driver front-end) exists.
    pub shim: Option<String>,
    /// Driver kind the `cli` shim dispatches to — resolved like a
    /// content type (`tamias-driver-<kind>` → capabilities fallback).
    /// Required iff `shim` is set.
    pub driver: Option<String>,
    /// Payload-relative path to a real binary — `tools/bin/…`.
    /// Required iff `shim` is unset.
    pub payload: Option<String>,
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

/// `bins[].payload` path rules — a relative `tools/bin/…` path made of
/// plain components only (no `.`/`..`, no root/prefix, no separators
/// beyond `/`, no NUL). The triple-selection and on-disk checks live
/// in `tamias-bin`; this is the manifest-level safety gate.
fn check_bin_payload_path(bin: &str, payload: &str) -> Result<(), Error> {
    let bad = |why: &str| {
        Err(Error::Failed(format!(
            "bin '{bin}': invalid payload path '{payload}' — {why}"
        )))
    };
    if payload.is_empty() || payload.contains(['\0', '\\']) {
        return bad("empty or contains NUL/backslash");
    }
    let comps: Vec<&str> = payload.split('/').collect();
    if comps
        .iter()
        .any(|c| c.is_empty() || *c == "." || *c == "..")
    {
        return bad("must be a clean relative path (no empty, '.', or '..' segments)");
    }
    if comps.len() < 3 || comps[0] != "tools" || comps[1] != "bin" {
        return bad("must live under tools/bin/");
    }
    Ok(())
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
        if let Some(kind) = &self.kind {
            if !is_valid_kind(kind) {
                return Err(Error::Failed(format!(
                    "invalid content type '{kind}' — expected [a-z0-9][a-z0-9._-]* with no '..'"
                )));
            }
        }
        if let Some(provider) = &self.provider {
            provider.check()?;
        }
        self.check_bins()?;
        self.check_sealed()?;
        Ok(())
    }

    /// Semantic checks for `sealed:` — the block's own rules plus the
    /// v1 incompatibilities (`include:`/`bins:`/`provider:` are
    /// meaningless over ciphertext).
    fn check_sealed(&self) -> Result<(), Error> {
        let Some(sealed) = &self.sealed else {
            return Ok(());
        };
        sealed.check()?;
        if !self.include.is_empty() {
            return Err(Error::Failed(
                "sealed packs cannot use include: — composition over ciphertext is meaningless"
                    .to_string(),
            ));
        }
        if !self.bins.is_empty() {
            return Err(Error::Failed(
                "sealed packs cannot declare bins: — bin deploys over ciphertext are meaningless"
                    .to_string(),
            ));
        }
        if self.provider.is_some() {
            return Err(Error::Failed(
                "sealed packs cannot declare provider: — service specs over ciphertext are meaningless"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Semantic checks for `bins[]` — name charset + uniqueness,
    /// exactly one of `shim`/`payload`, known shim kinds, a driver
    /// kind for shims, and a safe payload-relative path.
    fn check_bins(&self) -> Result<(), Error> {
        let mut seen = std::collections::BTreeSet::new();
        for bin in &self.bins {
            if !is_valid_name(&bin.name) || bin.name == "tamias" {
                return Err(Error::Failed(format!(
                    "invalid bin name '{}' — expected [A-Za-z0-9][A-Za-z0-9._-]*, not 'tamias'",
                    bin.name
                )));
            }
            if !seen.insert(bin.name.as_str()) {
                return Err(Error::Failed(format!("duplicate bin name '{}'", bin.name)));
            }
            match (&bin.shim, &bin.payload) {
                (Some(_), Some(_)) | (None, None) => {
                    return Err(Error::Failed(format!(
                        "bin '{}' must set exactly one of shim/payload",
                        bin.name
                    )));
                }
                (Some(shim), None) => {
                    if shim != "cli" {
                        return Err(Error::Failed(format!(
                            "bin '{}': unknown shim kind '{shim}' (only 'cli' exists)",
                            bin.name
                        )));
                    }
                    match &bin.driver {
                        Some(driver) if is_valid_kind(driver) => {}
                        _ => {
                            return Err(Error::Failed(format!(
                                "bin '{}': shim 'cli' requires a valid driver kind",
                                bin.name
                            )));
                        }
                    }
                }
                (None, Some(payload)) => {
                    check_bin_payload_path(&bin.name, payload)?;
                }
            }
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
    fn kind_validation() {
        for bad in [
            "../evil",
            "a/b",
            "a\\b",
            "..",
            ".",
            "-lead",
            ".lead",
            "_lead",
            "",
            "Upper",
            "has space",
            "a..b",
            "a\0b",
        ] {
            assert!(!is_valid_kind(bad), "kind {bad:?} must be rejected");
            assert!(
                Manifest {
                    name: "x".into(),
                    kind: Some(bad.into()),
                    ..Default::default()
                }
                .validate()
                .is_err(),
                "kind {bad:?} must fail validation"
            );
        }
        for good in [
            "files",
            "tamias",
            "peregrin-bundle",
            "myco-config",
            "kuda-domain",
            "mimus-rules",
            "type.v2_beta-1",
            "9lives",
        ] {
            assert!(is_valid_kind(good), "kind {good:?} must be accepted");
            assert!(
                Manifest {
                    name: "x".into(),
                    kind: Some(good.into()),
                    ..Default::default()
                }
                .validate()
                .is_ok(),
                "kind {good:?} must pass validation"
            );
        }
        // absent kind is fine
        assert!(Manifest {
            name: "x".into(),
            ..Default::default()
        }
        .validate()
        .is_ok());
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

    #[test]
    fn bins_parse_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tamias.yaml"),
            "name: x\nbins:\n  \
             - name: x-cli\n    shim: cli\n    driver: x\n  \
             - name: x-tool\n    payload: tools/bin/x86_64-unknown-linux-gnu/x-tool\n",
        )
        .unwrap();
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.bins.len(), 2);
        assert_eq!(m.bins[0].shim.as_deref(), Some("cli"));
        assert_eq!(m.bins[0].driver.as_deref(), Some("x"));
        assert_eq!(
            m.bins[1].payload.as_deref(),
            Some("tools/bin/x86_64-unknown-linux-gnu/x-tool")
        );

        let with_bins = |bins: Vec<BinDecl>| Manifest {
            name: "x".into(),
            bins,
            ..Default::default()
        };
        let shim = |name: &str| BinDecl {
            name: name.into(),
            shim: Some("cli".into()),
            driver: Some("x".into()),
            payload: None,
        };
        let payload = |name: &str, p: &str| BinDecl {
            name: name.into(),
            shim: None,
            driver: None,
            payload: Some(p.into()),
        };
        // happy paths
        assert!(with_bins(vec![shim("a"), payload("b", "tools/bin/t/a")])
            .validate()
            .is_ok());
        assert!(
            with_bins(vec![payload("b", "tools/bin/aarch64-apple-darwin/x/b")])
                .validate()
                .is_ok()
        );
        // name rules
        for bad in ["", "-x", "a b", "tamias", "../x"] {
            assert!(
                with_bins(vec![shim(bad)]).validate().is_err(),
                "bin name {bad:?} must fail"
            );
        }
        // exactly one of shim/payload
        assert!(with_bins(vec![BinDecl {
            name: "a".into(),
            shim: Some("cli".into()),
            driver: Some("x".into()),
            payload: Some("tools/bin/t/a".into()),
        }])
        .validate()
        .is_err());
        assert!(with_bins(vec![BinDecl {
            name: "a".into(),
            shim: None,
            driver: None,
            payload: None,
        }])
        .validate()
        .is_err());
        // unknown shim kind / missing driver
        assert!(with_bins(vec![BinDecl {
            name: "a".into(),
            shim: Some("mcp".into()),
            driver: Some("x".into()),
            payload: None,
        }])
        .validate()
        .is_err());
        assert!(with_bins(vec![BinDecl {
            name: "a".into(),
            shim: Some("cli".into()),
            driver: None,
            payload: None,
        }])
        .validate()
        .is_err());
        assert!(with_bins(vec![BinDecl {
            name: "a".into(),
            shim: Some("cli".into()),
            driver: Some("Bad Kind".into()),
            payload: None,
        }])
        .validate()
        .is_err());
        // payload path rules
        for bad in [
            "",
            "bin/x",
            "tools/x",
            "tools/bin",
            "tools/bin/",
            "tools//bin/x",
            "tools/bin/../x",
            "tools/bin/./x",
            "/tools/bin/x",
            "tools/bin/x/../../y",
            "tools\\bin\\x",
            "tools/bin/a\0b",
        ] {
            assert!(
                with_bins(vec![payload("a", bad)]).validate().is_err(),
                "payload {bad:?} must fail"
            );
        }
        // duplicate names
        assert!(with_bins(vec![shim("a"), shim("a")]).validate().is_err());
    }
}
