#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source-registry — the **registry-scoped install ref** kind:
//! `<label>:<name>@<version>` (and `<label>:@scope/<name>@<version>`
//! for scoped packages).
//!
//! `<label>` resolves against the *effective* config's `registries`
//! map — an entry's `id` (uuid) first, then its map key (slug) — via
//! [`tamias_registry_open::load_config`] (user-level `~/.config/tamias`
//! layered under the cwd `tamias.*`). Only `http`/`cf` kind registries
//! are installable; `local` (unserved dir) registries error with a
//! pointer at `tamias registry serve`.
//!
//! The kind parses straight into a [`CfSpec`] — the same spec the
//! `cf:` arm produces — so plan/fetch (index lookup → digest +
//! signature verification → tarball fetch) runs through `CfSource`
//! unchanged, and provenance records the verbatim `<label>:…` ref.
//!
//! `claims` is deliberately cheap — no config IO: any
//! `<slug>:`-shaped prefix that isn't a reserved kind prefix is
//! claimed, and `parse` does the config lookup (unknown label → hard
//! error, never an fs-path fallback).

use tamias_config::{Config, Registry as ConfigRegistry};
use tamias_core::Error;
use tamias_registry_open::load_config;
use tamias_source::{RefKind, Spec};
use tamias_source_cf::CfSpec;

/// Ref-kind prefixes that always win over a registry label — mirrors
/// the claims of the kinds registered before this one (`git:`, `cf:`,
/// `http://`, `https://`) plus the scheme spellings that must never be
/// misread as a label (`http:`, `ssh:`, `fs:`, `file:`).
const RESERVED_LABELS: &[&str] = &["git", "cf", "http", "https", "ssh", "fs", "file"];

/// The `<registry>:<name>[@<version>]` ref kind.
///
/// [`RegistryRefKind::new`] resolves the effective config at parse
/// time (`load_config`); [`RegistryRefKind::with_config`] pins a config
/// for tests and embedders.
#[derive(Clone, Debug, Default)]
pub struct RegistryRefKind {
    config: Option<Config>,
}

impl RegistryRefKind {
    /// Resolve registries through the effective layered config.
    #[must_use]
    pub fn new() -> Self {
        Self { config: None }
    }

    /// Resolve registries through `config` instead of discovery.
    #[must_use]
    pub fn with_config(config: Config) -> Self {
        Self {
            config: Some(config),
        }
    }
}

impl RefKind for RegistryRefKind {
    /// Cheap shape check — no config IO. Claims any `<slug>:` prefix
    /// (slug = `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, `:` before any `/`, longer
    /// than one char so Windows drive letters like `C:\…` can't
    /// false-positive) that isn't a reserved kind prefix.
    fn claims(&self, input: &str) -> bool {
        let Some(colon) = input.find(':') else {
            return false;
        };
        let label = &input[..colon];
        // A '/' before the ':' means the colon sits inside a path —
        // `a/b:c` is not `<label>:<rest>`.
        if label.contains('/') || label.len() < 2 || !valid_label(label) {
            return false;
        }
        !RESERVED_LABELS.contains(&label.to_ascii_lowercase().as_str())
    }

