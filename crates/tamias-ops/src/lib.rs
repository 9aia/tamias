//! tamias-ops — the I/O shell every front-end calls.
//!
//! `tamias-core` is pure domain; this crate is everything that touches
//! the outside world: config loading + env/vault resolution
//! (`--env`/`TAMIAS_ENV` → `[envs]` routing → a concrete
//! [`tamias_vault::Vault`]), the env-pin override on refs, the verb
//! operations the CLI calls, and the audit ndjson emission on
//! stderr. `tamias` (CLI) is a thin clap shell over this; `tamias-tui`
//! calls the same functions.

pub mod audit;
mod error;

pub use audit::{AuditEvent, Outcome, PolicyVerdict};
pub use error::OpsError;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use age::x25519;
use tamias_config::ConfigSource;
use tamias_config_toml::TomlSource;
use tamias_core::{is_valid_env_var, Actor, Config, Env, Op, Ref, Secret, VaultConfig};
use tamias_deploy::Deploy;
use tamias_deploy_cf::CfSecretsStore;
use tamias_identity::{IdentityError, IdentitySource};
use tamias_identity_file::FileIdentity;
#[cfg(not(coverage))]
use tamias_identity_keyring::KeyringIdentity;
use tamias_vault::{SecretRecord, Vault};
use tamias_vault_age::AgeVault;
use tamias_vault_cached::{CachePolicy, CachedVault};
use tamias_vault_cf::{Auth as CfAuth, CfVault, CfVaultConfig};

/// Knobs for [`Ops::load`] — the global CLI flags, resolved.
#[derive(Debug, Default)]
pub struct LoadOpts {
    /// `--config` path (explicit → errors propagate).
    pub config_path: Option<String>,
    /// `--env` name (else `TAMIAS_ENV`, else `dev`).
    pub env: Option<String>,
    /// `--actor` attribution (else `TAMIAS_ACTOR`, `$USER`, `anon`).
    pub actor: Option<String>,
}

/// Knobs for [`Ops::init`] — the `init` verb's flags, resolved.
#[derive(Debug, Default)]
pub struct InitOpts {
    /// `--with-file` — force the identity keyfile even when a
    /// session keyring is available (the headless/CI mode).
    pub with_file: bool,
}

/// What `init` reports back.
#[derive(Debug)]
pub struct InitInfo {
    /// The vault file that backs the selected env.
    pub vault_path: PathBuf,
    /// Where the identity lives — a keyfile path or a keyring
    /// locator. Never key material.
    pub identity: String,
    /// The identity backend — `"keyring"` or `"file"`.
    pub identity_backend: &'static str,
    /// `true` when a fresh identity was generated.
    pub identity_created: bool,
    /// `true` when an existing keyfile identity was imported into
    /// the keyring (the migration path).
    pub migrated: bool,
    /// The keyfile left behind after a migration — kept as a
    /// backup; the human deletes it when ready.
    pub file_remains: Option<PathBuf>,
    /// The identity's public recipient (`age1…`).
    pub recipient: String,
}

/// What `check` reports about one vault — the backend is
/// decryptable/readable, or why not.
#[derive(Debug)]
pub struct VaultCheck {
    /// Vault name from `[vaults]` (or the implicit `local`).
    pub name: String,
    /// Backend kind (`age`; anything else is a problem).
    pub kind: String,
    /// Vault file path for file-backed impls (`None` when the kind
    /// is unknown or unset).
    pub path: Option<PathBuf>,
    /// The vault opened (identity present, ciphertext decrypts).
    pub ok: bool,
    /// Why it didn't (`None` when `ok`).
    pub error: Option<String>,
}

/// What `deploy` reports back.
#[derive(Debug)]
pub struct DeployReport {
    /// The `[deploy.<name>]` target that received the value.
    pub target: String,
    /// The env the ref resolved under.
    pub env: Env,
    /// The resolved ref (env pin stripped).
    pub rref: Ref,
    /// The sink-side name the value was written under.
    pub secret_name: String,
    /// `true` when the sink created a new entry (else updated).
    pub created: bool,
}

/// What `migrate --private-refs` rewrote in one env's keyspace.
#[derive(Debug)]
pub struct MigrateReport {
    /// The `[vaults]` name the env routes to.
    pub vault: String,
    /// The env whose rows were rewritten.
    pub env: Env,
    /// Refs re-keyed (plaintext key → derived ref_key).
    pub rewritten: Vec<Ref>,
    /// Records that already carried a `name` blob (skipped).
    pub already_private: usize,
}

/// What `check` reports about one declared `[secrets]` entry.
#[derive(Debug)]
pub struct SecretCheck {
    /// The env-var name.
    pub var: String,
    /// The declared `tamias://` ref, env pin stripped if present.
    pub rref: String,
    /// The env it resolves under (pin-aware).
    pub env: String,
    /// A record exists at that `(env, ref)`.
    pub resolves: bool,
}

/// What `check` reports back.
#[derive(Debug)]
pub struct CheckReport {
    /// Where config was loaded from (`None` = defaults).
    pub config_path: Option<PathBuf>,
    /// The selected env and the vault it routes to.
    pub env: Env,
    /// Vault name `env` routes to.
    pub vault: String,
    /// Every configured or routed vault, with readability status.
    pub vaults: Vec<VaultCheck>,
    /// Every declared `[secrets]` entry, with resolution status.
    pub secrets: Vec<SecretCheck>,
    /// The configured guardrails table, if any.
    pub guardrails: Option<tamias_core::Guardrails>,
    /// Findings — non-empty means "problems to fix" (exit 1).
    pub problems: Vec<String>,
}

/// Loaded config + selected env + actor — the handle verbs run on.
pub struct Ops {
    config: Config,
    config_path: Option<PathBuf>,
    env: Env,
    actor: Actor,
    /// The PDP verdict attached to every audit event this handle
    /// emits — set by `tamias serve` per request via [`Self::scoped`].
    policy: Option<PolicyVerdict>,
}

impl Ops {
    /// Load config, resolve env + actor.
    ///
    /// Config search: explicit path → `TAMIAS_CONFIG` → `./tamias.toml`
    /// → `~/.config/tamias/tamias.toml`. No file on the default search
    /// means defaults (local vault, `dev`) — tamias works without a
    /// config; an explicit `--config`/`TAMIAS_CONFIG` that fails is an
    /// error, as is a file that fails to parse.
    pub fn load(opts: LoadOpts) -> Result<Self, OpsError> {
        let src = TomlSource {
            path: opts.config_path.clone(),
        };
        let located = src.locate();
        let config = match &located {
            Some(_) => src.load().map_err(OpsError::Config)?,
            None if opts.config_path.is_some() => {
                return Err(OpsError::Config(
                    // `locate()` found nothing — a load can't succeed.
                    src.load().err().unwrap_or("unreadable".into()),
                ));
            }
            None => Config::default(),
        };
        let env = resolve_env(opts.env.as_deref())?;
        let actor = resolve_actor(opts.actor.as_deref())?;
        Ok(Ops {
            config,
            config_path: located,
            env,
            actor,
            policy: None,
        })
    }

    /// The loaded config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Where config was loaded from, if anywhere.
    pub fn config_path(&self) -> Option<&PathBuf> {
        self.config_path.as_ref()
    }

    /// The selected env context.
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// The acting identity (attribution).
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// A per-request view of this handle — `tamias serve` overrides
    /// actor and env per ndjson request without reloading config,
    /// and attaches the PDP verdict so every event the scoped
    /// handle emits carries the policy context behind it.
    /// `None` keeps the loaded value.
    pub fn scoped(
        &self,
        actor: Option<Actor>,
        env: Option<Env>,
        policy: Option<PolicyVerdict>,
    ) -> Self {
        Ops {
            config: self.config.clone(),
            config_path: self.config_path.clone(),
            env: env.unwrap_or_else(|| self.env.clone()),
            actor: actor.unwrap_or_else(|| self.actor.clone()),
            policy,
        }
    }

    /// Attach (or clear) the PDP verdict — serve sets it after the
    /// consult so the op's audit events carry the policy context.
    pub fn set_policy(&mut self, policy: Option<PolicyVerdict>) {
        self.policy = policy;
    }

    /// Apply the env-pin rule: `tamias://prod/x` resolves under `prod`
    /// regardless of context; anything else uses the selected env.
    /// Returns `(env, ref-with-pin-stripped)`.
    pub fn resolve(&self, rref: &Ref) -> (Env, Ref) {
        match rref.env_pin(|s| self.config.is_known_env(s)) {
            Some((env, rest)) => (env, rest),
            None => (self.env.clone(), rref.clone()),
        }
    }

    /// Open the vault an env routes to. `local` (the implicit
    /// default) is always the age backend.
    pub fn open_vault(&self, env: &Env) -> Result<Box<dyn Vault>, OpsError> {
        self.open_vault_named(self.config.vault_for(env))
    }

    /// Open a vault by `[vaults]` name (`local` resolves to its
    /// implicit defaults when unconfigured).
    pub fn open_vault_named(&self, name: &str) -> Result<Box<dyn Vault>, OpsError> {
        let cfg = self.config.vault_config(name);
        let kind = cfg.map(|c| c.kind.as_str()).unwrap_or("age");
        match kind {
            "age" => {
                let vault_path = cfg
                    .and_then(|c| c.path.as_ref())
                    .map(PathBuf::from)
                    .unwrap_or_else(AgeVault::default_vault_path);
                let recipients = cfg.map(|c| c.recipients.as_slice()).unwrap_or(&[]);
                let identity = self.resolve_identity(cfg)?;
                let v = AgeVault::open(&vault_path, identity, recipients)?;
                self.wrap_cache(name, cfg, Box::new(v))
            }
            "cf" => {
                // `kind` was read off `cfg` — "cf" here means the
                // vault is configured, so this expect documents the
                // invariant rather than guarding a reachable path.
                #[allow(clippy::unwrap_used)]
                let c = cfg.expect("kind cf implies a configured vault");
                let v = self.open_cf_vault(name, c)?;
                self.wrap_cache(name, cfg, Box::new(v))
            }
            other => Err(OpsError::Config(format!(
                "vault {name:?} has unknown kind {other:?} (have: age, cf)"
            ))),
        }
    }

    /// Wrap `inner` in the read-through local cache when
    /// `[vaults.<name>] cache` scopes it on (P-007). The cache is an
    /// age vault at `$XDG_CACHE_HOME/tamias/<name>.age`, encrypted to
    /// the same identity + recipients as the vault itself — cached
    /// rows are ciphertext like any vault. A cache file that won't
    /// open is moved aside and rebuilt; one that still fails
    /// degrades to a remote-only pass-through — the remote is
    /// always the source of truth.
    fn wrap_cache(
        &self,
        name: &str,
        cfg: Option<&VaultConfig>,
        inner: Box<dyn Vault>,
    ) -> Result<Box<dyn Vault>, OpsError> {
        let scope = match cfg.and_then(|c| c.cache.as_ref()) {
            Some(s) if s.enabled() => s,
            _ => return Ok(inner),
        };
        let identity = self.resolve_identity(cfg)?;
        let recipients = cfg.map(|c| c.recipients.as_slice()).unwrap_or(&[]);
        let cache_path = CachedVault::default_cache_file(name);
        let cache: Option<AgeVault> =
            match AgeVault::open(&cache_path, identity.clone(), recipients) {
                Ok(v) => Some(v),
                Err(e) => {
                    fallback_note(&format!(
                        "cache for vault {name:?} unreadable ({e}) — rebuilding it"
                    ));
                    let _ = std::fs::rename(&cache_path, cache_path.with_extension("corrupt"));
                    match AgeVault::open(&cache_path, identity, recipients) {
                        Ok(v) => Some(v),
                        Err(e2) => {
                            fallback_note(&format!(
                                "cache for vault {name:?} still unavailable ({e2}) — \
                             remote-only reads"
                            ));
                            None
                        }
                    }
                }
            };
        let ttl = Duration::from_secs(
            cfg.and_then(|c| c.cache_ttl_secs)
                .unwrap_or_else(|| tamias_vault_cached::DEFAULT_TTL.as_secs()),
        );
        let policy = CachePolicy {
            envs: scope.envs(),
            ttl,
        };
        Ok(Box::new(CachedVault::new(
            inner,
            cache.map(|v| Box::new(v) as Box<dyn Vault>),
            Some(CachedVault::default_meta_file(name)),
            policy,
        )))
    }

