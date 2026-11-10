#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-registry-open — the shared registry-source layer.
//!
//! Resolves a registry value — a `registries` config **name** first,
//! then a raw dir/http ref — into a [`Registry`] impl, and owns the
//! config-file CRUD behind `tamias registry add|remove|get|list` and
//! the `tamias-serve` `registry.*` ops. Both `tamias-cli` and
//! `tamias-serve` consume this so name resolution, limit lookup, and
//! config writes can't diverge (same role `tamias-install` plays for
//! the install pipeline).

use std::path::{Path, PathBuf};

use tamias_config::{Config, ConfigFormat, Registry as ConfigRegistry, CONFIG_ENV};
use tamias_config_toml::TomlFormat;
use tamias_config_yml::YmlFormat;
use tamias_core::{payload_root, Error, Manifest};
use tamias_registry::{parse_registry_ref, Registry, RegistryEntry, RegistryRef};
use tamias_registry_cf::HttpRegistry;
use tamias_registry_local::LocalRegistry;

/// The effective byte limits for a publish/search target — `value`
/// doubles as the registry name when it matches a `registries` key
/// (raw dir/http refs simply miss the lookup and get global + package
/// rules). `None` config → built-in defaults.
#[must_use]
pub fn effective_limits(
    config: Option<&Config>,
    value: Option<&str>,
    package: Option<&str>,
) -> tamias_config::EffectiveLimits {
    config.map_or_else(
        || Config::default().effective_limits(value, package),
        |c| c.effective_limits(value, package),
    )
}