    /// Parse `<label>:<name>@<version>` (or `<label>:@scope/<name>@<version>`)
    /// into a [`CfSpec`] — the config lookup happens here, so an
    /// unknown label is a hard error.
    ///
    /// # Errors
    /// Fails on a missing `@<version>`, a malformed name, an unknown
    /// registry label, a `local`-kind registry, or a registry without
    /// a url.
    fn parse(&self, input: &str) -> Result<Spec, Error> {
        let (label, rest) = input
            .split_once(':')
            .ok_or_else(|| Error::Failed(format!("bad registry ref '{input}'")))?;
        let bad = || {
            Error::Failed(format!(
                "bad registry ref '{input}' — expected <registry>:<name>@<version>"
            ))
        };
        // `@<version>` is required for v1 — a bare name never silently
        // resolves "latest".
        let (name_part, version) = rest.rsplit_once('@').ok_or_else(bad)?;
        if version.is_empty() || name_part.is_empty() {
            return Err(bad());
        }
        // `@scope/<name>` keeps the npm-style scoped spelling; anything
        // else is a bare name. A stray '@' inside either part is
        // malformed.
        let (scope, name) = match name_part.strip_prefix('@') {
            Some(scoped) => {
                let (scope, name) = scoped.split_once('/').ok_or_else(bad)?;
                (Some(scope), name)
            }
            None => (None, name_part),
        };
        if name.is_empty()
            || scope.is_some_and(str::is_empty)
            || name.contains('@')
            || scope.is_some_and(|s| s.contains('@'))
        {
            return Err(bad());
        }
        let config = match &self.config {
            Some(config) => config.clone(),
            None => load_config(None)?,
        };
        let registry = find_registry(&config, label)
            .ok_or_else(|| Error::Failed(format!("unknown registry '{label}'")))?;
        let kind = registry.effective_kind();
        match kind.as_str() {
            "http" | "cf" => {}
            "local" => {
                return Err(Error::Failed(format!(
                    "registry '{label}' is kind local — serve it with \
                     `tamias registry serve` or use a cf:/http: ref"
                )));
            }
            other => {
                return Err(Error::Failed(format!(
                    "registry '{label}': unknown kind '{other}' (expected http or cf)"
                )));
            }
        }
        let base_url = registry
            .url
            .clone()
            .ok_or_else(|| Error::Failed(format!("registry '{label}': kind {kind} needs a url")))?;
        Ok(Box::new(CfSpec {
            base_url,
            address: label.to_string(),
            name: name.to_string(),
            scope: scope.map(str::to_string),
            version: version.to_string(),
            token_env: registry.token_env.clone(),
        }))
    }
}