    /// Build the concrete [`CfVault`] for a configured
    /// `[vaults.<name>] kind = "cf"` — scoped token refs resolved,
    /// identity resolved through the `tamias-identity` port,
    /// `private_refs` carried through. Used by both
    /// `open_vault_named` and `migrate` (which needs the concrete
    /// type, not the port).
    fn open_cf_vault(&self, name: &str, c: &tamias_core::VaultConfig) -> Result<CfVault, OpsError> {
        let url = c
            .url
            .as_deref()
            .ok_or_else(|| OpsError::Config(format!("[vaults.{name}] kind cf needs a `url`")))?;
        let scoped = self.scoped_tokens(name, &c.tokens)?;
        let identity = self.resolve_identity(Some(c))?;
        let v = CfVault::new(&CfVaultConfig {
            url: url.to_string(),
            token_env: Some(
                c.token_env
                    .clone()
                    .unwrap_or_else(|| "TAMIAS_CF_TOKEN".into()),
            ),
            scoped,
            identity,
            extra_recipients: c.recipients.clone(),
            private_refs: c.private_refs,
        })?;
        Ok(v)
    }

    /// Resolve `[vaults.<name>.tokens]` — every `tamias://` ref is a
    /// *token ref*: the token itself lives as a secret in the
    /// `local` vault, under the env the entry scopes. The ref path
    /// (env pin stripped) is the wire `token_id`; the stored value
    /// is the HMAC secret, so the client sends `TamiasHmac`
    /// signatures — the token never travels (P-003 phase 2).
    /// Each resolution is a materialization and is audited as such.
    fn scoped_tokens(
        &self,
        vault_name: &str,
        tokens: &BTreeMap<Env, Ref>,
    ) -> Result<BTreeMap<String, CfAuth>, OpsError> {
        let mut out = BTreeMap::new();
        if tokens.is_empty() {
            return Ok(out);
        }
        if self
            .config
            .vault_config(tamias_core::config::LOCAL_VAULT)
            .map(|c| c.kind.as_str())
            == Some("cf")
        {
            return Err(OpsError::Config(format!(
                "[vaults.{vault_name}.tokens] resolve against the local vault, \
                 but [vaults.local] is itself kind \"cf\" — move the tokens \
                 or the loop never terminates"
            )));
        }
        let local = self.open_vault_named(tamias_core::config::LOCAL_VAULT)?;
        for (env, rref) in tokens {
            let bare = rref
                .env_pin(|s| self.config.is_known_env(s))
                .map(|(_, r)| r)
                .unwrap_or_else(|| rref.clone());
            let rec = local.get(env, &bare)?.ok_or_else(|| {
                OpsError::Config(format!(
                    "[vaults.{vault_name}.tokens.{env}] → {rref} is not stored \
                     in the local vault env {env}"
                ))
            })?;
            self.audit(Op::Get, &bare, env);
            out.insert(
                env.as_str().to_string(),
                CfAuth::Hmac {
                    id: bare.path(),
                    secret: rec.value.expose().to_string(),
                },
            );
        }
        Ok(out)
    }

    /// `tamias migrate --private-refs` — re-key every plaintext-named
    /// row under its derived ref_key, in each env that routes to a
    /// cf vault with `private_refs = true` (P-004). Deliberate, not
    /// ambient: when nothing qualifies, that's a config error with
    /// a pointer, not a silent no-op.
    pub fn migrate_private_refs(&self) -> Result<Vec<MigrateReport>, OpsError> {
        let mut reports = Vec::new();
        let mut saw_cf = false;
        for env in self.config.envs.keys() {
            let name = self.config.vault_for(env);
            let cfg = match self.config.vault_config(name) {
                Some(c) if c.kind == "cf" => c,
                _ => continue,
            };
            saw_cf = true;
            if !cfg.private_refs {
                continue;
            }
            let vault = self.open_cf_vault(name, cfg)?;
            let rep = vault.migrate_private_refs(env)?;
            for r in &rep.rewritten {
                self.audit(Op::Migrate, r, env);
            }
            reports.push(MigrateReport {
                vault: name.to_string(),
                env: env.clone(),
                rewritten: rep.rewritten,
                already_private: rep.already_private,
            });
        }
        if reports.is_empty() {
            let msg = if saw_cf {
                "no routed cf vault has `private_refs = true` — \
                 enable it in [vaults.*] before migrating"
            } else {
                "no cf vault is routed in [envs] — nothing to migrate"
            };
            return Err(OpsError::Config(msg.into()));
        }
        Ok(reports)
    }

    /// Open a `[deploy.<name>]` sink — write-only push targets,
    /// never sources.
    pub fn open_deploy_named(&self, name: &str) -> Result<Box<dyn Deploy>, OpsError> {
        let cfg = self
            .config
            .deploy_config(name)
            .ok_or_else(|| OpsError::Config(format!("no [deploy.{name}] table in config")))?;
        match cfg.kind.as_str() {
            "cf-secrets-store" => {
                let need = |field: &str, v: &Option<String>| {
                    v.clone()
                        .ok_or_else(|| OpsError::Config(format!("[deploy.{name}] needs `{field}`")))
                };
                let token_env = cfg
                    .token_env
                    .as_deref()
                    .unwrap_or(tamias_deploy_cf::DEFAULT_TOKEN_ENV);
                let d = CfSecretsStore::new(
                    name,
                    &need("account_id", &cfg.account_id)?,
                    &need("store_id", &cfg.store_id)?,
                    token_env,
                    cfg.base_url.as_deref(),
                )?;
                Ok(Box::new(d))
            }
            other => Err(OpsError::Config(format!(
                "deploy target {name:?} has unknown kind {other:?} (have: cf-secrets-store)"
            ))),
        }
    }

    /// The `(vault file, configured identity override, recipients)`
    /// an env's vault would use — for `init`/`check` reporting
    /// without opening anything. The middle item is `Some` only
    /// when `[vaults.*].identity` pins a file; identity *resolution*
    /// (env → keyring → default file) happens in
    /// [`Self::resolve_identity`].
    pub fn vault_paths(
        &self,
        env: &Env,
    ) -> Result<(PathBuf, Option<PathBuf>, Vec<String>), OpsError> {
        let name = self.config.vault_for(env);
        let cfg = self.config.vault_config(name);
        let kind = cfg.map(|c| c.kind.as_str()).unwrap_or("age");
        match kind {
            "age" => {}
            "cf" => {
                return Err(OpsError::Config(format!(
                    "vault {name:?} is a remote cf vault — `tamias init` only \
                     bootstraps local age vaults"
                )));
            }
            other => {
                return Err(OpsError::Config(format!(
                    "vault {name:?} has unknown kind {other:?} (have: age, cf)"
                )));
            }
        }
        Ok((
            cfg.and_then(|c| c.path.as_ref())
                .map(PathBuf::from)
                .unwrap_or_else(AgeVault::default_vault_path),
            cfg.and_then(|c| c.identity.as_ref()).map(PathBuf::from),
            cfg.map(|c| c.recipients.clone()).unwrap_or_default(),
        ))
    }

    /// Identity resolution for vault opens —
    ///
    /// 1. an **explicit file override** (`TAMIAS_IDENTITY_FILE`, then
    ///    `[vaults.*].identity`) — always honored, always required;
    /// 2. the **session keyring** when one is reachable and holds
    ///    an `tamias` identity;
    /// 3. the **default keyfile** (`~/.config/tamias/identity.txt`).
    ///
    /// A keyring that can't be read falls back to the file with a
    /// stderr note — a headless box is a degraded session, not a
    /// crash.
    fn resolve_identity(&self, cfg: Option<&VaultConfig>) -> Result<x25519::Identity, OpsError> {
        // An explicit file pins the source — skip the store probe
        // entirely (no dbus round-trip, no misleading fallback note).
        if explicit_identity_file(cfg).is_some() {
            return self.resolve_identity_in(cfg, None);
        }
        // The real probe only exists off-coverage: any Ok-arm
        // expression is unreachable in headless coverage runs
        // (no session store), so it would be a phantom miss.
        #[cfg(not(coverage))]
        let keyring: Option<Arc<dyn IdentitySource>> = match KeyringIdentity::platform() {
            Ok(k) => Some(Arc::new(k)),
            Err(e) => {
                fallback_note(&format!(
                    "no session keyring reachable ({e}) — using the identity file"
                ));
                None
            }
        };
        #[cfg(coverage)]
        let keyring: Option<Arc<dyn IdentitySource>> = {
            fallback_note("no session keyring reachable — using the identity file");
            None
        };
        self.resolve_identity_in(cfg, keyring.as_deref())
    }

    /// The injectable core of [`Self::resolve_identity`] — tests
    /// pass a mock-backed `IdentitySource` through `keyring`.
    fn resolve_identity_in(
        &self,
        cfg: Option<&VaultConfig>,
        keyring: Option<&dyn IdentitySource>,
    ) -> Result<x25519::Identity, OpsError> {
        if let Some(src) = explicit_identity_file(cfg) {
            return load_required(&src);
        }
        if let Some(k) = keyring {
            match k.load() {
                Ok(Some(id)) => return Ok(id),
                Ok(None) => {}
                Err(e) => fallback_note(&format!(
                    "keyring identity unreadable ({e}) — using the identity file"
                )),
            }
        }
        load_required(&FileIdentity::default())
    }

    /// `tamias init` — store the identity in the session keyring when
    /// one is reachable (importing an existing keyfile identity
    /// rather than orphaning its ciphertext), else the `0600`
    /// keyfile; then verify the vault opens. `opts.with_file`
    /// forces the keyfile path.
    pub fn init(&self, opts: &InitOpts) -> Result<InitInfo, OpsError> {
        let name = self.config.vault_for(&self.env);
        let cfg = self.config.vault_config(name);
        let keyring: Option<Arc<dyn IdentitySource>> =
            if opts.with_file || explicit_identity_file(cfg).is_some() {
                None
            } else {
                // See resolve_identity — the probe's Ok arm is
                // unreachable in headless coverage runs.
                #[cfg(not(coverage))]
                match KeyringIdentity::platform() {
                    Ok(k) => Some(Arc::new(k)),
                    Err(e) => {
                        // Headless init is the expected case — say why
                        // the file was chosen, once.
                        fallback_note(&format!(
                            "no session keyring reachable ({e}) — using the identity file"
                        ));
                        None
                    }
                }
                #[cfg(coverage)]
                {
                    fallback_note("no session keyring reachable — using the identity file");
                    None
                }
            };
        self.init_in(opts, keyring.as_deref())
    }