/// The `--registry`/`"registry"` value `publish`/`search` resolve:
/// explicit flag → `TAMIAS_REGISTRY` env (wins over config, mirroring
/// the store/cache env-wins rule) → the config routing defaults, in
/// order → `None` (the local default root). `config_defaults` is e.g.
/// `[search.default, publish.default]` for `search`.
#[must_use]
pub fn registry_target(flag: Option<&str>, config_defaults: &[Option<&str>]) -> Option<String> {
    flag.map(str::to_string)
        .or_else(|| {
            std::env::var("TAMIAS_REGISTRY")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .or_else(|| {
            config_defaults
                .iter()
                .flatten()
                .next()
                .map(|s| (*s).to_string())
        })
}

/// Open a registry: `value` is a `registries` config **name** first,
/// then a raw dir/http ref ([`parse_registry_ref`]). `None` → the
/// env / default local root at `registry_root`. Callers resolve the
/// routing defaults (`publish.default`/`search.default`,
/// `TAMIAS_REGISTRY` env) into `value` before calling — env wins over
/// config, flag over env.
///
/// `package` (the manifest name on publish) selects per-package limit
/// overrides; the resolved [`tamias_config::EffectiveLimits`] are handed
/// to the adapter (`with_max_upload`), which enforces them — the
/// packed-tarball size probe runs post-pack/pre-upload inside
/// `publish`, so an over-limit payload never reaches the wire or the
/// index.
pub fn open_registry(
    registry_root: &Path,
    config: Option<&Config>,
    value: Option<&str>,
    package: Option<&str>,
) -> Result<Box<dyn Registry>, Error> {
    let Some(value) = value else {
        return Ok(Box::new(LocalRegistry::new(registry_root).with_max_upload(
            effective_limits(config, None, package).max_upload,
        )));
    };
    if let Some(named) = config.and_then(|c| c.registries.get(value)) {
        return open_named_registry(registry_root, value, named, config, package);
    }
    let eff = effective_limits(config, Some(value), package);
    Ok(match parse_registry_ref(value) {
        RegistryRef::Local(path) => {
            Box::new(LocalRegistry::new(path).with_max_upload(eff.max_upload))
        }
        RegistryRef::Http(base) => {
            Box::new(http_registry(&base, None, value)?.with_max_upload(eff.max_upload))
        }
    })
}

/// Build the `Registry` impl for a `registries` entry — explicit
/// `kind` wins, url-scheme inference is the fallback
/// ([`tamias_config::Registry::effective_kind`]). `http`/`cf` share
/// the `HttpRegistry` client; auth is `token_env` (an env var *name*)
/// → `TAMIAS_REGISTRY_TOKEN` → none.
fn open_named_registry(
    registry_root: &Path,
    name: &str,
    reg: &ConfigRegistry,
    config: Option<&Config>,
    package: Option<&str>,
) -> Result<Box<dyn Registry>, Error> {
    let eff = effective_limits(config, Some(name), package);
    match reg.effective_kind().as_str() {
        "local" => {
            let root = reg
                .url
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| registry_root.to_path_buf());
            Ok(Box::new(
                LocalRegistry::new(root).with_max_upload(eff.max_upload),
            ))
        }
        "http" | "cf" => {
            let base = reg.url.clone().ok_or_else(|| {
                Error::Failed(format!("registry '{name}': kind http/cf needs a url"))
            })?;
            http_registry(&base, reg.token_env.as_deref(), name)
                .map(|r| Box::new(r.with_max_upload(eff.max_upload)) as Box<dyn Registry>)
        }
        other => {
            eprintln!(
                "tamias: warning — registry '{name}': unknown kind '{other}'; inferring from url"
            );
            match parse_registry_ref(reg.url.as_deref().unwrap_or_default()) {
                RegistryRef::Local(path) => Ok(Box::new(
                    LocalRegistry::new(path).with_max_upload(eff.max_upload),
                )),
                RegistryRef::Http(base) => http_registry(&base, reg.token_env.as_deref(), name)
                    .map(|r| Box::new(r.with_max_upload(eff.max_upload)) as Box<dyn Registry>),
            }
        }
    }
}

/// An http(s) registry with bearer auth — `token_env` names the env
/// var holding the token (the var's *name* lives in config; the value
/// is read at connect time, never stored). `TAMIAS_REGISTRY_TOKEN` is
/// the fallback when `token_env` is unset or its var is empty.
///
/// Returns the concrete [`HttpRegistry`] (not the trait object) so
/// callers can layer on the resolved upload cap.
fn http_registry(base: &str, token_env: Option<&str>, name: &str) -> Result<HttpRegistry, Error> {
    let base = base.trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(Error::Failed(format!(
            "registry '{name}': url '{base}' is not http(s)"
        )));
    }
    let token = token_env
        .and_then(|var| match std::env::var(var) {
            Ok(value) if !value.is_empty() => Some(value),
            _ => {
                eprintln!("tamias: warning — registry '{name}': ${var} is unset; no token");
                None
            }
        })
        .or_else(|| {
            std::env::var("TAMIAS_REGISTRY_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
        });
    let mut registry = HttpRegistry::new(base);
    if let Some(token) = token {
        registry = registry.with_token(token);
    }
    Ok(registry)
}

/// `search <query> [registry]` — the shared routing + open + query
/// behind `tamias search`, `tamias serve`'s `search` op, and `tamias
/// ui`'s Registry pane: `registry_target(flag, [search.default,
/// publish.default])` → `open_registry` → `registry.search`. Returns
/// `(results, resolved_target)` — `None` target means the local
/// default root.
///
/// # Errors
/// Fails on registry resolution or the search itself.
pub fn search(
    registry_root: &Path,
    config: Option<&Config>,
    flag: Option<&str>,
    query: &str,
) -> Result<(Vec<RegistryEntry>, Option<String>), Error> {
    let target = registry_target(
        flag,
        &[
            config.and_then(|c| c.search.default.as_deref()),
            config.and_then(|c| c.publish.default.as_deref()),
        ],
    );
    let registry = open_registry(registry_root, config, target.as_deref(), None)?;
    let results = registry.search(query)?;
    Ok((results, target))
}

/// A resolved `publish <dir> [registry]` — the payload root, loaded
/// manifest, routed target, and opened registry (named-registry +
/// per-package limits applied). Surfaces render their own progress
/// around [`publish_plan`] and finish with [`PublishPlan::publish`].
///
/// When the manifest carries `include:` and `keep_includes` is false,
/// [`publish_plan`] flattens bases read-only via
/// [`tamias_publish::compose`] — `root` then points at the *composed*
/// scratch payload (kept alive by `_composed`) so signing and packing
/// both cover the flattened tree. The manifest is unchanged —
/// `include:` stays as provenance.
pub struct PublishPlan {
    /// `payload_root(dir)` — or the composed scratch dir when
    /// `include:` was flattened. Holds `tamias.yaml` either way.
    pub root: PathBuf,
    /// The validated package manifest (unmodified — `include:` kept).
    pub manifest: Manifest,
    /// The opened registry — [`PublishPlan::publish`] goes through it.
    pub registry: Box<dyn Registry>,
    /// The resolved target (`--registry` flag → `TAMIAS_REGISTRY` →
    /// `config_defaults`); `None` → the local default root.
    pub target: Option<String>,
    /// Keeps the flatten scratch dir alive for the plan's lifetime.
    _composed: Option<tamias_publish::Composed>,
}

impl PublishPlan {
    /// Upload the packed payload — `registry.publish(&root, &manifest,
    /// signature)`. `signature` is the armored `ssh-keygen -Y` signature
    /// over the payload's provenance anchors (`tamias publish --sign`);
    /// `None` publishes unsigned.
    ///
    /// # Errors
    /// Propagates pack/publish failures from the registry impl.
    pub fn publish(&self, signature: Option<&str>) -> Result<RegistryEntry, Error> {
        self.registry.publish(&self.root, &self.manifest, signature)
    }
}

/// `publish <dir> [registry]` resolution — the shared head of
/// `tamias publish`, `tamias serve`'s `publish` op, and `tamias ui`'s
/// Author pane: `payload_root` → `Manifest::load` → `include:`
/// flatten (unless `keep_includes`) → `registry_target` →
/// `open_registry`. `store_root` is the install store the flatten
/// resolves bases against — read-only, publish never installs.
/// `report` fires after the manifest loads and the target resolves but
/// *before* the registry opens, so each surface keeps its own
/// "publishing …" line in its own position (the CLI prints post-open;
/// the TUI logs pre-open).
///
/// # Errors
/// Fails on a missing/invalid manifest, an unresolvable `include:`
/// base, or registry resolution.
#[allow(clippy::too_many_arguments)]
pub fn publish_plan(
    dir: &Path,
    registry_root: &Path,
    config: Option<&Config>,
    flag: Option<&str>,
    config_defaults: &[Option<&str>],
    store_root: &Path,
    keep_includes: bool,
    report: &mut dyn FnMut(&Manifest, Option<&str>),
) -> Result<PublishPlan, Error> {
    let root = payload_root(dir)?;
    let manifest = Manifest::load(&root)?;
    let target = registry_target(flag, config_defaults);
    report(&manifest, target.as_deref());
    // Flatten `include:` bases read-only — the composed payload is
    // what gets signed and packed; `root` swaps to the scratch dir.
    let (root, composed) = if keep_includes || manifest.include.is_empty() {
        (root, None)
    } else {
        let composed = tamias_publish::compose(&root, store_root)?;
        (composed.dir.clone(), Some(composed))
    };
    let registry = open_registry(
        registry_root,
        config,
        target.as_deref(),
        Some(&manifest.name),
    )?;
    Ok(PublishPlan {
        root,
        manifest,
        registry,
        target,
        _composed: composed,
    })
}

/// The config file registry management verbs read/write — `file`,
/// then `TAMIAS_CONFIG`, then `./tamias.{yml,yaml,toml}` discovery. No
/// file anywhere → `./tamias.yml` + defaults (created on write).
pub fn config_target(file: Option<&Path>) -> Result<(PathBuf, Config), Error> {
    let formats: [&dyn ConfigFormat; 2] = [&YmlFormat, &TomlFormat];
    let explicit = file
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(CONFIG_ENV).map(PathBuf::from));
    if let Some(path) = explicit {
        let config = if path.is_file() {
            tamias_config::load_file(&path, &formats)?
        } else {
            Config::default()
        };
        return Ok((path, config));
    }
    Ok(match tamias_config::load_in(Path::new("."), &formats)? {
        Some(found) => found,
        None => (PathBuf::from("tamias.yml"), Config::default()),
    })
}

