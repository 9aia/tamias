//! The `tamias.toml`-shaped config model every `tamias-config` source
//! loads. Six tables:
//!
//! ```toml
//! [vaults.local]                 # named vault backends ("local" is
//! kind = "age"                   #   the built-in age default)
//!
//! [envs]                         # env → vault routing; unlisted
//! dev = { vault = "local" }      #   envs route to "local"
//!
//! [secrets]                      # the declared surface run may
//! STRIPE_KEY = "tamias://billing/stripe"   #   inject: VAR → ref
//!
//! [deploy.prod]                  # write-only push targets (sinks)
//! kind = "cf-secrets-store"
//!
//! [guardrails]                   # advisory op table — accident
//! default = "allow"              #   prevention, not policy
//! allow = ["run", "list"]
//! deny = ["put"]
//!
//! [serve.pdp]                    # `tamias serve`'s external policy
//! exec = "peregrin policy"       #   decision point — exec driver:
//! allow_on_error = false         #   request on stdin, verdict on
//! ```                            #   stdout; absent → fail closed
//!
//! Unknown keys are rejected so a typo fails loud instead of
//! silently doing nothing.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Env, Op, Ref};

/// The name of the built-in local (age) vault.
pub const LOCAL_VAULT: &str = "local";

/// The `tamias.toml` config value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    /// Named vault backends. `local` always exists implicitly (age,
    /// default paths); an entry only overrides it or adds more.
    #[serde(default)]
    pub vaults: BTreeMap<String, VaultConfig>,
    /// Env → vault routing. Envs not listed route to `local`;
    /// `dev`/`staging`/`prod` always exist.
    #[serde(default)]
    pub envs: BTreeMap<Env, EnvRoute>,
    /// The declared secret surface: env-var name → `tamias://` ref.
    /// Bounds what `run`/`env` may inject — least privilege, no
    /// ambient vault dump.
    #[serde(default)]
    pub secrets: BTreeMap<String, Ref>,
    /// Named deploy targets — write-only sinks (`tamias deploy`
    /// pushes a stored value in; nothing ever reads one back).
    #[serde(default)]
    pub deploy: BTreeMap<String, DeployConfig>,
    /// Advisory op guardrails — accident prevention for standalone
    /// use, honestly *not* a policy boundary (a locally claimed
    /// actor is spoofable by construction).
    #[serde(default)]
    pub guardrails: Option<Guardrails>,
    /// The `tamias serve` surface — ndjson request/response on
    /// stdio plus the optional policy decision point. Absent,
    /// serve runs with no PDP — same advisory posture as the
    /// single-shot CLI.
    #[serde(default)]
    pub serve: Option<ServeConfig>,
}

impl Config {
    /// The vault name an env routes to — `[envs] <env>` if present,
    /// else `local`.
    pub fn vault_for(&self, env: &Env) -> &str {
        self.envs.get(env).map(|r| r.vault()).unwrap_or(LOCAL_VAULT)
    }

    /// Does `name` denote a known env? Well-known envs plus every
    /// `[envs]` key — what makes `tamias://<name>/…` an env pin.
    pub fn is_known_env(&self, name: &str) -> bool {
        Env::WELL_KNOWN.contains(&name) || self.envs.keys().any(|e| e.as_str() == name)
    }

    /// The [`VaultConfig`] for a vault name, or `None` when the name
    /// isn't configured (`local` included — it has implicit
    /// defaults).
    pub fn vault_config(&self, name: &str) -> Option<&VaultConfig> {
        self.vaults.get(name)
    }

    /// The [`DeployConfig`] for a deploy target name.
    pub fn deploy_config(&self, name: &str) -> Option<&DeployConfig> {
        self.deploy.get(name)
    }
}