    /// [`Self::init`] with the keyring injected — the testable core.
    fn init_in(
        &self,
        opts: &InitOpts,
        keyring: Option<&dyn IdentitySource>,
    ) -> Result<InitInfo, OpsError> {
        let (vault_path, _cfg_identity, recipients) = self.vault_paths(&self.env)?;
        let name = self.config.vault_for(&self.env);
        let cfg = self.config.vault_config(name);
        let explicit = explicit_identity_file(cfg);
        let (identity, backend, locator, created, migrated, file_remains) =
            if let Some(src) = explicit {
                // An explicit file pins the source — like a
                // `--with-file` user, just configured.
                let (id, created) = ensure_on(&src)?;
                (id, "file", src.describe(), created, false, None)
            } else if opts.with_file {
                let src = FileIdentity::default();
                let (id, created) = ensure_on(&src)?;
                (id, "file", src.describe(), created, false, None)
            } else if let Some(k) = keyring {
                match k.load() {
                    Ok(Some(id)) => (id, "keyring", k.describe(), false, false, None),
                    Ok(None) => {
                        // Migration: an existing keyfile identity is
                        // imported, never orphaned — its ciphertext
                        // must keep decrypting. The file stays as a
                        // backup.
                        let f = FileIdentity::default();
                        match f.load().map_err(OpsError::Identity)? {
                            Some(id) => {
                                k.store(&id).map_err(OpsError::Identity)?;
                                (
                                    id,
                                    "keyring",
                                    k.describe(),
                                    false,
                                    true,
                                    Some(f.path().to_path_buf()),
                                )
                            }
                            None => {
                                let (id, created) = ensure_on(k)?;
                                (id, "keyring", k.describe(), created, false, None)
                            }
                        }
                    }
                    // Store reachable but the read failed — degraded
                    // session, not a crash: bootstrap on the file.
                    Err(e) => {
                        fallback_note(&format!(
                            "keyring identity unreadable ({e}) — using the identity file"
                        ));
                        let src = FileIdentity::default();
                        let (id, created) = ensure_on(&src)?;
                        (id, "file", src.describe(), created, false, None)
                    }
                }
            } else {
                // No session store (headless, no dbus) — the file
                // path, with a clear note rather than a crash.
                let src = FileIdentity::default();
                let (id, created) = ensure_on(&src)?;
                (id, "file", src.describe(), created, false, None)
            };
        let vault = AgeVault::open(&vault_path, identity, &recipients)?;
        Ok(InitInfo {
            vault_path,
            identity: locator,
            identity_backend: backend,
            identity_created: created,
            migrated,
            file_remains,
            recipient: vault.public_recipient(),
        })
    }

    /// Store a secret (env-pin aware); emits an audit event.
    pub fn put(&self, rref: &Ref, value: Secret) -> Result<SecretRecord, OpsError> {
        let (env, r) = self.resolve(rref);
        let rec = self.open_vault(&env)?.put(&env, &r, value)?;
        self.audit(Op::Put, &r, &env);
        Ok(rec)
    }

    /// Read a secret (env-pin aware); `None` when absent. Emits a
    /// materialization audit event only when a value comes out.
    pub fn get(&self, rref: &Ref) -> Result<Option<SecretRecord>, OpsError> {
        let (env, r) = self.resolve(rref);
        let rec = self.open_vault(&env)?.get(&env, &r)?;
        if rec.is_some() {
            self.audit(Op::Get, &r, &env);
        }
        Ok(rec)
    }

    /// Remove a secret; `true` when something was removed.
    pub fn rm(&self, rref: &Ref) -> Result<bool, OpsError> {
        let (env, r) = self.resolve(rref);
        let gone = self.open_vault(&env)?.rm(&env, &r)?;
        if gone {
            self.audit(Op::Rm, &r, &env);
        }
        Ok(gone)
    }

    /// Rotate a secret to a new value (env-pin aware).
    pub fn rotate(&self, rref: &Ref, value: Secret) -> Result<SecretRecord, OpsError> {
        let (env, r) = self.resolve(rref);
        let rec = self.open_vault(&env)?.rotate(&env, &r, value)?;
        self.audit(Op::Rotate, &r, &env);
        Ok(rec)
    }

    /// Push a stored value to a deploy target — the vault is the
    /// source, the `[deploy.NAME]` table the sink. The plaintext
    /// value crosses here (and only here): the sink is an external
    /// store that serves runtimes directly, outside tamias's
    /// ciphertext boundary. Emits an `op = "deploy"` audit event.
    ///
    /// `target` selects the sink; absent, a lone configured target
    /// is used — ambiguity and absence are both errors.
    pub fn deploy(&self, rref: &Ref, target: Option<&str>) -> Result<DeployReport, OpsError> {
        let name = match target {
            Some(t) => t.to_string(),
            None => match self.config.deploy.len() {
                1 => self
                    .config
                    .deploy
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_default(),
                0 => return Err(OpsError::Config("no [deploy.*] targets configured".into())),
                _ => {
                    let names: Vec<&str> = self.config.deploy.keys().map(String::as_str).collect();
                    return Err(OpsError::Config(format!(
                        "multiple deploy targets — pass --target ({})",
                        names.join(", ")
                    )));
                }
            },
        };
        let (env, r) = self.resolve(rref);
        let rec = self
            .open_vault(&env)?
            .get(&env, &r)?
            .ok_or_else(|| OpsError::NotStored {
                var: "(deploy)".into(),
                rref: rref.to_string(),
                env: env.as_str().to_string(),
            })?;
        let sink = self.open_deploy_named(&name)?;
        let report = sink.push(&env, &r, &rec.value)?;
        self.audit(Op::Deploy, &r, &env);
        Ok(DeployReport {
            target: name,
            env,
            rref: r,
            secret_name: report.name,
            created: report.created,
        })
    }

    /// Read a secret in an explicit env — the audited read for
    /// callers that already know the env (the TUI's reveal, which
    /// browses envs other than the selected one). Same event
    /// [`get`](Self::get) emits: one materialization per value out.
    pub fn get_in(&self, env: &Env, rref: &Ref) -> Result<Option<SecretRecord>, OpsError> {
        let rec = self.open_vault(env)?.get(env, rref)?;
        if rec.is_some() {
            self.audit(Op::Get, rref, env);
        }
        Ok(rec)
    }

    /// Refs stored in the selected env (or an explicit one).
    pub fn list(&self, env: Option<&Env>) -> Result<(Env, Vec<Ref>), OpsError> {
        let env = env.cloned().unwrap_or_else(|| self.env.clone());
        let refs = self.open_vault(&env)?.list(&env)?;
        Ok((env, refs))
    }

    /// Refs plus their records (metadata) in the selected env —
    /// backs `list --json`. Values come along inside
    /// [`SecretRecord`]s but are never printed.
    pub fn list_records(
        &self,
        env: Option<&Env>,
    ) -> Result<(Env, Vec<(Ref, SecretRecord)>), OpsError> {
        let env = env.cloned().unwrap_or_else(|| self.env.clone());
        let vault = self.open_vault(&env)?;
        let refs = vault.list(&env)?;
        let mut out = Vec::new();
        for r in refs {
            if let Some(rec) = vault.get(&env, &r)? {
                out.push((r, rec));
            }
        }
        Ok((env, out))
    }