/// The effective config for read-side resolution (`search`/`publish`
/// routing, `install`'s download cap): `file` → `TAMIAS_CONFIG` → cwd
/// discovery → defaults when no file exists. Unlike the CLI's
/// warn-and-ignore reader this propagates parse errors — callers that
/// want CLI semantics catch and warn.
pub fn load_config(file: Option<&Path>) -> Result<Config, Error> {
    Ok(config_target(file)?.1)
}

/// Render `config` back to `path`'s format and write it — the only
/// write path for `add_registry`/`remove_registry`.
fn write_config(path: &Path, config: &Config) -> Result<(), Error> {
    let formats: [&dyn ConfigFormat; 2] = [&YmlFormat, &TomlFormat];
    let format = tamias_config::format_for(path, &formats)
        .ok_or_else(|| Error::Failed(format!("no config format for {}", path.display())))?;
    let text = format.render(config)?;
    std::fs::write(path, text).map_err(Error::io(format!("write {}", path.display())))
}

/// One `registries` entry as a JSON object — the `registry.list`/`get`
/// wire shape. `token_env` is the env var *name*; the token value is
/// never read here, let alone serialized.
#[must_use]
pub fn registry_json(name: &str, reg: &ConfigRegistry) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "kind": reg.kind,
        "effective_kind": reg.effective_kind(),
        "url": reg.url,
        "token_env": reg.token_env,
        "limits": reg.limits,
    })
}