/// One `[vaults.<name>]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VaultConfig {
    /// Backend kind — `"age"` (built-in, local file) or `"cf"`
    /// (self-hosted Cloudflare Worker + D1, `tamias-vault-cf`).
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Vault file path override (age impl; `~` is *not* expanded —
    /// write the path in full).
    pub path: Option<String>,
    /// Age identity file override (`TAMIAS_IDENTITY_FILE` wins over
    /// this; see `tamias-ops`). `cf` vaults decrypt with the same
    /// keyfile — all vault impls store age ciphertext (ADR-0001).
    pub identity: Option<String>,
    /// Extra age recipients to encrypt to, beyond the identity's
    /// public key (`age1…` strings).
    #[serde(default)]
    pub recipients: Vec<String>,
    /// Worker base URL (`cf` impl), e.g.
    /// `https://tamias-vault.you.workers.dev`.
    pub url: Option<String>,
    /// Name of the env var holding the bearer token (`cf` impl) —
    /// the token *name* lives in config, the token *value* only in
    /// the process env.
    pub token_env: Option<String>,
    /// Per-env scoped credentials (`cf` impl, P-003): env → the
    /// `tamias://` ref of a token stored in the `local` vault. Each
    /// resolved token authenticates as `TamiasHmac <ref-path>, ts,
    /// nonce, sig` — the ref path is the wire `token_id`. Token
    /// *refs* live in config, never token values.
    #[serde(default)]
    pub tokens: BTreeMap<Env, Ref>,
    /// Opt-in private ref metadata (`cf` impl, P-004): storage keys
    /// become HMAC-derived hex and every record carries an
    /// age-encrypted `name` blob, so the remote store can't see
    /// which secrets exist. Default false; `tamias migrate
    /// --private-refs` rewrites existing rows.
    #[serde(default)]
    pub private_refs: bool,
    /// Read-through encrypted local cache in front of this vault
    /// (P-007): `true` caches every env; a name or list scopes it —
    /// `cache = "dev"` or `cache = ["dev", "staging"]`. Rows land
    /// at `$XDG_CACHE_HOME/tamias/<name>.age` as ordinary age
    /// ciphertext carrying the remote `version`/`rotated_at`
    /// verbatim; freshness is TTL-bounded (`cache_ttl_secs`) and
    /// writes are always remote-first. Meant for remote backends —
    /// `dev` may cache, `prod` should live-fetch.
    pub cache: Option<CacheScope>,
    /// Cache freshness window in seconds — a cached row or env
    /// listing younger than this answers without a remote
    /// round-trip; past it the remote is re-fetched and the cache
    /// updated. A remote *failure* falls back to the stale copy —
    /// the offline mode. Absent → 300.
    pub cache_ttl_secs: Option<u64>,
}

/// The `[vaults.*] cache` scope — which envs the read-through
/// cache may hold (P-007).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum CacheScope {
    /// `cache = true` caches every env; `false` is off (same as
    /// leaving the field out).
    All(bool),
    /// `cache = "dev"` — a single env.
    One(Env),
    /// `cache = ["dev", "staging"]` — a set of envs.
    Envs(Vec<Env>),
}

impl CacheScope {
    /// Is the cache on at all? `cache = false` and `cache = []`
    /// are off — equivalent to omitting the field.
    pub fn enabled(&self) -> bool {
        match self {
            CacheScope::All(b) => *b,
            CacheScope::One(_) => true,
            CacheScope::Envs(es) => !es.is_empty(),
        }
    }

    /// Does `env` fall inside the scope? (Assumes [`Self::enabled`].)
    pub fn caches(&self, env: &Env) -> bool {
        match self {
            CacheScope::All(b) => *b,
            CacheScope::One(e) => e == env,
            CacheScope::Envs(es) => es.contains(env),
        }
    }

    /// The env set the scope names — `None` = every env
    /// (`cache = true`), `Some(set)` = only those envs. The shape
    /// `tamias-vault-cached`'s `CachePolicy` consumes; kept here so
    /// the config model stays impl-agnostic.
    pub fn envs(&self) -> Option<BTreeSet<String>> {
        match self {
            CacheScope::All(true) => None,
            CacheScope::All(false) => Some(BTreeSet::new()),
            CacheScope::One(e) => Some(BTreeSet::from([e.as_str().to_string()])),
            CacheScope::Envs(es) => Some(es.iter().map(|e| e.as_str().to_string()).collect()),
        }
    }
}

fn default_kind() -> String {
    "age".into()
}