    /// Resolve every declared + ad-hoc `VAR = tamias://ref` pair into
    /// a `VAR → Secret` map for injection. Emits one
    /// materialization event per resolved ref. Missing refs and bad
    /// var names fail the whole operation — partial injection is
    /// worse than none.
    pub fn resolve_for_injection(
        &self,
        op: Op,
        extra: &[(String, String)],
    ) -> Result<BTreeMap<String, Secret>, OpsError> {
        // `[guardrails] deny_adhoc` closes the `deny = ["get"]`
        // bypass: ad-hoc -s pairs can expand the injectable surface
        // arbitrarily — under the flag they're refused (MED-5).
        if !extra.is_empty()
            && self
                .config
                .guardrails
                .as_ref()
                .is_some_and(|g| g.deny_adhoc)
        {
            self.emit(op, None, Outcome::Denied);
            return Err(OpsError::Msg(
                "ad-hoc -s injection is denied by [guardrails] deny_adhoc".into(),
            ));
        }
        // declared surface first, then ad-hoc overrides/additions
        let mut wanted: BTreeMap<String, Ref> = self.config.secrets.clone();
        for (var, raw) in extra {
            if !is_valid_env_var(var) {
                return Err(OpsError::Msg(format!("invalid env var name {var:?}")));
            }
            wanted.insert(var.clone(), raw.parse::<Ref>()?);
        }
        let mut vaults: BTreeMap<String, Box<dyn Vault>> = BTreeMap::new();
        let mut out = BTreeMap::new();
        for (var, rref) in wanted {
            if !is_valid_env_var(&var) {
                return Err(OpsError::Msg(format!(
                    "[secrets] key {var:?} is not a valid env var name"
                )));
            }
            let (env, r) = self.resolve(&rref);
            let vault = match vaults.entry(env.as_str().to_string()) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => e.insert(self.open_vault(&env)?),
            };
            let rec = vault.get(&env, &r)?.ok_or_else(|| OpsError::NotStored {
                var: var.clone(),
                rref: rref.to_string(),
                env: env.as_str().to_string(),
            })?;
            self.audit(op, &r, &env);
            out.insert(var, rec.value);
        }
        Ok(out)
    }

    /// `tamias check` — validate the config, the declared surface,
    /// and vault readability. Opens vaults read-only (no identity
    /// generation, no writes); declared secrets that don't resolve
    /// and undecryptable vaults are findings.
    pub fn check(&self) -> CheckReport {
        let mut problems = Vec::new();
        for var in self.config.secrets.keys() {
            if !is_valid_env_var(var) {
                problems.push(format!("[secrets] key {var:?} is not a valid env var name"));
            }
        }
        for (env, route) in &self.config.envs {
            let name = route.vault();
            if name != tamias_core::config::LOCAL_VAULT && !self.config.vaults.contains_key(name) {
                problems.push(format!(
                    "[envs] {env} routes to unconfigured vault {name:?}"
                ));
            }
        }
        for (name, v) in &self.config.vaults {
            if !matches!(v.kind.as_str(), "age" | "cf") {
                problems.push(format!("[vaults.{name}] kind {:?} — have: age, cf", v.kind));
            }
            if v.kind == "cf" && v.url.is_none() {
                problems.push(format!("[vaults.{name}] kind cf needs a `url`"));
            }
            if v.private_refs && v.kind != "cf" {
                problems.push(format!(
                    "[vaults.{name}] private_refs only applies to kind \"cf\""
                ));
            }
            if !v.tokens.is_empty() && v.kind != "cf" {
                problems.push(format!("[vaults.{name}] tokens only apply to kind \"cf\""));
            }
            if v.cache_ttl_secs.is_some() && v.cache.is_none() {
                problems.push(format!(
                    "[vaults.{name}] cache_ttl_secs is set but cache is not"
                ));
            }
        }
        for (name, d) in &self.config.deploy {
            if d.kind != "cf-secrets-store" {
                problems.push(format!(
                    "[deploy.{name}] kind {:?} — have: cf-secrets-store",
                    d.kind
                ));
            }
            if d.account_id.is_none() || d.store_id.is_none() {
                problems.push(format!("[deploy.{name}] needs `account_id` and `store_id`"));
            }
        }

        // Every vault worth reporting on: configured names plus
        // every route's target (the implicit `local` included).
        let mut vault_names: Vec<String> = self.config.vaults.keys().cloned().collect();
        for route in self.config.envs.values() {
            let name = route.vault().to_string();
            if !vault_names.contains(&name) {
                vault_names.push(name);
            }
        }
        let selected_vault = self.config.vault_for(&self.env).to_string();
        if !vault_names.contains(&selected_vault) {
            vault_names.push(selected_vault.clone());
        }
        vault_names.sort();
        vault_names.dedup();

        let mut vaults = Vec::new();
        let mut opened: BTreeMap<String, Box<dyn Vault>> = BTreeMap::new();
        let mut failures: BTreeMap<String, String> = BTreeMap::new();
        for name in &vault_names {
            let cfg = self.config.vault_config(name);
            let kind = cfg.map(|c| c.kind.clone()).unwrap_or_else(|| "age".into());
            let path = cfg.and_then(|c| c.path.as_ref()).map(PathBuf::from);
            match self.open_vault_named(name).and_then(|v| {
                // Probe: construction is cheap (no I/O); a `list`
                // proves a remote vault is actually reachable.
                match v.list(&self.env) {
                    Ok(_) => Ok(v),
                    Err(e) => Err(e.into()),
                }
            }) {
                Ok(v) => {
                    opened.insert(name.clone(), v);
                    vaults.push(VaultCheck {
                        name: name.clone(),
                        kind,
                        path,
                        ok: true,
                        error: None,
                    });
                }
                Err(e) => {
                    problems.push(format!("vault {name:?} is not readable: {e}"));
                    failures.insert(name.clone(), e.to_string());
                    vaults.push(VaultCheck {
                        name: name.clone(),
                        kind,
                        path,
                        ok: false,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        // Declared surface: does each VAR's ref resolve in its env?
        let mut secrets = Vec::new();
        for (var, rref) in &self.config.secrets {
            let (env, r) = self.resolve(rref);
            let vault_name = self.config.vault_for(&env);
            let resolves = match opened.get(vault_name) {
                Some(v) => v.get(&env, &r).map(|rec| rec.is_some()).unwrap_or(false),
                None => {
                    // The vault probe above already tried this name
                    // and it failed — a second open would fail
                    // identically, so report the recorded error.
                    let err = failures
                        .get(vault_name)
                        .map(String::as_str)
                        .unwrap_or("unknown error");
                    problems.push(format!(
                        "{var} → {rref}: env {env} vault {vault_name:?} not readable: {err}"
                    ));
                    false
                }
            };
            if !resolves && is_valid_env_var(var) {
                problems.push(format!("{var} → {rref} is not stored in env {env}"));
            }
            secrets.push(SecretCheck {
                var: var.clone(),
                rref: r.to_string(),
                env: env.as_str().to_string(),
                resolves,
            });
        }

        CheckReport {
            config_path: self.config_path.clone(),
            env: self.env.clone(),
            vault: selected_vault,
            vaults,
            secrets,
            guardrails: self.config.guardrails.clone(),
            problems,
        }
    }

    /// Emit an op-scoped audit event — denials, failures, and
    /// ref-less verb completions the front-end knows about.
    /// `rref` is the raw requested ref when the verb takes one.
    pub fn emit(&self, op: Op, rref: Option<&str>, outcome: Outcome) {
        audit::emit(
            &AuditEvent::op(self.actor.as_str(), op, rref, self.env.as_str(), outcome)
                .with_policy(self.policy.clone()),
        );
    }

    fn audit(&self, op: Op, rref: &Ref, env: &Env) {
        audit::emit(
            &AuditEvent::materialized(self.actor.as_str(), op, &rref.path(), env.as_str())
                .with_policy(self.policy.clone()),
        );
    }
}

/// The explicit identity-file override for a vault —
/// `TAMIAS_IDENTITY_FILE` wins over `[vaults.*].identity` (the
/// documented precedence). `None` means "resolve normally".
fn explicit_identity_file(cfg: Option<&VaultConfig>) -> Option<FileIdentity> {
    if let Ok(p) = std::env::var("TAMIAS_IDENTITY_FILE") {
        if !p.is_empty() {
            return Some(FileIdentity::new(p));
        }
    }
    cfg.and_then(|c| c.identity.as_ref()).map(FileIdentity::new)
}

/// `load` with `None` → "run `tamias init`" guidance — every caller
/// that *requires* an identity gets the same error shape.
fn load_required(src: &dyn IdentitySource) -> Result<x25519::Identity, OpsError> {
    match src.load() {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(OpsError::Config(format!(
            "no identity at {} — run `tamias init`",
            src.describe()
        ))),
        Err(e) => Err(OpsError::Config(format!(
            "identity via {}: {e}",
            src.describe()
        ))),
    }
}

/// [`tamias_identity::ensure`] with ops-context errors.
fn ensure_on(src: &dyn IdentitySource) -> Result<(x25519::Identity, bool), OpsError> {
    tamias_identity::ensure(src)
        .map_err(|e| OpsError::Identity(IdentityError::Store(format!("{}: {e}", src.describe()))))
}

/// A user-visible fallback notice on stderr — at most once per
/// process so `check` (which opens several vaults) doesn't repeat
/// itself. Not an audit event: it's UX, not wire.
fn fallback_note(msg: &str) {
    static EMITTED: AtomicBool = AtomicBool::new(false);
    if !EMITTED.swap(true, Ordering::Relaxed) {
        eprintln!("tamias: {msg}");
    }
}

/// `--env` → `TAMIAS_ENV` → `dev`.
fn resolve_env(flag: Option<&str>) -> Result<Env, OpsError> {
    let name = flag
        .map(str::to_string)
        .or_else(|| std::env::var("TAMIAS_ENV").ok())
        .unwrap_or_else(|| "dev".into());
    Ok(Env::new(&name)?)
}

/// `--actor`/`TAMIAS_ACTOR` (must validate) → `$USER`/`$LOGNAME`
/// (best-effort) → `anon`.
fn resolve_actor(flag: Option<&str>) -> Result<Actor, OpsError> {
    let claimed = flag
        .map(str::to_string)
        .or_else(|| std::env::var("TAMIAS_ACTOR").ok());
    if let Some(a) = claimed {
        return Ok(Actor::new(&a)?);
    }
    for var in ["USER", "LOGNAME"] {
        if let Ok(u) = std::env::var(var) {
            if let Ok(a) = Actor::new(&u) {
                return Ok(a);
            }
        }
    }
    Actor::new("anon").map_err(OpsError::Actor)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn env_pin_overrides_context() {
        let ops = Ops {
            config: Config::default(),
            config_path: None,
            env: Env::dev(),
            actor: Actor::new("devin").unwrap(),
            policy: None,
        };
        let (env, r) = ops.resolve(&Ref::new("tamias://prod/billing/stripe").unwrap());
        assert_eq!(env.as_str(), "prod");
        assert_eq!(r.path(), "billing/stripe");
        let (env, r) = ops.resolve(&Ref::new("tamias://billing/stripe").unwrap());
        assert_eq!(env.as_str(), "dev");
        assert_eq!(r.path(), "billing/stripe");
        // custom envs count when configured
        let mut config = Config::default();
        config.envs.insert(Env::new("ci").unwrap(), "local".into());
        let ops = Ops { config, ..ops };
        let (env, _) = ops.resolve(&Ref::new("tamias://ci/token").unwrap());
        assert_eq!(env.as_str(), "ci");
    }

    #[test]
    fn check_flags_bad_var_names_and_routes() {
        let mut config = Config::default();
        config
            .secrets
            .insert("NOT-A-VAR".into(), Ref::new("tamias://x/y").unwrap());
        config
            .envs
            .insert(Env::new("ci").unwrap(), "nowhere".into());
        let ops = Ops {
            config,
            config_path: None,
            env: Env::dev(),
            actor: Actor::new("devin").unwrap(),
            policy: None,
        };
        // Isolate HOME so default vault paths miss deterministically.
        let home = tempfile::tempdir().unwrap();
        temp_env::with_vars(
            [
                ("HOME", Some(home.path().to_str().unwrap())),
                ("XDG_CONFIG_HOME", Some(home.path().to_str().unwrap())),
                ("XDG_DATA_HOME", Some(home.path().to_str().unwrap())),
                // pinned at a missing file → deterministic file-mode
                // resolution, the real keyring never probed
                (
                    "TAMIAS_IDENTITY_FILE",
                    Some(home.path().join("no-id.txt").to_str().unwrap()),
                ),
            ],
            || {
                let report = ops.check();
                let has = |needle: &str| report.problems.iter().any(|p| p.contains(needle));
                assert!(has("not a valid env var"), "{:?}", report.problems);
                assert!(has("unconfigured vault"), "{:?}", report.problems);
                // vaults can't open without an identity → reported
                assert!(report.vaults.iter().all(|v| !v.ok));
                assert!(has("not readable"), "{:?}", report.problems);
            },
        );
    }

    #[test]
    fn check_reports_resolving_secrets() {
        let home = tempfile::tempdir().unwrap();
        let id_path = home.path().join("identity.txt");
        let vault_path = home.path().join("vault.age");
        let (identity, _) = tamias_identity::ensure(&FileIdentity::new(&id_path)).unwrap();
        let vault = AgeVault::open(&vault_path, identity, &[]).unwrap();
        Vault::put(
            &vault,
            &Env::dev(),
            &Ref::new("tamias://billing/stripe").unwrap(),
            Secret::new("fake-check-value"),
        )
        .unwrap();

        let mut config = Config::default();
        config.vaults.insert(
            "local".into(),
            tamias_core::VaultConfig {
                kind: "age".into(),
                path: Some(vault_path.to_string_lossy().into_owned()),
                identity: Some(id_path.to_string_lossy().into_owned()),
                recipients: vec![],
                url: None,
                token_env: None,
                tokens: BTreeMap::new(),
                private_refs: false,
                cache: None,
                cache_ttl_secs: None,
            },
        );
        config.secrets.insert(
            "STRIPE_KEY".into(),
            Ref::new("tamias://billing/stripe").unwrap(),
        );
        config
            .secrets
            .insert("MISSING".into(), Ref::new("tamias://not/here").unwrap());
        let ops = Ops {
            config,
            config_path: None,
            env: Env::dev(),
            actor: Actor::new("devin").unwrap(),
            policy: None,
        };
        let report = ops.check();
        assert!(report.vaults.iter().all(|v| v.ok), "{:?}", report.vaults);
        let stripe = report.secrets.iter().find(|s| s.var == "STRIPE_KEY");
        assert_eq!(stripe.map(|s| s.resolves), Some(true));
        let missing = report.secrets.iter().find(|s| s.var == "MISSING");
        assert_eq!(missing.map(|s| s.resolves), Some(false));
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("MISSING") && p.contains("not stored")),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn scoped_overrides_actor_and_env_per_request() {
        let ops = mk_ops(Config::default());
        let scoped = ops.scoped(
            Some(Actor::new("agent:peregrin").unwrap()),
            Some(Env::new("prod").unwrap()),
            Some(PolicyVerdict {
                allow: true,
                reason: None,
                obligations: None,
            }),
        );
        assert_eq!(scoped.actor().as_str(), "agent:peregrin");
        assert_eq!(scoped.env().as_str(), "prod");
        // resolution honors the scoped env; a pin still wins
        let (env, _) = scoped.resolve(&Ref::new("tamias://a/b").unwrap());
        assert_eq!(env.as_str(), "prod");
        // None keeps the loaded values
        let same = ops.scoped(None, None, None);
        assert_eq!(same.actor().as_str(), "devin");
        assert_eq!(same.env().as_str(), "dev");
    }

    fn mk_ops(config: Config) -> Ops {
        Ops {
            config,
            config_path: None,
            env: Env::dev(),
            actor: Actor::new("devin").unwrap(),
            policy: None,
        }
    }

    fn vault_cfg(kind: &str, url: Option<&str>) -> tamias_core::VaultConfig {
        tamias_core::VaultConfig {
            kind: kind.into(),
            path: None,
            identity: None,
            recipients: vec![],
            url: url.map(str::to_string),
            token_env: None,
            tokens: BTreeMap::new(),
            private_refs: false,
            cache: None,
            cache_ttl_secs: None,
        }
    }

    fn deploy_cfg(
        kind: &str,
        account: Option<&str>,
        store: Option<&str>,
    ) -> tamias_core::DeployConfig {
        tamias_core::DeployConfig {
            kind: kind.into(),
            account_id: account.map(str::to_string),
            store_id: store.map(str::to_string),
            token_env: None,
            base_url: None,
        }
    }

    /// Isolate every path an age vault could pick up by default.
    /// `TAMIAS_IDENTITY_FILE` is pinned at a temp path so resolution
    /// is deterministic file-mode — the real session keyring is
    /// never probed in tests.
    fn isolate_home(home: &tempfile::TempDir, f: impl FnOnce()) {
        temp_env::with_vars(
            [
                ("HOME", Some(home.path().to_str().unwrap())),
                (
                    "XDG_CONFIG_HOME",
                    Some(home.path().join("c").to_str().unwrap()),
                ),
                (
                    "XDG_DATA_HOME",
                    Some(home.path().join("d").to_str().unwrap()),
                ),
                (
                    "TAMIAS_IDENTITY_FILE",
                    Some(home.path().join("identity.txt").to_str().unwrap()),
                ),
                ("TAMIAS_CONFIG", None::<&str>),
            ],
            f,
        );
    }

    /// Write a fresh identity keyfile under `dir` — via the file
    /// impl, the same path `resolve_identity` takes.
    fn write_identity(dir: &std::path::Path) -> PathBuf {
        let src = FileIdentity::new(dir.join("identity.txt"));
        tamias_identity::ensure(&src).unwrap();
        src.path().to_path_buf()
    }

    /// An Ops whose `local` vault is a real age vault in a tempdir,
    /// with `refs` pre-stored under `env`.
    fn ops_with_vault(env: &Env, refs: &[&str]) -> (tempfile::TempDir, Ops) {
        let home = tempfile::tempdir().unwrap();
        let id_path = home.path().join("identity.txt");
        let vault_path = home.path().join("vault.age");
        let (identity, _) = tamias_identity::ensure(&FileIdentity::new(&id_path)).unwrap();
        let vault = AgeVault::open(&vault_path, identity, &[]).unwrap();
        for r in refs {
            Vault::put(
                &vault,
                env,
                &Ref::new(r).unwrap(),
                Secret::new("fake-stored-value"),
            )
            .unwrap();
        }
        let mut config = Config::default();
        let mut v = vault_cfg("age", None);
        v.path = Some(vault_path.to_string_lossy().into_owned());
        v.identity = Some(id_path.to_string_lossy().into_owned());
        config.vaults.insert("local".into(), v);
        (home, mk_ops(config))
    }

    /// An Ops whose `dev` env routes to a cf vault on `url` —
    /// `token_var` is left to the caller to set (or not).
    fn ops_on_cf(url: &str, token_var: &str, id_path: &std::path::Path) -> Ops {
        let mut config = Config::default();
        let mut v = vault_cfg("cf", Some(url));
        v.token_env = Some(token_var.into());
        v.identity = Some(id_path.to_string_lossy().into_owned());
        config.vaults.insert("cloud".into(), v);
        config.envs.insert(Env::dev(), "cloud".into());
        mk_ops(config)
    }

    #[test]
    fn load_reports_explicit_config_and_flag_errors() {
        // explicit --config that doesn't exist → config error
        let e = Ops::load(LoadOpts {
            config_path: Some("/nonexistent/tamias.toml".into()),
            ..LoadOpts::default()
        })
        .err()
        .unwrap();
        assert!(format!("{e:?}").starts_with("Config"), "{e}");
        // explicit --config that exists but won't parse
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("tamias.toml");
        std::fs::write(&bad, "[[[ not toml").unwrap();
        let e = Ops::load(LoadOpts {
            config_path: Some(bad.to_string_lossy().into_owned()),
            ..LoadOpts::default()
        })
        .err()
        .unwrap();
        assert!(format!("{e:?}").starts_with("Config"), "{e}");
        // bad --env and bad --actor surface as typed errors
        let e = Ops::load(LoadOpts {
            env: Some("PROD".into()),
            ..LoadOpts::default()
        })
        .err()
        .unwrap();
        assert!(format!("{e:?}").starts_with("Env"), "{e}");
        let e = Ops::load(LoadOpts {
            actor: Some("bad actor!".into()),
            ..LoadOpts::default()
        })
        .err()
        .unwrap();
        assert!(format!("{e:?}").starts_with("Actor"), "{e}");
    }

    #[test]
    fn env_and_actor_resolution_variants() {
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_vars(
                [("TAMIAS_ENV", Some("ci")), ("TAMIAS_ACTOR", Some("bot-1"))],
                || {
                    let ops = Ops::load(LoadOpts::default()).unwrap();
                    assert_eq!(ops.env().as_str(), "ci");
                    assert_eq!(ops.actor().as_str(), "bot-1");
                    assert!(ops.config_path().is_none());
                },
            );
            // an unreadable TAMIAS_ENV is a typed error
            temp_env::with_vars(
                [("TAMIAS_ENV", Some("BAD!!")), ("TAMIAS_ACTOR", Some("bot"))],
                || {
                    let e = Ops::load(LoadOpts::default()).err().unwrap();
                    assert!(format!("{e:?}").starts_with("Env"), "{e}");
                },
            );
            // USER present and valid → claimed as actor
            temp_env::with_vars(
                [
                    ("TAMIAS_ACTOR", None::<&str>),
                    ("USER", Some("luis-x")),
                    ("LOGNAME", None::<&str>),
                ],
                || {
                    let ops = Ops::load(LoadOpts::default()).unwrap();
                    assert_eq!(ops.actor().as_str(), "luis-x");
                },
            );
            // USER invalid + LOGNAME unset → the anon fallback
            temp_env::with_vars(
                [
                    ("TAMIAS_ACTOR", None::<&str>),
                    ("USER", Some("bad name")),
                    ("LOGNAME", None::<&str>),
                ],
                || {
                    let ops = Ops::load(LoadOpts::default()).unwrap();
                    assert_eq!(ops.actor().as_str(), "anon");
                },
            );
        });
    }

    #[test]
    fn open_vault_named_rejects_bad_kinds_and_missing_cf_url() {
        // cf without a url → a named config error
        let mut config = Config::default();
        config.vaults.insert("cloud".into(), vault_cfg("cf", None));
        let ops = mk_ops(config.clone());
        let e = ops.open_vault_named("cloud").err().unwrap().to_string();
        assert!(e.contains("needs a `url`"), "{e}");
        // unknown kind → named error listing what exists
        config.vaults.insert("bogus".into(), vault_cfg("zzz", None));
        let ops = mk_ops(config);
        let e = ops.open_vault_named("bogus").err().unwrap().to_string();
        assert!(e.contains("unknown kind") && e.contains("zzz"), "{e}");
    }

    #[test]
    fn open_deploy_named_covers_table_errors() {
        // no table at all
        let ops = mk_ops(Config::default());
        let e = ops.open_deploy_named("prod").err().unwrap().to_string();
        assert!(e.contains("no [deploy.prod] table"), "{e}");
        // missing account_id/store_id fields
        let mut config = Config::default();
        config
            .deploy
            .insert("prod".into(), deploy_cfg("cf-secrets-store", None, None));
        let ops = mk_ops(config.clone());
        let e = ops.open_deploy_named("prod").err().unwrap().to_string();
        assert!(e.contains("account_id"), "{e}");
        // account_id present, store_id missing → the second `need`
        let mut config2 = Config::default();
        config2.deploy.insert(
            "prod".into(),
            deploy_cfg("cf-secrets-store", Some("acc"), None),
        );
        let ops = mk_ops(config2);
        let e = ops.open_deploy_named("prod").err().unwrap().to_string();
        assert!(e.contains("store_id"), "{e}");
        // unknown kind
        config
            .deploy
            .insert("odd".into(), deploy_cfg("gcs", None, None));
        let ops = mk_ops(config);
        let e = ops.open_deploy_named("odd").err().unwrap().to_string();
        assert!(e.contains("unknown kind") && e.contains("gcs"), "{e}");
        // complete shape but the token env var is unset
        temp_env::with_var_unset("TAMIAS_TEST_DEPLOY_TOKEN", || {
            let mut config = Config::default();
            let mut d = deploy_cfg("cf-secrets-store", Some("acc"), Some("sto"));
            d.token_env = Some("TAMIAS_TEST_DEPLOY_TOKEN".into());
            config.deploy.insert("prod".into(), d);
            let ops = mk_ops(config);
            let e = ops.open_deploy_named("prod").err().unwrap().to_string();
            assert!(e.contains("TAMIAS_TEST_DEPLOY_TOKEN"), "{e}");
        });
    }

    #[test]
    fn vault_paths_reports_configured_paths_and_kind_errors() {
        // a configured age vault reports its configured paths
        let mut config = Config::default();
        let mut v = vault_cfg("age", None);
        v.path = Some("/tmp/x/vault.age".into());
        v.identity = Some("/tmp/x/id.txt".into());
        v.recipients =
            vec!["age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqygh9".to_string()];
        config.vaults.insert("local".into(), v);
        let ops = mk_ops(config);
        let (vp, ip, rec) = ops.vault_paths(&Env::dev()).unwrap();
        assert_eq!(vp, PathBuf::from("/tmp/x/vault.age"));
        assert_eq!(ip, Some(PathBuf::from("/tmp/x/id.txt")));
        assert_eq!(rec.len(), 1);
        // cf and unknown kinds can't init locally
        let mut config = Config::default();
        config
            .vaults
            .insert("cloud".into(), vault_cfg("cf", Some("http://x")));
        config.envs.insert(Env::new("ci").unwrap(), "cloud".into());
        config.vaults.insert("bogus".into(), vault_cfg("zzz", None));
        config.envs.insert(Env::new("st").unwrap(), "bogus".into());
        let ops = mk_ops(config);
        let e = ops
            .vault_paths(&Env::new("ci").unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("remote cf"), "{e}");
        let e = ops
            .vault_paths(&Env::new("st").unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown kind"), "{e}");
    }

    #[test]
    fn init_happy_and_error_paths() {
        // happy: isolated HOME → fresh identity generated, vault opens.
        // isolate_home pins TAMIAS_IDENTITY_FILE → explicit file mode.
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            let ops = mk_ops(Config::default());
            let info = ops.init(&InitOpts::default()).unwrap();
            assert!(info.identity_created);
            assert_eq!(info.identity_backend, "file");
            assert!(info.recipient.starts_with("age1"));
            assert!(PathBuf::from(&info.identity).exists());
            assert!(info.vault_path.ends_with("vault.age"));
        });
        // cf-routed env → init refuses (remote vaults can't init locally)
        let mut config = Config::default();
        config
            .vaults
            .insert("cloud".into(), vault_cfg("cf", Some("http://x")));
        config.envs.insert(Env::dev(), "cloud".into());
        let ops = mk_ops(config);
        assert!(ops
            .init(&InitOpts::default())
            .unwrap_err()
            .to_string()
            .contains("cf"));
        // identity path whose parent can't be created → ensure fails
        let home = tempfile::tempdir().unwrap();
        let blocker = home.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let mut config = Config::default();
        let mut v = vault_cfg("age", None);
        v.path = Some(home.path().join("v1.age").to_string_lossy().into_owned());
        v.identity = Some(blocker.join("id.txt").to_string_lossy().into_owned());
        config.vaults.insert("local".into(), v);
        let ops = mk_ops(config);
        assert!(ops.init(&InitOpts::default()).is_err());
        // garbage identity present → ensure refuses, never clobbers
        let bad_id = home.path().join("bad-id.txt");
        std::fs::write(&bad_id, "not-a-key\n").unwrap();
        let mut config = Config::default();
        let mut v = vault_cfg("age", None);
        v.path = Some(home.path().join("v2.age").to_string_lossy().into_owned());
        v.identity = Some(bad_id.to_string_lossy().into_owned());
        config.vaults.insert("local".into(), v);
        let ops = mk_ops(config);
        assert!(ops.init(&InitOpts::default()).is_err());
    }

    #[test]
    fn verbs_fail_when_the_vault_cant_open() {
        // env dev routes to a vault of unknown kind → every verb's
        // open fails with a config error, not a panic
        let mut config = Config::default();
        config.vaults.insert("bogus".into(), vault_cfg("zzz", None));
        config.envs.insert(Env::dev(), "bogus".into());
        let ops = mk_ops(config);
        let r = Ref::new("tamias://x/y").unwrap();
        assert!(ops.put(&r, Secret::new("fake")).is_err());
        assert!(ops.get(&r).is_err());
        assert!(ops.rm(&r).is_err());
        assert!(ops.rotate(&r, Secret::new("fake")).is_err());
        assert!(ops.get_in(&Env::dev(), &r).is_err());
        assert!(ops.list(None).is_err());
        assert!(ops.list_records(None).is_err());
        assert!(ops.deploy(&r, Some("t")).is_err());
        // an explicit env other than the selected one works the same
        assert!(ops.list(Some(&Env::dev())).is_err());
    }

    #[test]
    fn deploy_target_selection_errors() {
        let r = Ref::new("tamias://x/y").unwrap();
        // zero targets configured → can't pick
        let ops = mk_ops(Config::default());
        let e = ops.deploy(&r, None).unwrap_err().to_string();
        assert!(e.contains("no [deploy.*] targets"), "{e}");
        // several targets and no --target → ambiguous
        let mut config = Config::default();
        config.deploy.insert(
            "a".into(),
            deploy_cfg("cf-secrets-store", Some("x"), Some("y")),
        );
        config.deploy.insert(
            "b".into(),
            deploy_cfg("cf-secrets-store", Some("x"), Some("y")),
        );
        let ops = mk_ops(config);
        let e = ops.deploy(&r, None).unwrap_err().to_string();
        assert!(e.contains("multiple deploy targets"), "{e}");
        // a --target name that has no table → open_deploy_named errs
        let (_h, ops) = ops_with_vault(&Env::dev(), &["tamias://x/y"]);
        let e = ops.deploy(&r, Some("ghost")).unwrap_err().to_string();
        assert!(e.contains("no [deploy.ghost] table"), "{e}");
    }

    #[test]
    fn get_in_reads_across_envs() {
        let (_h, ops) = ops_with_vault(&Env::new("prod").unwrap(), &["tamias://x/y"]);
        let got = ops
            .get_in(
                &Env::new("prod").unwrap(),
                &Ref::new("tamias://x/y").unwrap(),
            )
            .unwrap();
        assert_eq!(got.unwrap().value.expose(), "fake-stored-value");
        let miss = ops
            .get_in(
                &Env::new("prod").unwrap(),
                &Ref::new("tamias://no/thing").unwrap(),
            )
            .unwrap();
        assert!(miss.is_none());
    }

    #[test]
    fn injection_validates_pairs_and_var_names() {
        let ops = mk_ops(Config::default());
        // bad ad-hoc var name
        let e = ops
            .resolve_for_injection(Op::Run, &[("NOT-A-VAR".into(), "tamias://x/y".into())])
            .unwrap_err()
            .to_string();
        assert!(e.contains("invalid env var"), "{e}");
        // bad ad-hoc ref
        assert!(ops
            .resolve_for_injection(Op::Run, &[("GOOD".into(), "not-a-ref".into())])
            .is_err());
        // a declared [secrets] key that isn't a var name
        let mut config = Config::default();
        config
            .secrets
            .insert("NOT-A-VAR".into(), Ref::new("tamias://x/y").unwrap());
        let ops = mk_ops(config);
        let e = ops
            .resolve_for_injection(Op::Run, &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a valid env var"), "{e}");
        // a ref pinned to an env whose vault can't open
        let mut config = Config::default();
        config
            .envs
            .insert(Env::new("ci").unwrap(), "nowhere".into());
        config
            .secrets
            .insert("V".into(), Ref::new("tamias://ci/x").unwrap());
        let ops = mk_ops(config);
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            assert!(ops.resolve_for_injection(Op::Run, &[]).is_err());
        });
    }

    #[test]
    fn deny_adhoc_blocks_dash_s_but_not_the_declared_surface() {
        // MED-5: [guardrails] deny_adhoc refuses ad-hoc -s pairs —
        // the `deny = ["get"]` bypass — while declared [secrets]
        // injection is untouched.
        let (_h, mut ops) = ops_with_vault(&Env::dev(), &["tamias://x/y"]);
        ops.config
            .secrets
            .insert("V".into(), Ref::new("tamias://x/y").unwrap());
        ops.config.guardrails = Some(tamias_core::Guardrails {
            deny_adhoc: true,
            ..Default::default()
        });
        // -s is refused with a denied audit event
        let e = ops
            .resolve_for_injection(Op::Run, &[("X".into(), "tamias://x/y".into())])
            .unwrap_err()
            .to_string();
        assert!(e.contains("deny_adhoc"), "{e}");
        // declared surface still resolves
        let vars = ops.resolve_for_injection(Op::Run, &[]).unwrap();
        assert_eq!(vars["V"].expose(), "fake-stored-value");
        // guardrails absent → ad-hoc works as before
        let (_h, ops) = ops_with_vault(&Env::dev(), &["tamias://x/y"]);
        let vars = ops
            .resolve_for_injection(Op::Run, &[("X".into(), "tamias://x/y".into())])
            .unwrap();
        assert_eq!(vars["X"].expose(), "fake-stored-value");
    }

    #[test]
    fn check_flags_kind_and_deploy_problems() {
        let mut config = Config::default();
        config.vaults.insert("bad".into(), vault_cfg("zzz", None));
        config.vaults.insert("nolink".into(), vault_cfg("cf", None));
        config
            .deploy
            .insert("weird".into(), deploy_cfg("bogus-kind", None, None));
        config
            .deploy
            .insert("half".into(), deploy_cfg("cf-secrets-store", None, None));
        // account set, store missing → the `||` right arm runs too
        config.deploy.insert(
            "half2".into(),
            deploy_cfg("cf-secrets-store", Some("a"), None),
        );
        let ops = mk_ops(config);
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            let report = ops.check();
            let has = |n: &str| report.problems.iter().any(|p| p.contains(n));
            assert!(has("\"zzz\""), "{:?}", report.problems);
            assert!(has("needs a `url`"), "{:?}", report.problems);
            assert!(has("bogus-kind"), "{:?}", report.problems);
            assert!(has("account_id"), "{:?}", report.problems);
            // none of the configured vaults can open either
            assert!(report.vaults.iter().all(|v| !v.ok));
        });
    }

    #[test]
    fn check_reports_open_ok_but_list_fails_and_secret_findings() {
        // A cf vault: construction is cheap (no I/O) — it opens,
        // then `list` proves it unreachable → "not readable" on the
        // vault AND on each secret that routes through it.
        let home = tempfile::tempdir().unwrap();
        let id_path = write_identity(home.path());
        let mut config = Config::default();
        let mut v = vault_cfg("cf", Some("http://127.0.0.1:1"));
        v.identity = Some(id_path.to_string_lossy().into_owned());
        config.vaults.insert("cloud".into(), v);
        config.envs.insert(Env::dev(), "cloud".into());
        config
            .secrets
            .insert("V".into(), Ref::new("tamias://x/y").unwrap());
        let ops = mk_ops(config);
        temp_env::with_vars(
            [
                ("TAMIAS_CF_TOKEN", Some("fake-cf-token")),
                ("HOME", Some(home.path().to_str().unwrap())),
                (
                    "XDG_CONFIG_HOME",
                    Some(home.path().join("c").to_str().unwrap()),
                ),
                (
                    "XDG_DATA_HOME",
                    Some(home.path().join("d").to_str().unwrap()),
                ),
                ("TAMIAS_IDENTITY_FILE", None::<&str>),
            ],
            || {
                let report = ops.check();
                let has = |n: &str| report.problems.iter().any(|p| p.contains(n));
                assert!(has("not readable"), "{:?}", report.problems);
                assert!(has("V → tamias://x/y"), "{:?}", report.problems);
                let cloud = report.vaults.iter().find(|v| v.name == "cloud").unwrap();
                assert!(!cloud.ok);
                assert!(cloud.error.is_some());
            },
        );
    }

    #[test]
    fn cf_backend_errors_propagate_through_verbs() {
        use tamias_testkit::httpmock::{HttpMock, Response};
        // List says the ref exists; single-get says otherwise or
        // just fails — the verbs surface backend errors, no panic.
        let mock = HttpMock::new(|req| {
            if req.path == "/v1/dev" {
                Response::json(200, r#"{"refs":["gone/one","x/y"]}"#)
            } else if req.path.ends_with("/gone/one") {
                Response::json(404, r#"{"error":"not found"}"#)
            } else {
                Response::json(500, r#"{"error":"boom"}"#)
            }
        });
        let home = tempfile::tempdir().unwrap();
        let id_path = write_identity(home.path());
        let ops = ops_on_cf(mock.url(), "TAMIAS_TEST_OPS_TOKEN", &id_path);
        temp_env::with_var("TAMIAS_TEST_OPS_TOKEN", Some("fake-token"), || {
            let r = Ref::new("tamias://x/y").unwrap();
            // backend 500s propagate as vault errors on every verb
            assert!(ops.get(&r).is_err());
            assert!(ops.put(&r, Secret::new("fake")).is_err());
            assert!(ops.rm(&r).is_err());
            assert!(ops.rotate(&r, Secret::new("fake")).is_err());
            // list_records: gone/one → 404 → skipped silently;
            // x/y → 500 → the error propagates
            assert!(ops.list_records(None).is_err());
            // injection hits the same failing get
            let mut config = Config::default();
            let mut v = vault_cfg("cf", Some(mock.url()));
            v.token_env = Some("TAMIAS_TEST_OPS_TOKEN".into());
            v.identity = Some(id_path.to_string_lossy().into_owned());
            config.vaults.insert("cloud".into(), v);
            config.envs.insert(Env::dev(), "cloud".into());
            config.secrets.insert("V".into(), r.clone());
            let ops = mk_ops(config);
            assert!(ops.resolve_for_injection(Op::Run, &[]).is_err());
        });
    }

    #[test]
    fn list_and_deploy_surface_backend_failures() {
        use tamias_testkit::httpmock::{HttpMock, Response};
        let home = tempfile::tempdir().unwrap();
        let id_path = write_identity(home.path());
        let r = Ref::new("tamias://x/y").unwrap();
        // every backend call 500s → list() and list_records() both
        // surface the vault's list error
        let mock = HttpMock::new(|_| Response::json(500, r#"{"error":"boom"}"#));
        let ops = ops_on_cf(mock.url(), "TAMIAS_TEST_OPS_TOKEN", &id_path);
        temp_env::with_var("TAMIAS_TEST_OPS_TOKEN", Some("fake-token"), || {
            assert!(ops.list(None).is_err());
            assert!(ops.list_records(None).is_err());
            // deploy: vault get fails before any sink work
            let mut config = Config::default();
            let mut v = vault_cfg("cf", Some(mock.url()));
            v.token_env = Some("TAMIAS_TEST_OPS_TOKEN".into());
            v.identity = Some(id_path.to_string_lossy().into_owned());
            config.vaults.insert("cloud".into(), v);
            config.envs.insert(Env::dev(), "cloud".into());
            let mut d = deploy_cfg("cf-secrets-store", Some("a"), Some("s"));
            d.token_env = Some("TAMIAS_TEST_OPS_TOKEN".into());
            d.base_url = Some(mock.url().to_string());
            config.deploy.insert("prod".into(), d);
            let ops = mk_ops(config);
            assert!(ops.deploy(&r, Some("prod")).is_err());
        });
        // vault get succeeds (real age vault) but the sink push 500s
        let (_h, mut ops) = ops_with_vault(&Env::dev(), &["tamias://x/y"]);
        let mut d = deploy_cfg("cf-secrets-store", Some("a"), Some("s"));
        d.token_env = Some("TAMIAS_TEST_OPS_TOKEN".into());
        d.base_url = Some(mock.url().to_string());
        ops.config.deploy.insert("prod".into(), d);
        temp_env::with_var("TAMIAS_TEST_OPS_TOKEN", Some("fake-token"), || {
            assert!(ops.deploy(&r, Some("prod")).is_err());
        });
    }

    #[test]
    fn emit_writes_an_op_scoped_event() {
        let ops = mk_ops(Config::default());
        // stderr is the wire — this is a "does not panic" probe;
        // shape assertions live in audit.rs's tests
        ops.emit(Op::Get, Some("x/y"), Outcome::Denied);
        ops.emit(Op::List, None, Outcome::Error);
    }

    /// A scripted `IdentitySource` for resolution tests — no real
    /// keyring needed.
    struct StubSource {
        stored: std::sync::Mutex<Option<x25519::Identity>>,
        err: Option<IdentityError>,
    }

    impl StubSource {
        fn holding(id: x25519::Identity) -> Self {
            StubSource {
                stored: std::sync::Mutex::new(Some(id)),
                err: None,
            }
        }

        fn empty() -> Self {
            StubSource {
                stored: std::sync::Mutex::new(None),
                err: None,
            }
        }

        fn failing(e: IdentityError) -> Self {
            StubSource {
                stored: std::sync::Mutex::new(None),
                err: Some(e),
            }
        }
    }

    #[test]
    fn open_cf_vault_constructs_when_the_config_is_whole() {
        // Ok(v) — a complete cf config (identity + a credential)
        // constructs the client without touching the network.
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_var("TAMIAS_TEST_CF_OPEN_TOK", Some("fake-tok"), || {
                let id_path = write_identity(home.path());
                let mut cfg = vault_cfg("cf", Some("http://127.0.0.1:1"));
                cfg.identity = Some(id_path.to_string_lossy().into_owned());
                cfg.token_env = Some("TAMIAS_TEST_CF_OPEN_TOK".into());
                let mut config = Config::default();
                config.vaults.insert("cloud".into(), cfg);
                let ops = mk_ops(config);
                let c = ops.config.vault_config("cloud").unwrap().clone();
                assert!(ops.open_cf_vault("cloud", &c).is_ok());
                // and the `?` — a config that names a token env var
                // that's unset fails inside CfVault::new
                let mut bad = c.clone();
                bad.token_env = Some("TAMIAS_TEST_CF_MISSING_TOK".into());
                assert!(ops.open_cf_vault("cloud", &bad).is_err());
            });
        });
    }

    #[test]
    fn migrate_private_refs_skips_envs_on_non_cf_vaults() {
        // an env routed at the local vault → the `_ => continue`
        // arm — migration reports nothing, not a crash.
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            let (_h, mut ops) = ops_with_vault(&Env::dev(), &[]);
            ops.config.envs.insert(Env::dev(), "local".into());
            let e = ops.migrate_private_refs().unwrap_err();
            assert!(e.to_string().contains("nothing to migrate"), "{e}");
        });
    }

    #[test]
    fn explicit_identity_file_env_var_pins_the_source() {
        temp_env::with_var("TAMIAS_IDENTITY_FILE", Some("/tmp/tamias-id-x"), || {
            assert!(explicit_identity_file(None).is_some());
        });
        temp_env::with_var("TAMIAS_IDENTITY_FILE", Some(""), || {
            assert!(explicit_identity_file(None).is_none());
        });
    }

    #[test]
    fn load_required_guides_when_empty_and_quotes_the_source_on_err() {
        let empty = StubSource::empty();
        let e = load_required(&empty).err().unwrap();
        assert!(e.to_string().contains("run `tamias init`"), "{e}");
        let dead = StubSource::failing(IdentityError::Store("dead".into()));
        let e = load_required(&dead).err().unwrap();
        assert!(e.to_string().contains("identity via stub source"), "{e}");
    }

    #[test]
    fn stub_source_surfaces_every_error_and_names_itself() {
        for err in [
            IdentityError::Store("s".into()),
            IdentityError::Unavailable("u".into()),
            IdentityError::Parse("p".into()),
            IdentityError::Io("i".into()),
        ] {
            let s = StubSource::failing(err);
            assert!(s.load().is_err());
        }
        let s = StubSource::empty();
        assert_eq!(s.kind(), "stub");
        assert_eq!(s.describe(), "stub source");
        let id = x25519::Identity::generate();
        s.store(&id).unwrap();
        assert!(s.load().unwrap().is_some());
    }

    #[test]
    fn init_in_reports_a_keyring_held_identity() {
        let id = x25519::Identity::generate();
        let keyring = StubSource::holding(id.clone());
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            let ops = mk_ops(Config::default());
            temp_env::with_var_unset("TAMIAS_IDENTITY_FILE", || {
                let info = ops.init_in(&InitOpts::default(), Some(&keyring)).unwrap();
                assert_eq!(info.identity, keyring.describe());
                assert!(!info.migrated);
            });
        });
    }

    #[test]
    fn init_falls_back_to_file_when_the_keyring_is_unreachable() {
        // platform() probes the session store — absent on headless
        // CI — the Err arm degrades to the file, never a crash.
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_var_unset("TAMIAS_IDENTITY_FILE", || {
                let ops = mk_ops(Config::default());
                let info = ops.init(&InitOpts::default()).unwrap();
                assert!(info.identity.ends_with("identity.txt"));
            });
        });
    }

    impl IdentitySource for StubSource {
        fn load(&self) -> Result<Option<x25519::Identity>, IdentityError> {
            match &self.err {
                Some(IdentityError::Store(m)) => Err(IdentityError::Store(m.clone())),
                Some(IdentityError::Unavailable(m)) => Err(IdentityError::Unavailable(m.clone())),
                Some(IdentityError::Parse(m)) => Err(IdentityError::Parse(m.clone())),
                Some(IdentityError::Io(m)) => Err(IdentityError::Io(m.clone())),
                None => Ok(self.stored.lock().unwrap().clone()),
            }
        }

        fn store(&self, id: &x25519::Identity) -> Result<(), IdentityError> {
            *self.stored.lock().unwrap() = Some(id.clone());
            Ok(())
        }

        fn kind(&self) -> &'static str {
            "stub"
        }

        fn describe(&self) -> String {
            "stub source".into()
        }
    }

    #[test]
    fn resolve_identity_prefers_keyring_then_file() {
        let home = tempfile::tempdir().unwrap();
        // a keyring holding identity A wins over the default file
        let a = x25519::Identity::generate();
        let keyring = StubSource::holding(a.clone());
        isolate_home(&home, || {
            temp_env::with_var_unset("TAMIAS_IDENTITY_FILE", || {
                let ops = mk_ops(Config::default());
                let got = ops.resolve_identity_in(None, Some(&keyring)).unwrap();
                assert_eq!(got.to_public(), a.to_public());
            });
        });
        // an empty keyring falls through to the default keyfile
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_var("TAMIAS_IDENTITY_FILE", None::<&str>, || {
                // seed the default-path keyfile (XDG under the
                // isolated home)
                let default = FileIdentity::default();
                let (file_id, _) = tamias_identity::ensure(&default).unwrap();
                let ops = mk_ops(Config::default());
                let keyring = StubSource::empty();
                let got = ops.resolve_identity_in(None, Some(&keyring)).unwrap();
                assert_eq!(got.to_public(), file_id.to_public());
                // a failing keyring also falls back — note on
                // stderr, not a crash
                let keyring = StubSource::failing(IdentityError::Store("dead".into()));
                let got = ops.resolve_identity_in(None, Some(&keyring)).unwrap();
                assert_eq!(got.to_public(), file_id.to_public());
            });
        });
    }

    #[test]
    fn resolve_identity_explicit_file_wins_and_is_required() {
        let home = tempfile::tempdir().unwrap();
        let id_path = write_identity(home.path());
        let file_id = FileIdentity::new(&id_path).load().unwrap().unwrap();
        // explicit config path beats a populated keyring
        let mut config = Config::default();
        let mut v = vault_cfg("age", None);
        v.identity = Some(id_path.to_string_lossy().into_owned());
        config.vaults.insert("local".into(), v);
        let ops = mk_ops(config);
        let cfg = ops.config.vault_config("local");
        let a = x25519::Identity::generate();
        let keyring = StubSource::holding(a);
        let got = ops.resolve_identity_in(cfg, Some(&keyring)).unwrap();
        assert_eq!(got.to_public(), file_id.to_public());
        // env override beats config path — and a missing explicit
        // file is an error, never a fall-through
        temp_env::with_var(
            "TAMIAS_IDENTITY_FILE",
            Some(home.path().join("missing.txt").to_str().unwrap()),
            || {
                let e = ops
                    .resolve_identity_in(cfg, Some(&keyring))
                    .err()
                    .unwrap()
                    .to_string();
                assert!(e.contains("run `tamias init`"), "{e}");
            },
        );
    }

    #[test]
    fn resolve_identity_reports_missing_everywhere() {
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_var_unset("TAMIAS_IDENTITY_FILE", || {
                let ops = mk_ops(Config::default());
                let e = ops
                    .resolve_identity_in(None, None)
                    .err()
                    .unwrap()
                    .to_string();
                assert!(e.contains("run `tamias init`"), "{e}");
            });
        });
    }

    #[test]
    fn init_prefers_keyring_and_migrates_the_keyfile() {
        // fresh machine + reachable store → identity lands in the
        // keyring, nothing on disk
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_var_unset("TAMIAS_IDENTITY_FILE", || {
                let ops = mk_ops(Config::default());
                let keyring = StubSource::empty();
                let info = ops.init_in(&InitOpts::default(), Some(&keyring)).unwrap();
                assert_eq!(info.identity_backend, "keyring");
                assert!(info.identity_created);
                assert!(!info.migrated);
                assert!(keyring.load().unwrap().is_some());
            });
        });

        // an existing keyfile is imported — the migration path —
        // and kept on disk as backup
        let home = tempfile::tempdir().unwrap();
        let id_path = write_identity(home.path());
        isolate_home(&home, || {
            temp_env::with_var("TAMIAS_IDENTITY_FILE", None::<&str>, || {
                // make the default-path file visible
                let default = FileIdentity::default_path();
                std::fs::create_dir_all(default.parent().unwrap()).unwrap();
                std::fs::copy(&id_path, &default).unwrap();
                let file_id = FileIdentity::default().load().unwrap().unwrap();
                let ops = mk_ops(Config::default());
                let keyring = StubSource::empty();
                let info = ops.init_in(&InitOpts::default(), Some(&keyring)).unwrap();
                assert_eq!(info.identity_backend, "keyring");
                assert!(!info.identity_created);
                assert!(info.migrated);
                assert_eq!(info.file_remains, Some(default.clone()));
                let stored = keyring.load().unwrap().unwrap();
                assert_eq!(stored.to_public(), file_id.to_public());
            });
        });
    }

    #[test]
    fn init_falls_back_to_file_when_the_store_is_gone_or_broken() {
        // no store at all → file bootstrap
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_var_unset("TAMIAS_IDENTITY_FILE", || {
                let ops = mk_ops(Config::default());
                let info = ops.init_in(&InitOpts::default(), None).unwrap();
                assert_eq!(info.identity_backend, "file");
                assert!(info.identity_created);
                assert!(PathBuf::from(&info.identity).exists());
            });
        });
        // a reachable-but-broken store → same clean fallback
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_var_unset("TAMIAS_IDENTITY_FILE", || {
                let ops = mk_ops(Config::default());
                let keyring = StubSource::failing(IdentityError::Store("dbus gone".into()));
                let info = ops.init_in(&InitOpts::default(), Some(&keyring)).unwrap();
                assert_eq!(info.identity_backend, "file");
                assert!(info.identity_created);
            });
        });
        // --with-file skips the store even when one holds an
        // identity — and never migrates
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            temp_env::with_var_unset("TAMIAS_IDENTITY_FILE", || {
                let ops = mk_ops(Config::default());
                let keyring = StubSource::holding(x25519::Identity::generate());
                let info = ops
                    .init_in(&InitOpts { with_file: true }, Some(&keyring))
                    .unwrap();
                assert_eq!(info.identity_backend, "file");
                assert!(!info.migrated);
            });
        });
    }

    /// A `local` age vault + cf vault on `mock`, both under one
    /// identity in a tempdir. `tokens` seeds the local vault's
    /// `dev` keyspace.
    fn ops_on_cf_v2(url: &str, tokens: &[&str], private_refs: bool) -> (tempfile::TempDir, Ops) {
        let home = tempfile::tempdir().unwrap();
        let id_path = write_identity(home.path());
        let identity = FileIdentity::new(&id_path).load().unwrap().unwrap();
        let vault_path = home.path().join("vault.age");
        let local = AgeVault::open(&vault_path, identity, &[]).unwrap();
        for r in tokens {
            Vault::put(
                &local,
                &Env::dev(),
                &Ref::new(r).unwrap(),
                Secret::new("fake-token-value"),
            )
            .unwrap();
        }
        let mut config = Config::default();
        let mut local_cfg = vault_cfg("age", None);
        local_cfg.path = Some(vault_path.to_string_lossy().into_owned());
        local_cfg.identity = Some(id_path.to_string_lossy().into_owned());
        config.vaults.insert("local".into(), local_cfg);
        let mut cf = vault_cfg("cf", Some(url));
        cf.identity = Some(id_path.to_string_lossy().into_owned());
        cf.private_refs = private_refs;
        config.vaults.insert("cloud".into(), cf);
        config.envs.insert(Env::dev(), "cloud".into());
        (home, mk_ops(config))
    }

    #[test]
    fn scoped_tokens_sign_requests_from_local_vault_secrets() {
        use tamias_testkit::mocks::{self, ScopedToken};
        let tokens = &[ScopedToken {
            id: "ops/cf-token-dev",
            secret: "fake-token-value",
            scopes: &[("dev", true, true)],
        }];
        let mock = mocks::vault_wire_v2(tokens, None);
        let (_h, mut ops) = ops_on_cf_v2(mock.url(), &["tamias://ops/cf-token-dev"], false);
        // wire the [vaults.cloud.tokens] table at the ref
        ops.config
            .vaults
            .get_mut("cloud")
            .unwrap()
            .tokens
            .insert(Env::dev(), Ref::new("tamias://ops/cf-token-dev").unwrap());
        // every verb on dev authenticates TamiasHmac — the mock
        // verifies the signature against the local-vault secret.
        temp_env::with_var_unset("TAMIAS_CF_TOKEN", || {
            let r = Ref::new("tamias://x/y").unwrap();
            ops.put(&r, Secret::new("fake")).unwrap();
            assert!(ops.get(&r).unwrap().is_some());
            let (_, refs) = ops.list(None).unwrap();
            assert_eq!(refs, vec![Ref::new("tamias://x/y").unwrap()]);
            assert!(ops.rm(&r).unwrap());
        });
        // a token ref the local vault doesn't hold → config error
        ops.config.vaults.get_mut("cloud").unwrap().tokens.insert(
            Env::new("prod").unwrap(),
            Ref::new("tamias://ops/not-there").unwrap(),
        );
        let e = ops.open_vault_named("cloud").err().unwrap().to_string();
        assert!(e.contains("not stored"), "{e}");
    }

    #[test]
    fn migrate_private_refs_via_ops() {
        use tamias_testkit::mocks;
        let mock = mocks::vault_wire("ops-fake-token");
        // private_refs off at first → rows land as plaintext keys.
        let (_h, mut ops) = ops_on_cf_v2(mock.url(), &[], false);
        temp_env::with_var("TAMIAS_CF_TOKEN", Some("ops-fake-token"), || {
            let r = Ref::new("tamias://a/b").unwrap();
            ops.put(&r, Secret::new("fake")).unwrap();
            // flip the vault to private_refs and migrate — the row
            // is re-keyed and its name blob sealed to the same
            // identity, so the value still decrypts end-to-end.
            ops.config.vaults.get_mut("cloud").unwrap().private_refs = true;
            let reports = ops.migrate_private_refs().unwrap();
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].env.as_str(), "dev");
            assert_eq!(reports[0].vault, "cloud");
            assert_eq!(
                reports[0].rewritten,
                vec![Ref::new("tamias://a/b").unwrap()]
            );
            assert_eq!(reports[0].already_private, 0);
            let (_, refs) = ops.list(None).unwrap();
            assert_eq!(refs, vec![r.clone()]);
            assert_eq!(ops.get(&r).unwrap().unwrap().value.expose(), "fake");
        });
    }

    #[test]
    fn migrate_errors_without_a_qualifying_cf_vault() {
        // no cf vault routed at all
        let ops = mk_ops(Config::default());
        let e = ops.migrate_private_refs().unwrap_err().to_string();
        assert!(e.contains("nothing to migrate"), "{e}");
        // cf routed but private_refs off
        let mut config = Config::default();
        config
            .vaults
            .insert("cloud".into(), vault_cfg("cf", Some("http://x")));
        config.envs.insert(Env::dev(), "cloud".into());
        let ops = mk_ops(config);
        let e = ops.migrate_private_refs().unwrap_err().to_string();
        assert!(e.contains("private_refs"), "{e}");
    }

    #[test]
    fn cf_tokens_cant_resolve_against_a_cf_local() {
        // [vaults.local] as cf + tokens on another cf vault → the
        // loop guard fires instead of recursing.
        let mut config = Config::default();
        config
            .vaults
            .insert("local".into(), vault_cfg("cf", Some("http://x")));
        let mut cf = vault_cfg("cf", Some("http://y"));
        cf.tokens
            .insert(Env::dev(), Ref::new("tamias://ops/tok").unwrap());
        config.vaults.insert("cloud".into(), cf);
        let ops = mk_ops(config);
        let e = ops.open_vault_named("cloud").err().unwrap().to_string();
        assert!(e.contains("itself kind"), "{e}");
    }

    #[test]
    fn check_flags_cf_only_fields_on_other_kinds() {
        let mut config = Config::default();
        let mut v = vault_cfg("age", None);
        v.private_refs = true;
        config.vaults.insert("local".into(), v);
        let mut v2 = vault_cfg("age", None);
        v2.tokens
            .insert(Env::dev(), Ref::new("tamias://ops/tok").unwrap());
        config.vaults.insert("other".into(), v2);
        let ops = mk_ops(config);
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            let report = ops.check();
            let has = |n: &str| report.problems.iter().any(|p| p.contains(n));
            assert!(has("private_refs only applies"), "{:?}", report.problems);
            assert!(has("tokens only apply"), "{:?}", report.problems);
        });
    }

    #[test]
    fn check_flags_orphaned_cache_ttl() {
        let mut config = Config::default();
        let mut v = vault_cfg("cf", Some("https://x.workers.dev"));
        v.cache_ttl_secs = Some(60);
        config.vaults.insert("cloud".into(), v);
        let ops = mk_ops(config);
        let home = tempfile::tempdir().unwrap();
        isolate_home(&home, || {
            let report = ops.check();
            assert!(
                report
                    .problems
                    .iter()
                    .any(|p| p.contains("cache_ttl_secs") && p.contains("cache is not")),
                "{:?}",
                report.problems
            );
        });
    }

    /// The env vars a cached-cf test needs — token + an isolated
    /// XDG cache home under `home`.
    fn cached_cf_env(home: &std::path::Path, f: impl FnOnce()) {
        temp_env::with_vars(
            [
                ("TAMIAS_CF_TOKEN", Some("ops-fake-token")),
                (
                    "XDG_CACHE_HOME",
                    Some(home.join("cache-home").to_str().unwrap()),
                ),
            ],
            f,
        );
    }

    #[test]
    fn cached_cf_vault_reads_through_and_serves_offline() {
        use tamias_testkit::mocks;
        let mock = mocks::vault_wire("ops-fake-token");
        let (home, mut ops) = ops_on_cf_v2(mock.url(), &[], false);
        ops.config.vaults.get_mut("cloud").unwrap().cache =
            Some(tamias_core::CacheScope::All(true));
        cached_cf_env(home.path(), || {
            let r = Ref::new("tamias://a/b").unwrap();
            ops.put(&r, Secret::new("fake")).unwrap();
            // write-through: the cache vault landed under the XDG
            // cache dir, per-vault
            let cache_file = home.path().join("cache-home/tamias/cloud.age");
            assert!(cache_file.exists());
            assert_eq!(ops.get(&r).unwrap().unwrap().value.expose(), "fake");
            let (_, refs) = ops.list(None).unwrap();
            assert_eq!(refs, vec![r.clone()]);
            // the mirror itself is the proof — the same ref, held
            // locally as ciphertext under the same identity
            let identity = FileIdentity::new(home.path().join("identity.txt"))
                .load()
                .unwrap()
                .unwrap();
            let cache = AgeVault::open(&cache_file, identity.clone(), &[]).unwrap();
            assert_eq!(
                cache.list(&Env::dev()).unwrap(),
                vec![Ref::new("tamias://a/b").unwrap()]
            );
            assert!(ops.rm(&r).unwrap());
            // rm invalidated the mirror too
            let cache = AgeVault::open(&cache_file, identity, &[]).unwrap();
            assert!(cache.list(&Env::dev()).unwrap().is_empty());
        });
    }

    #[test]
    fn corrupt_cache_file_is_rebuilt_not_fatal() {
        use tamias_testkit::mocks;
        let mock = mocks::vault_wire("ops-fake-token");
        let (home, mut ops) = ops_on_cf_v2(mock.url(), &[], false);
        ops.config.vaults.get_mut("cloud").unwrap().cache =
            Some(tamias_core::CacheScope::All(true));
        let cache_file = home.path().join("cache-home/tamias/cloud.age");
        std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        std::fs::write(&cache_file, b"garbage - not age ciphertext").unwrap();
        cached_cf_env(home.path(), || {
            let r = Ref::new("tamias://a/b").unwrap();
            // the unreadable cache is moved aside, rebuilt, and the
            // remote answers normally
            ops.put(&r, Secret::new("fake")).unwrap();
            assert_eq!(ops.get(&r).unwrap().unwrap().value.expose(), "fake");
            assert!(home.path().join("cache-home/tamias/cloud.corrupt").exists());
        });
    }

    #[test]
    fn cache_scope_keeps_unlisted_envs_off_the_disk() {
        use tamias_testkit::mocks;
        let mock = mocks::vault_wire("ops-fake-token");
        let (home, mut ops) = ops_on_cf_v2(mock.url(), &[], false);
        let cloud = ops.config.vaults.get_mut("cloud").unwrap();
        cloud.cache = Some(tamias_core::CacheScope::Envs(vec![Env::dev()]));
        // route prod at the same vault — its rows must stay remote
        ops.config
            .envs
            .insert(Env::new("prod").unwrap(), "cloud".into());
        cached_cf_env(home.path(), || {
            let prod_ref = Ref::new("tamias://prod/a/b").unwrap();
            ops.put(&prod_ref, Secret::new("fake")).unwrap();
            assert_eq!(ops.get(&prod_ref).unwrap().unwrap().value.expose(), "fake");
            let cache_file = home.path().join("cache-home/tamias/cloud.age");
            let identity = FileIdentity::new(home.path().join("identity.txt"))
                .load()
                .unwrap()
                .unwrap();
            let cache = AgeVault::open(&cache_file, identity, &[]).unwrap();
            // dev may land in the mirror; prod never does
            assert!(cache.list(&Env::new("prod").unwrap()).unwrap().is_empty());
        });
    }
}