/// Validate a `registry add --kind` value — `local` | `http` | `cf`
/// (case-insensitive), canonicalized to the lowercase spelling stored
/// in config.
fn registry_kind(kind: &str) -> Result<&'static str, Error> {
    match kind.to_lowercase().as_str() {
        "local" => Ok("local"),
        "http" => Ok("http"),
        "cf" => Ok("cf"),
        other => Err(Error::Failed(format!(
            "unknown registry kind '{other}' (expected local, http, or cf)"
        ))),
    }
}

/// `registry add` — validate kind/url, refuse duplicates, render the
/// config back through its own format. Returns the config path
/// written. http/cf need an http(s) url; local takes a dir (or
/// anything — the explicit kind wins over url-scheme inference at
/// resolve time).
///
/// # Errors
/// Fails on an unknown kind, a non-http(s) url for http/cf, a
/// duplicate name, or a config read/write error.
pub fn add_registry(
    file: Option<&Path>,
    name: &str,
    kind: &str,
    url: &str,
    token_env: Option<String>,
) -> Result<PathBuf, Error> {
    let kind = registry_kind(kind)?;
    if kind != "local" {
        match parse_registry_ref(url) {
            RegistryRef::Http(_) => {}
            RegistryRef::Local(_) => {
                return Err(Error::Failed(format!(
                    "registry '{name}': kind {kind} needs an http(s) url, got '{url}'"
                )));
            }
        }
    }
    let (path, mut config) = config_target(file)?;
    if config.registries.contains_key(name) {
        return Err(Error::Failed(format!("registry '{name}' already exists")));
    }
    config.registries.insert(
        name.to_string(),
        ConfigRegistry {
            kind: Some(kind.to_string()),
            url: Some(url.to_string()),
            token_env,
            limits: Default::default(),
        },
    );
    write_config(&path, &config)?;
    Ok(path)
}

/// `registry remove` — drop a named registry; unknown names error.
/// Returns the config path written.
///
/// # Errors
/// Fails when `name` isn't configured or the config can't be written.
pub fn remove_registry(file: Option<&Path>, name: &str) -> Result<PathBuf, Error> {
    let (path, mut config) = config_target(file)?;
    if config.registries.remove(name).is_none() {
        return Err(Error::Failed(format!("registry '{name}' not found")));
    }
    write_config(&path, &config)?;
    Ok(path)
}