/// One `[deploy.<name>]` entry — a write-only push target.
///
/// Deploy targets are **sinks**, not sources: `tamias deploy` pushes a
/// vault-stored value in and nothing is ever read back (CF Secrets
/// Store values are write-only by design). Credentials always arrive
/// via the env var named by `token_env`, never inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeployConfig {
    /// Target kind — `"cf-secrets-store"` (Cloudflare account
    /// Secrets Store, `tamias-deploy-cf`).
    #[serde(default = "default_deploy_kind")]
    pub kind: String,
    /// Cloudflare account id (`cf-secrets-store`).
    pub account_id: Option<String>,
    /// Secrets Store id (`cf-secrets-store`).
    pub store_id: Option<String>,
    /// Name of the env var holding the API token — never the token
    /// itself (`cf-secrets-store` defaults to
    /// `CLOUDFLARE_API_TOKEN`).
    pub token_env: Option<String>,
    /// API base URL override — for tests, gateways, and
    /// self-hosted proxies. Defaults to
    /// `https://api.cloudflare.com/client/v4`.
    pub base_url: Option<String>,
}

fn default_deploy_kind() -> String {
    "cf-secrets-store".into()
}

/// How an env routes to its vault — `dev = "local"` shorthand or
/// `dev = { vault = "local" }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum EnvRoute {
    /// Bare vault name.
    Name(String),
    /// Table form — room for later per-env options.
    Table(EnvRouteTable),
}

/// The `{ vault = "…" }` table form of [`EnvRoute`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnvRouteTable {
    /// Vault name in `[vaults]` (or the implicit `local`).
    pub vault: String,
}

impl EnvRoute {
    /// The vault name either form routes to.
    pub fn vault(&self) -> &str {
        match self {
            EnvRoute::Name(v) => v,
            EnvRoute::Table(t) => &t.vault,
        }
    }
}

impl From<&str> for EnvRoute {
    fn from(vault: &str) -> Self {
        EnvRoute::Name(vault.to_string())
    }
}

impl From<String> for EnvRoute {
    fn from(vault: String) -> Self {
        EnvRoute::Name(vault)
    }
}

/// What an unlisted op falls back to in [`Guardrails`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum GuardDefault {
    /// Unlisted ops pass.
    #[default]
    Allow,
    /// Unlisted ops are refused.
    Deny,
}

/// Advisory op table — `[guardrails]` in `tamias.toml`.
///
/// Guardrails, not policy: they catch accidents ("agents get
/// `run`/`list`, never `get`/`put`") for standalone use. Real
/// enforcement is peregrin's job — `TAMIAS_ACTOR` is a self-claimed
/// string, spoofable by construction.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Guardrails {
    /// Fallback for ops in neither list.
    #[serde(default)]
    pub default: GuardDefault,
    /// Ops always allowed.
    #[serde(default)]
    pub allow: Vec<Op>,
    /// Ops always refused — deny wins over allow.
    #[serde(default)]
    pub deny: Vec<Op>,
    /// Refuse `-s VAR=tamias://ref` ad-hoc injection on `run`/`env`
    /// (MED-5). Without it, a posture like `deny = ["get"]` is
    /// sidestepped: `tamias run -s V=tamias://x -- printenv V` *is* a
    /// get. Declared `[secrets]` injection is unaffected — the
    /// declared surface is auditable config, not an ad-hoc grab.
    #[serde(default)]
    pub deny_adhoc: bool,
}

impl Guardrails {
    /// Advisory check: deny list wins, then allow list, then
    /// `default`.
    pub fn allows(&self, op: Op) -> bool {
        if self.deny.contains(&op) {
            return false;
        }
        if self.allow.contains(&op) {
            return true;
        }
        matches!(self.default, GuardDefault::Allow)
    }
}

/// One `[serve]` entry — `tamias serve`'s config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ServeConfig {
    /// The external policy decision point serve consults before
    /// every op that can materialize (`get`/`put`/`rm`/`rotate`/
    /// `list`/`check`). Absent, no consult happens — the advisory
    /// posture only.
    pub pdp: Option<PdpConfig>,
}