/// `<slug>` charset — `[a-zA-Z0-9][a-zA-Z0-9_.-]*` (the same shape
/// `claims` enforces).
fn valid_label(label: &str) -> bool {
    let mut chars = label.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Resolve `label` against the effective config — `id` (uuid) first,
/// then the `registries` map key (slug). Id wins on collision.
fn find_registry<'a>(config: &'a Config, label: &str) -> Option<&'a ConfigRegistry> {
    config
        .registries
        .values()
        .find(|reg| reg.id.as_deref() == Some(label))
        .or_else(|| config.registries.get(label))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn config_with(entries: &[(&str, ConfigRegistry)]) -> Config {
        let mut config = Config::default();
        for (name, reg) in entries {
            config.registries.insert((*name).to_string(), reg.clone());
        }
        config
    }

    fn http_registry(url: &str) -> ConfigRegistry {
        ConfigRegistry {
            kind: Some("http".into()),
            url: Some(url.into()),
            ..Default::default()
        }
    }

    #[test]
    fn claims_shape_without_io() {
        let kind = RegistryRefKind::new();
        assert!(kind.claims("9aia:cicada-demo@0.1.0"));
        assert!(kind.claims("9aia:@scope/pkg@1.0.0"));
        assert!(kind.claims("my-reg_1.2:x@1"));
        // reserved prefixes are never registry labels
        for input in [
            "git:x",
            "cf:a/b/x@1",
            "http://x/y.tgz",
            "https://x/y.tgz",
            "ssh:host/x",
            "fs:/x",
            "file:///x",
            "HTTP://x",
        ] {
            assert!(!kind.claims(input), "{input}");
        }
        // Windows drive letters + single-char labels can't false-positive
        assert!(!kind.claims("C:\\pkg"));
        assert!(!kind.claims("C:/pkg"));
        assert!(!kind.claims("c:x@1"));
        // no colon / colon after a slash / bad slug charset
        assert!(!kind.claims("./x:y"));
        assert!(!kind.claims("a/b:c@1"));
        assert!(!kind.claims("-bad:x@1"));
        assert!(!kind.claims(".bad:x@1"));
        assert!(!kind.claims("no-colon"));
        assert!(!kind.claims("x"));
    }

    #[test]
    fn parse_resolves_by_name_and_id() {
        let config = config_with(&[
            ("9aia", http_registry("https://reg.example")),
            (
                "renamed",
                ConfigRegistry {
                    id: Some("018f3c2e-7b1a-7c4d-9e5f-2a6b8c0d1e2f".into()),
                    ..http_registry("https://id.example")
                },
            ),
        ]);
        let kind = RegistryRefKind::with_config(config);

        // by slug
        let spec = kind.parse("9aia:cicada-demo@0.1.0").unwrap();
        let cf = spec.downcast::<CfSpec>().unwrap();
        assert_eq!(cf.base_url, "https://reg.example");
        assert_eq!(cf.address, "9aia");
        assert_eq!(cf.name, "cicada-demo");
        assert_eq!(cf.version, "0.1.0");
        assert_eq!(cf.scope, None);
        assert_eq!(cf.token_env, None);

        // by id — and id wins over a slug collision
        let spec = kind
            .parse("018f3c2e-7b1a-7c4d-9e5f-2a6b8c0d1e2f:@scope/pkg@1.0.0")
            .unwrap();
        let cf = spec.downcast::<CfSpec>().unwrap();
        assert_eq!(cf.base_url, "https://id.example");
        assert_eq!(cf.name, "pkg");
        assert_eq!(cf.scope.as_deref(), Some("scope"));
    }

    #[test]
    fn id_beats_slug_on_collision() {
        // "other" carries id "shared" — `shared:` resolves by id to
        // "other", not by map key to the "shared" entry.
        let config = config_with(&[
            ("shared", http_registry("https://by-name.example")),
            (
                "other",
                ConfigRegistry {
                    id: Some("shared".into()),
                    ..http_registry("https://by-id.example")
                },
            ),
        ]);
        let kind = RegistryRefKind::with_config(config);
        let spec = kind.parse("shared:x@1.0.0").unwrap();
        let cf = spec.downcast::<CfSpec>().unwrap();
        assert_eq!(cf.base_url, "https://by-id.example");
    }

    #[test]
    fn unknown_label_and_bad_shapes_error() {
        let config = config_with(&[("9aia", http_registry("https://reg.example"))]);
        let kind = RegistryRefKind::with_config(config);
        let err = kind.parse("nope:x@1.0.0").unwrap_err();
        assert!(err.to_string().contains("unknown registry 'nope'"), "{err}");
        // bare name — @<version> is required
        assert!(kind.parse("9aia:cicada-demo").is_err());
        assert!(kind.parse("9aia:@scope/pkg").is_err());
        assert!(kind.parse("9aia:x@").is_err());
        assert!(kind.parse("9aia:@1.0.0").is_err());
        assert!(kind.parse("9aia:a@b@1.0.0").is_err());
        assert!(kind.parse("9aia:@scope").is_err());
    }

    #[test]
    fn local_kind_registry_errors() {
        let mut reg = http_registry("/tmp/reg");
        reg.kind = Some("local".into());
        reg.url = Some("/tmp/reg".into());
        let config = config_with(&[("mine", reg)]);
        let kind = RegistryRefKind::with_config(config);
        let err = kind.parse("mine:x@1.0.0").unwrap_err();
        assert!(err.to_string().contains("kind local"), "{err}");
        assert!(err.to_string().contains("registry serve"), "{err}");
    }

    #[test]
    fn token_env_and_kind_cf_pass_through() {
        let mut reg = ConfigRegistry {
            kind: Some("cf".into()),
            url: Some("https://reg.example".into()),
            token_env: Some("MY_REGISTRY_TOKEN".into()),
            ..Default::default()
        };
        reg.id = None;
        let config = config_with(&[("cfreg", reg)]);
        let kind = RegistryRefKind::with_config(config);
        let spec = kind.parse("cfreg:x@1.0.0").unwrap();
        let cf = spec.downcast::<CfSpec>().unwrap();
        assert_eq!(cf.token_env.as_deref(), Some("MY_REGISTRY_TOKEN"));
        assert_eq!(cf.base_url, "https://reg.example");
        // http-kind inference from an http(s) url with no explicit kind
        let config = config_with(&[(
            "inferred",
            ConfigRegistry {
                url: Some("https://inferred.example".into()),
                ..Default::default()
            },
        )]);
        let kind = RegistryRefKind::with_config(config);
        let spec = kind.parse("inferred:x@1.0.0").unwrap();
        assert_eq!(
            spec.downcast::<CfSpec>().unwrap().base_url,
            "https://inferred.example"
        );
    }

    #[test]
    fn parse_output_plans_through_cf_source() {
        use std::sync::Arc;
        use tamias_source::Sources;
        use tamias_source_cf::CfSource;

        let config = config_with(&[("9aia", http_registry("http://127.0.0.1:1"))]);
        let kind = RegistryRefKind::with_config(config);
        let spec = kind.parse("9aia:x@1.0.0").unwrap();
        // the spec IS a CfSpec — CfSource accepts it (plan fails here:
        // nothing listens on :1, proving the delegation path)
        let sources = Sources::new().with(Arc::new(CfSource::new()));
        assert!(sources.plan(&*spec).is_err());
    }
}