/// `registry get` — one named registry's config entry.
///
/// # Errors
/// Fails when `name` isn't configured or the config can't be read.
pub fn get_registry(file: Option<&Path>, name: &str) -> Result<ConfigRegistry, Error> {
    let (_path, config) = config_target(file)?;
    config
        .registries
        .get(name)
        .cloned()
        .ok_or_else(|| Error::Failed(format!("registry '{name}' not found")))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn add_get_list_remove_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("tamias.yml");
        // add creates the file (defaults + the new entry)
        let path = add_registry(
            Some(&file),
            "mine",
            "local",
            "/tmp/reg",
            Some("MY_TOKEN".into()),
        )
        .unwrap();
        assert_eq!(path, file);
        assert!(file.is_file());
        // duplicate name errors
        assert!(add_registry(Some(&file), "mine", "local", "/x", None).is_err());
        // get returns the entry; list sees it; token_env is a name only
        let reg = get_registry(Some(&file), "mine").unwrap();
        assert_eq!(reg.effective_kind(), "local");
        assert_eq!(reg.token_env.as_deref(), Some("MY_TOKEN"));
        let (_p, config) = config_target(Some(&file)).unwrap();
        assert_eq!(config.registries.len(), 1);
        let json = registry_json("mine", &reg);
        assert_eq!(json["name"], "mine");
        assert_eq!(json["token_env"], "MY_TOKEN");
        // remove drops it; removing again errors
        remove_registry(Some(&file), "mine").unwrap();
        assert!(get_registry(Some(&file), "mine").is_err());
        assert!(remove_registry(Some(&file), "mine").is_err());
    }

    #[test]
    fn add_validates_kind_and_url() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("tamias.yml");
        assert!(add_registry(Some(&file), "b", "bogus", "/x", None).is_err());
        assert!(add_registry(Some(&file), "b", "http", "/not-http", None).is_err());
        assert!(add_registry(Some(&file), "b", "HTTP", "http://h/r", None).is_ok());
        // local accepts a bare dir
        assert!(add_registry(Some(&file), "l", "local", "/tmp/reg", None).is_ok());
    }

    #[test]
    fn open_registry_resolves_name_then_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_root = tmp.path().join("reg");
        let mut config = Config::default();
        config.registries.insert(
            "mine".into(),
            ConfigRegistry {
                kind: Some("local".into()),
                url: Some(reg_root.display().to_string()),
                ..Default::default()
            },
        );
        // by name
        let r = open_registry(&reg_root, Some(&config), Some("mine"), None).unwrap();
        assert!(r.entries().unwrap().is_empty());
        // raw local ref
        let raw = reg_root.display().to_string();
        let r = open_registry(&reg_root, Some(&config), Some(&raw), None).unwrap();
        assert!(r.entries().unwrap().is_empty());
        // unknown name falls through to raw-ref parse → local dir
        let r = open_registry(&reg_root, Some(&config), Some("/nonexistent-xyz"), None).unwrap();
        // entries() on a missing dir errors or is empty — either way open succeeded
        let _ = r.entries();
        // None → default root
        let r = open_registry(&reg_root, None, None, None).unwrap();
        let _ = r.entries();
    }

    #[test]
    fn registry_target_order() {
        // env var may be set by the environment — only assert the
        // flag-wins and config-fallback arms that don't depend on it.
        assert_eq!(
            registry_target(Some("flag"), &[Some("cfg")]).as_deref(),
            Some("flag")
        );
        if std::env::var_os("TAMIAS_REGISTRY").is_none() {
            assert_eq!(
                registry_target(None, &[Some("cfg")]).as_deref(),
                Some("cfg")
            );
            assert_eq!(
                registry_target(None, &[None, Some("pub")]).as_deref(),
                Some("pub")
            );
            assert_eq!(registry_target(None, &[None]), None);
        }
    }

    #[test]
    fn load_config_defaults_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        // explicit missing file → defaults (created on write)
        let missing = tmp.path().join("nope.yml");
        assert_eq!(load_config(Some(&missing)).unwrap(), Config::default());
        // written file round-trips
        let file = tmp.path().join("tamias.yml");
        add_registry(Some(&file), "r", "local", "/x", None).unwrap();
        assert!(load_config(Some(&file))
            .unwrap()
            .registries
            .contains_key("r"));
    }
}