/// `[serve.pdp]` — where policy decisions come from (P-005).
///
/// The PDP is always *external* — peregrin's job, never tamias's.
/// One driver today: `exec`, a command run once per decision. The
/// request (`{actor, attestation?, op, env, ref}`) goes in on
/// stdin as one JSON line; the verdict comes back on stdout —
/// `{"allow": bool, "reason"?: string, "obligations"?: [string]}`.
/// Spawn failure, non-zero exit, or unparseable output all count
/// as *unreachable* — `allow_on_error` decides what that means.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PdpConfig {
    /// The exec driver command — run via `sh -c` once per
    /// decision, request JSON on stdin, verdict JSON on stdout.
    /// **Trust root**: the line authorizes shell exec; the child
    /// inherits only the `tamias-shell` env allowlist. `tamias serve`
    /// warns when this arrives via an implicit `./tamias.toml` —
    /// pin config with `--config`/`$TAMIAS_CONFIG`.
    pub exec: String,
    /// What an unreachable PDP means. `false` (default) fails
    /// **closed** — the op is denied; a policy engine that can't
    /// answer must not silently mean "allowed". `true` fails
    /// open — the op runs anyway and the audit event records the
    /// degraded verdict.
    #[serde(default)]
    pub allow_on_error: bool,
    /// Per-decision deadline in milliseconds — the child is
    /// killed and the consult counts as *unreachable* past it.
    /// A hung or flooding PDP must never freeze the serve loop;
    /// absent → 5000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_shape() {
        let text = r#"
            [vaults.local]
            kind = "age"
            recipients = ["age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqygh9"]

            [envs]
            dev = { vault = "local" }
            prod = "local"

            [secrets]
            STRIPE_KEY = "tamias://billing/stripe"

            [vaults.cloud]
            kind = "cf"
            url = "https://tamias-vault.example.workers.dev"
            token_env = "TAMIAS_CF_TOKEN"

            [deploy.prod]
            kind = "cf-secrets-store"
            account_id = "0000000000000000000000000000dead"
            store_id = "0000000000000000000000000000beef"

            [guardrails]
            default = "deny"
            allow = ["run", "list"]
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.vaults["local"].kind, "age");
        assert_eq!(cfg.vault_for(&Env::dev()), "local");
        assert_eq!(cfg.vault_for(&Env::new("ci").unwrap()), "local");
        assert!(cfg.is_known_env("prod"));
        assert!(cfg.is_known_env("staging")); // well-known, unconfigured
        assert!(!cfg.is_known_env("ci"));
        assert_eq!(
            cfg.secrets["STRIPE_KEY"].to_string(),
            "tamias://billing/stripe"
        );
        assert_eq!(cfg.vaults["cloud"].kind, "cf");
        assert_eq!(
            cfg.vaults["cloud"].token_env.as_deref(),
            Some("TAMIAS_CF_TOKEN")
        );
        assert!(cfg.vaults["cloud"].tokens.is_empty());
        assert!(!cfg.vaults["cloud"].private_refs);
        let d = &cfg.deploy["prod"];
        assert_eq!(d.kind, "cf-secrets-store");
        assert!(d.account_id.is_some() && d.token_env.is_none());
        let g = cfg.guardrails.unwrap();
        assert_eq!(g.default, GuardDefault::Deny);
        assert!(g.allows(Op::Run));
        assert!(g.allows(Op::List));
        assert!(!g.allows(Op::Get));
    }

    #[test]
    fn cf_tokens_and_private_refs_parse() {
        let text = r#"
            [vaults.cloud]
            kind = "cf"
            url = "https://tamias-vault.example.workers.dev"
            private_refs = true

            [vaults.cloud.tokens]
            dev = "tamias://ops/cf-token-dev"
            prod = "tamias://ops/cf-token-prod"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        let cloud = &cfg.vaults["cloud"];
        assert!(cloud.private_refs);
        assert_eq!(cloud.tokens.len(), 2);
        assert_eq!(
            cloud
                .tokens
                .get(&Env::new("prod").unwrap())
                .map(ToString::to_string)
                .as_deref(),
            Some("tamias://ops/cf-token-prod")
        );
    }

    #[test]
    fn cache_scope_parses_all_forms() {
        let text = r#"
            [vaults.all]
            kind = "cf"
            url = "https://x.workers.dev"
            cache = true
            cache_ttl_secs = 60

            [vaults.one]
            kind = "cf"
            url = "https://x.workers.dev"
            cache = "dev"

            [vaults.some]
            kind = "cf"
            url = "https://x.workers.dev"
            cache = ["dev", "staging"]

            [vaults.off]
            kind = "cf"
            url = "https://x.workers.dev"
            cache = false

            [vaults.plain]
            kind = "cf"
            url = "https://x.workers.dev"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        let dev = Env::dev();
        let prod = Env::new("prod").unwrap();
        let st = Env::new("staging").unwrap();
        let all = cfg.vaults["all"].cache.as_ref().unwrap();
        assert!(all.enabled() && all.caches(&dev) && all.caches(&prod));
        assert!(all.envs().is_none());
        assert_eq!(cfg.vaults["all"].cache_ttl_secs, Some(60));
        let one = cfg.vaults["one"].cache.as_ref().unwrap();
        assert!(one.enabled() && one.caches(&dev) && !one.caches(&prod));
        assert_eq!(one.envs().unwrap().len(), 1);
        let some = cfg.vaults["some"].cache.as_ref().unwrap();
        assert!(some.caches(&dev) && some.caches(&st) && !some.caches(&prod));
        let off = cfg.vaults["off"].cache.as_ref().unwrap();
        assert!(!off.enabled() && !off.caches(&dev));
        assert!(cfg.vaults["plain"].cache.is_none());
        assert!(cfg.vaults["plain"].cache_ttl_secs.is_none());
        // an invalid env name in the scope fails the parse
        assert!(toml::from_str::<Config>("[vaults.x]\ncache = \"NOT AN ENV\"\n").is_err());
    }

    #[test]
    fn env_route_from_string_is_a_name() {
        assert_eq!(EnvRoute::from("local".to_string()).vault(), "local");
        assert_eq!(EnvRoute::from("cloud").vault(), "cloud");
    }

    #[test]
    fn empty_is_valid_and_routes_local() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.vault_for(&Env::dev()), "local");
        assert!(cfg.is_known_env("prod"));
        assert!(cfg.guardrails.is_none());
    }

    #[test]
    fn unknown_keys_fail_loud() {
        assert!(toml::from_str::<Config>("project_dir = \"project\"").is_err());
        assert!(toml::from_str::<Config>("[envs]\nPROD = \"local\"").is_err());
        assert!(toml::from_str::<Config>("[secrets]\nX = \"not-a-ref\"").is_err());
    }

    #[test]
    fn serve_pdp_parses_and_defaults_fail_closed() {
        let cfg: Config = toml::from_str(
            r#"
            [serve.pdp]
            exec = "peregrin policy decide"
            "#,
        )
        .unwrap();
        let pdp = cfg.serve.unwrap().pdp.unwrap();
        assert_eq!(pdp.exec, "peregrin policy decide");
        assert!(!pdp.allow_on_error, "default is fail-closed");
        // allow_on_error parses when set; empty [serve] is valid
        let cfg: Config =
            toml::from_str("[serve.pdp]\nexec = \"x\"\nallow_on_error = true\n").unwrap();
        assert!(cfg.serve.unwrap().pdp.unwrap().allow_on_error);
        let cfg: Config = toml::from_str("[serve]\n").unwrap();
        assert!(cfg.serve.unwrap().pdp.is_none());
        // pdp without an exec command fails loud
        assert!(toml::from_str::<Config>("[serve.pdp]\nallow_on_error = true\n").is_err());
        // unknown keys fail loud here too
        assert!(toml::from_str::<Config>("[serve.pdp]\nexec = \"x\"\nurl = \"y\"\n").is_err());
    }

    #[test]
    fn guardrails_deny_wins() {
        let g = Guardrails {
            default: GuardDefault::Allow,
            allow: vec![Op::Get],
            deny: vec![Op::Get],
            deny_adhoc: false,
        };
        assert!(!g.allows(Op::Get));
        assert!(g.allows(Op::Put));
        // deny_adhoc parses; absent it defaults off
        let cfg: Config = toml::from_str("[guardrails]\ndeny_adhoc = true\n").unwrap();
        assert!(cfg.guardrails.unwrap().deny_adhoc);
    }
}
