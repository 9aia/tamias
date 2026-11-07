//! tamias-vault-cf — the Cloudflare Worker + D1 vault impl.
//!
//! A [`Vault`] over HTTPS: a self-hosted Worker (source in
//! `worker/`) fronts a D1 table of opaque rows —
//! `(env, key) → {ciphertext, name, version, created_at,
//! rotated_at}`. **The Worker never sees plaintext.** Each value is
//! age-encrypted client-side to the configured recipients *before*
//! it leaves the process; the store holds and returns base64
//! ciphertext only. This is the same crypto boundary as
//! `tamias-vault-age` (ADR-0001) — a remote store is just a different
//! persistence medium — but with *per-secret* ciphertext so rows
//! map 1:1 to D1.
//!
//! ## Wire protocol (`tamias-vault-cf/2`)
//!
//! Two auth schemes; the Worker accepts both during migration
//! (P-003):
//!
//! - `Authorization: Bearer <token>` — legacy; the token travels on
//!   every request.
//! - `Authorization: TamiasHmac <token_id>, <ts>, <nonce>, <sig>` — the
//!   token *signs* instead of traveling:
//!   `sig = hex HMAC-SHA256(token, method+"\n"+path+"\n"+ts+"\n"+nonce+"\n"+sha256hex(body))`,
//!   `ts` unix seconds within a ±60s replay window. The `nonce`
//!   makes identical requests inside one second sign differently —
//!   a legit retry isn't a replay against the worker's `used_sig`
//!   table (the pre-nonce 3-field form stays accepted during
//!   migration).
//!
//! Tokens are scoped per env on the Worker side
//! (`TAMIAS_VAULT_TOKENS` maps `token_id → {env → {read, write}}`);
//! config carries `tamias://` refs resolved into values before the
//! vault is built — never inline tokens.
//!
//! | Verb | Path | Body → Response |
//! | --- | --- | --- |
//! | `GET` | `/v1/{env}` | `{"refs": [{"key": "a/b", "name": blob\|null}, …]}` |
//! | `GET` | `/v1/{env}/{key}` | `{"ciphertext": "<b64>", "name": blob\|null, "version": n, "created_at": rfc3339, "rotated_at": rfc3339\|null}` |
//! | `PUT` | `/v1/{env}/{key}` | `{"ciphertext": "<b64>", "name": blob\|null, "rotate": bool}` → record metadata; `rotate:true` on a missing key → 404 |
//! | `DELETE` | `/v1/{env}/{key}` | `{"removed": bool}` |
//!
//! `{key}` is the storage key: the bare ref path (`billing/stripe`)
//! or — with `private_refs` (P-004) — `hex HMAC-SHA256(k, env+"/"+ref)`
//! where `k` is derived from the age identity. Private rows carry a
//! `name` blob (age ciphertext of the real ref) so `list` can map
//! keys back; a store compromise then yields ciphertext and opaque
//! hex, not which secrets exist.
//!
//! Bookkeeping is **server-side**: the Worker bumps `version`,
//! preserves `created_at`, and stamps `rotated_at` on rotate, so
//! concurrent writers can't race client-computed versions.
//!
//! HTTP uses `ureq` (blocking, no async runtime) per AGENTS.md.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::str::FromStr;
use std::time::Duration;

use age::secrecy::ExposeSecret;
use age::x25519;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tamias_core::{Env, Ref, Secret};
use tamias_vault::{SecretRecord, Vault, VaultError};
use time::OffsetDateTime;
use ureq::Agent;

/// HMAC-SHA256 — request signing (P-003) and ref_key derivation
/// (P-004).
type HmacSha256 = Hmac<Sha256>;

/// Domain label for the private-refs derivation — a versioned tag
/// so derived keys can't be confused with any other use of the
/// identity's key material.
const PRIVATE_REFS_DOMAIN: &[u8] = b"tamias-cf/private-refs/v1";

/// Response bodies are vault rows and listings — a handful of KB.
/// 4 MiB is generous headroom; an unbounded read lets a hostile or
/// broken endpoint exhaust memory (L7).
const MAX_RESPONSE_BODY: u64 = 4 * 1024 * 1024;

/// One row of the wire protocol — `ciphertext` present only on the
/// single-record GET, `name` is the private-refs blob (`null` on
/// plain rows).
#[derive(Debug, Deserialize)]
struct Row {
    ciphertext: Option<String>,
    #[serde(default)]
    name: Option<String>,
    version: u64,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    rotated_at: Option<OffsetDateTime>,
}

/// One `refs` entry — untagged so a v1 worker's bare-string shape
/// still parses.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ListItem {
    /// wire v1 shape — the bare ref path.
    Plain(String),
    /// wire v2 — storage key plus the private-refs name blob.
    Keyed { key: String, name: Option<String> },
}

#[derive(Debug, Deserialize)]
struct ListBody {
    refs: Vec<ListItem>,
}

#[derive(Debug, Deserialize)]
struct RmBody {
    removed: bool,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

/// How the client authenticates a request.
///
/// `Debug` is hand-rolled and redacts — a derived impl would print
/// `Bearer("…")`/`secret` verbatim into any `{cfg:?}` dump.
#[derive(Clone)]
pub enum Auth {
    /// `Bearer <token>` — legacy; the token travels on each request.
    Bearer(String),
    /// `TamiasHmac <id>, <ts>, <nonce>, <sig>` — the token signs,
    /// never travels (P-003 phase 2). `id` is the wire `token_id` —
    /// the key under which the Worker's `TAMIAS_VAULT_TOKENS` map
    /// holds this token.
    Hmac {
        /// The wire `token_id` (the scoped ref's path, by convention).
        id: String,
        /// The token secret — the HMAC key. Never on the wire.
        secret: String,
    },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Bearer(_) => f.write_str("Bearer(***)"),
            Auth::Hmac { id, .. } => f
                .debug_struct("Hmac")
                .field("id", id)
                .field("secret", &"***")
                .finish(),
        }
    }
}

/// A resolved cf-vault configuration — mirrors `[vaults.<name>]
/// kind = "cf"` after ops has resolved `tamias://` token refs into
/// credential values (no refs, no plaintext tokens in config).
pub struct CfVaultConfig {
    /// Worker base URL, e.g. `https://tamias-vault.you.workers.dev`.
    /// Must be `https://` (loopback `http://` is allowed for
    /// tests) — `./tamias.toml` is the trust root for this endpoint.
    pub url: String,
    /// Env var holding the legacy bearer token — the fallback
    /// credential. The *name* is config; the value is process env.
    pub token_env: Option<String>,
    /// Credentials by env name (`"*"` is the fallback for unscoped
    /// envs) — token values already resolved.
    pub scoped: BTreeMap<String, Auth>,
    /// The already-resolved age identity the vault decrypts with —
    /// where it lives (keyfile or keyring) is the `tamias-identity`
    /// port's business; ops resolves it before building this
    /// config (the same contract as `AgeVault::open`).
    pub identity: x25519::Identity,
    /// Extra `age1…` recipients encrypted to alongside the identity.
    pub extra_recipients: Vec<String>,
    /// P-004: derive storage keys + carry name blobs.
    pub private_refs: bool,
}

// `x25519::Identity` isn't `Debug` and shouldn't print anyway —
// the Debug impl shows its public recipient instead.
impl std::fmt::Debug for CfVaultConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfVaultConfig")
            .field("url", &self.url)
            .field("token_env", &self.token_env)
            .field("scoped", &self.scoped)
            .field("identity", &self.identity.to_public())
            .field("extra_recipients", &self.extra_recipients)
            .field("private_refs", &self.private_refs)
            .finish()
    }
}

/// What [`CfVault::migrate_private_refs`] did.
#[derive(Debug, Default)]
pub struct MigrateReport {
    /// Refs re-keyed (plaintext key → derived ref_key).
    pub rewritten: Vec<Ref>,
    /// Records already private (a `name` blob was present).
    pub already_private: usize,
}

/// A [`Vault`] backed by the Worker + D1 service.
pub struct CfVault {
    agent: Agent,
    base: String,
    /// env name (or `"*"` fallback) → credential.
    auth: BTreeMap<String, Auth>,
    identity: x25519::Identity,
    recipients: Vec<x25519::Recipient>,
    /// Derived only when `private_refs` is on — `HMAC(identity,
    /// PRIVATE_REFS_DOMAIN)`, the PRF key for storage keys.
    private_key: Option<[u8; 32]>,
}

impl CfVault {
    /// Build a vault from resolved config values. Construction does
    /// **no** network I/O — the first verb does.
    ///
    /// `token_env` names the env var holding the legacy bearer
    /// token; when set but empty that's a config error, and when
    /// unset it errors *only* if `scoped` is empty too (scoped
    /// tokens can be the whole credential set). `cfg.identity` is
    /// the already-resolved age identity — where it lives is the
    /// `tamias-identity` port's business.
    pub fn new(cfg: &CfVaultConfig) -> Result<Self, VaultError> {
        let mut auth = cfg.scoped.clone();
        if let Some(var) = &cfg.token_env {
            match std::env::var(var) {
                Ok(t) if t.is_empty() => {
                    return Err(VaultError::Config(format!(
                        "env var {var} is empty — cf vault tokens come via env, never config"
                    )));
                }
                Ok(t) => {
                    auth.insert("*".into(), Auth::Bearer(t));
                }
                Err(_) if auth.is_empty() => {
                    return Err(VaultError::Config(format!(
                        "env var {var} is unset — cf vault tokens come via env, never config"
                    )));
                }
                Err(_) => {} // scoped creds cover every env — no fallback needed
            }
        }
        if auth.is_empty() {
            return Err(VaultError::Config(
                "cf vault has no credentials — set token_env or [vaults.*.tokens]".into(),
            ));
        }
        Self::with_auth(
            &cfg.url,
            auth,
            cfg.identity.clone(),
            &cfg.extra_recipients,
            cfg.private_refs,
        )
    }

    /// Build from an already-parsed identity + a single bearer
    /// token — the test and embedding path.
    pub fn with_identity(
        url: &str,
        token: &str,
        identity: x25519::Identity,
        extra_recipients: &[String],
    ) -> Result<Self, VaultError> {
        Self::with_auth(
            url,
            BTreeMap::from([("*".to_string(), Auth::Bearer(token.to_string()))]),
            identity,
            extra_recipients,
            false,
        )
    }

    /// Full constructor: credential map + private-refs flag.
    /// `auth` keys are env names; `"*"` is the fallback.
    ///
    /// `url` must be `https://` — bearer tokens and HMAC-signed
    /// ciphertext cross here, so plaintext HTTP is refused unless
    /// the host is loopback (`127.0.0.0/8`, `::1`, `localhost`),
    /// the test/mock case. `./tamias.toml` is the trust root: it
    /// chooses the endpoint credentials are sent to.
    pub fn with_auth(
        url: &str,
        auth: BTreeMap<String, Auth>,
        identity: x25519::Identity,
        extra_recipients: &[String],
        private_refs: bool,
    ) -> Result<Self, VaultError> {
        validate_base_url(url)?;
        if auth.is_empty() {
            return Err(VaultError::Config(
                "cf vault has no credentials — set token_env or [vaults.*.tokens]".into(),
            ));
        }
        let mut recipients = vec![identity.to_public()];
        for r in extra_recipients {
            recipients.push(
                x25519::Recipient::from_str(r)
                    .map_err(|e| VaultError::Config(format!("invalid recipient {r:?}: {e}")))?,
            );
        }
        let private_key = private_refs.then(|| derive_private_key(&identity));
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .into();
        Ok(CfVault {
            agent,
            base: url.trim_end_matches('/').to_string(),
            auth,
            identity,
            recipients,
            private_key,
        })
    }

    /// The credential for `env` — a scoped entry wins over the
    /// `"*"` fallback; neither is a config error.
    fn cred_for(&self, env: &Env) -> Result<&Auth, VaultError> {
        self.auth
            .get(env.as_str())
            .or_else(|| self.auth.get("*"))
            .ok_or_else(|| {
                VaultError::Config(format!(
                    "no cf token scoped for env {env} — add a [vaults.*.tokens] \
                     entry or a fallback token_env",
                    env = env.as_str()
                ))
            })
    }

    /// `Authorization` header value for one request — Bearer
    /// verbatim, or an `TamiasHmac` signature over
    /// `method\npath\nts\nnonce\nsha256hex(body)` (wire v2). The
    /// nonce makes a same-second retry a *different* signature —
    /// without it the worker's used_sig replay guard 401s an
    /// identical second request (F7).
    fn auth_header(
        &self,
        env: &Env,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<String, VaultError> {
        match self.cred_for(env)? {
            Auth::Bearer(t) => Ok(format!("Bearer {t}")),
            Auth::Hmac { id, secret } => {
                let ts = unix_ts();
                let nonce = request_nonce();
                let msg = format!("{method}\n{path}\n{ts}\n{nonce}\n{}", sha256_hex(body));
                let sig = hex_encode(&hmac_sha256(secret.as_bytes(), msg.as_bytes()));
                Ok(format!("TamiasHmac {id}, {ts}, {nonce}, {sig}"))
            }
        }
    }

    /// The storage key for `rref` in `env`: the ref path, or the
    /// private-refs `ref_key` — `hex HMAC-SHA256(k, env+"/"+ref)`,
    /// one 64-hex segment, opaque to the store (P-004).
    fn storage_key(&self, env: &Env, rref: &Ref) -> String {
        match &self.private_key {
            Some(key) => hex_encode(&hmac_sha256(
                key,
                format!("{}/{}", env.as_str(), rref.path()).as_bytes(),
            )),
            None => rref.path(),
        }
    }

    /// `{base}/v1/{env}/{key}` — ref segments and derived keys are
    /// URL-safe by construction (`[A-Za-z0-9._-]` / hex), no
    /// escaping needed.
    fn item_url(&self, env: &Env, key: &str) -> String {
        format!("{}/v1/{}/{}", self.base, env.as_str(), key)
    }

    /// The private-refs `name` blob for a ref: age ciphertext of the
    /// real ref path, sealed to the vault's recipients. `None` on
    /// plain rows.
    fn name_blob(&self, rref: &Ref) -> Option<String> {
        self.private_key.as_ref().map(|_| {
            base64::engine::general_purpose::STANDARD.encode(seal(&self.recipients, &rref.path()))
        })
    }

    /// One HTTP round-trip. `http_status_as_error(false)` so error
    /// bodies are readable; transport failure is `VaultError::Io`.
    /// (ureq is typestate: GET/DELETE and body-carrying verbs are
    /// different builder types, so each arm is self-contained.)
    fn request(
        &self,
        env: &Env,
        method: &str,
        url: &str,
        body: Option<String>,
    ) -> Result<(u16, String), VaultError> {
        let io = |e: ureq::Error| VaultError::Io(format!("{method} {url}: {e}"));
        let has_body = body.is_some();
        let auth = self.auth_header(
            env,
            method,
            url_path(url),
            body.as_deref().unwrap_or("").as_bytes(),
        )?;
        let mut res = match (method, body) {
            ("GET", None) => self
                .agent
                .get(url)
                .header("Authorization", &auth)
                .config()
                .http_status_as_error(false)
                .build()
                .call()
                .map_err(io),
            ("DELETE", None) => self
                .agent
                .delete(url)
                .header("Authorization", &auth)
                .config()
                .http_status_as_error(false)
                .build()
                .call()
                .map_err(io),
            ("PUT", Some(b)) => self
                .agent
                .put(url)
                .header("Authorization", &auth)
                .config()
                .http_status_as_error(false)
                .build()
                .content_type("application/json")
                .send(b)
                .map_err(io),
            _ => {
                return Err(VaultError::Backend(format!(
                    "bad request shape {method} body={has_body}"
                )))
            }
        }?;
        let status = res.status().as_u16();
        let text = res
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BODY)
            .read_to_string()
            .map_err(|e| VaultError::Io(format!("{method} {url}: read body: {e}")))?;
        Ok((status, text))
    }

    /// Extract `{"error": "…"}` from a non-2xx body.
    fn error_msg(status: u16, body: &str) -> VaultError {
        if status == 401 || status == 403 {
            return VaultError::Backend(format!(
                "HTTP {status} — unauthorized; check the token env var"
            ));
        }
        let msg = serde_json::from_str::<ErrorBody>(body)
            .ok()
            .and_then(|e| e.error)
            .unwrap_or_else(|| body.chars().take(120).collect());
        VaultError::Backend(format!("HTTP {status}: {msg}"))
    }

    fn parse_row(body: &str) -> Result<Row, VaultError> {
        serde_json::from_str(body)
            .map_err(|e| VaultError::Backend(format!("worker returned bad row json: {e}")))
    }

    /// The raw `refs` listing — `(storage key, name blob)` pairs
    /// without the private-refs mapping or the mixed-mode refusal.
    /// `migrate` needs the unfiltered view; [`Vault::list`] maps.
    fn list_raw(&self, env: &Env) -> Result<Vec<(String, Option<String>)>, VaultError> {
        let url = format!("{}/v1/{}", self.base, env.as_str());
        let (status, body) = self.request(env, "GET", &url, None)?;
        match status {
            200 => {
                let out: ListBody = serde_json::from_str(&body).map_err(|e| {
                    VaultError::Backend(format!("worker returned bad list json: {e}"))
                })?;
                Ok(out
                    .refs
                    .into_iter()
                    .map(|item| match item {
                        ListItem::Plain(key) => (key, None),
                        ListItem::Keyed { key, name } => (key, name),
                    })
                    .collect())
            }
            other => Err(Self::error_msg(other, &body)),
        }
    }

    /// Decrypt a private-refs `name` blob back into a [`Ref`].
    fn open_name(&self, blob: &str) -> Result<Ref, VaultError> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(blob)
            .map_err(|e| VaultError::Backend(format!("bad b64 name blob: {e}")))?;
        let path = open(&self.identity, &raw).map_err(|e| {
            VaultError::Crypto(format!("name blob won't decrypt (mixed-mode vault?): {e}"))
        })?;
        Ref::from_path(&path)
            .map_err(|e| VaultError::Backend(format!("name blob held an invalid ref: {e}")))
    }

    /// `tamias migrate --private-refs` — re-key every plaintext-named
    /// row in `env` under its derived `ref_key`, attaching the
    /// `name` blob so `list` can still map back. The ciphertext
    /// moves verbatim — the value is never decrypted here. Already-
    /// private rows are skipped. Requires `private_refs = true` —
    /// the mode the rows are being rewritten *to*.
    pub fn migrate_private_refs(&self, env: &Env) -> Result<MigrateReport, VaultError> {
        if self.private_key.is_none() {
            return Err(VaultError::Config(
                "migrate --private-refs needs [vaults.*] private_refs = true".into(),
            ));
        }
        let mut report = MigrateReport::default();
        for (key, name) in self.list_raw(env)? {
            if name.is_some() {
                report.already_private += 1;
                continue;
            }
            let rref = Ref::from_path(&key).map_err(|e| {
                VaultError::Backend(format!("stored key {key:?} can't migrate: {e}"))
            })?;
            // Fetch the row verbatim — ciphertext only, no decrypt.
            let (status, body) = self.request(env, "GET", &self.item_url(env, &key), None)?;
            let ciphertext = match status {
                200 => Self::parse_row(&body)?
                    .ciphertext
                    .ok_or_else(|| VaultError::Backend("worker row missing ciphertext".into()))?,
                other => return Err(Self::error_msg(other, &body)),
            };
            // Write under the derived key first — the new row must
            // exist before the old one goes away.
            let new_key = self.storage_key(env, &rref);
            let put = serde_json::json!({
                "ciphertext": ciphertext,
                "name": self.name_blob(&rref),
                "rotate": false,
            })
            .to_string();
            let (status, body) =
                self.request(env, "PUT", &self.item_url(env, &new_key), Some(put))?;
            if status != 200 {
                return Err(Self::error_msg(status, &body));
            }
            let (status, body) = self.request(env, "DELETE", &self.item_url(env, &key), None)?;
            if status != 200 {
                return Err(Self::error_msg(status, &body));
            }
            report.rewritten.push(rref);
        }
        Ok(report)
    }
}

impl Vault for CfVault {
    fn get(&self, env: &Env, rref: &Ref) -> Result<Option<SecretRecord>, VaultError> {
        let key = self.storage_key(env, rref);
        let (status, body) = self.request(env, "GET", &self.item_url(env, &key), None)?;
        match status {
            200 => {
                let row = Self::parse_row(&body)?;
                // Private mode: the name blob must decrypt to the
                // ref we asked for — a wrong/missing name means a
                // mixed-mode vault or a lying store, never a value.
                if self.private_key.is_some() {
                    match &row.name {
                        Some(blob) => {
                            let named = self.open_name(blob)?;
                            if named != *rref {
                                return Err(VaultError::Backend(format!(
                                    "name blob says {named}, asked for {rref} — \
                                     the store is lying or mixed-mode"
                                )));
                            }
                        }
                        None => {
                            return Err(VaultError::Backend(
                                "mixed-mode vault: row has no name blob — \
                                 run `tamias migrate --private-refs`"
                                    .into(),
                            ));
                        }
                    }
                }
                let b64 = row
                    .ciphertext
                    .ok_or_else(|| VaultError::Backend("worker row missing ciphertext".into()))?;
                let blob = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .map_err(|e| VaultError::Backend(format!("bad b64 ciphertext: {e}")))?;
                let plain = open(&self.identity, &blob)?;
                Ok(Some(SecretRecord {
                    value: Secret::new(plain),
                    version: row.version,
                    created_at: row.created_at,
                    rotated_at: row.rotated_at,
                }))
            }
            404 => Ok(None),
            other => Err(Self::error_msg(other, &body)),
        }
    }

    fn put(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        self.write(env, rref, value, false)
    }

    fn rm(&self, env: &Env, rref: &Ref) -> Result<bool, VaultError> {
        let key = self.storage_key(env, rref);
        let (status, body) = self.request(env, "DELETE", &self.item_url(env, &key), None)?;
        match status {
            200 => {
                let out: RmBody = serde_json::from_str(&body).map_err(|e| {
                    VaultError::Backend(format!("worker returned bad rm json: {e}"))
                })?;
                Ok(out.removed)
            }
            other => Err(Self::error_msg(other, &body)),
        }
    }

    /// Refs in `env`, mapped back through `name` blobs when
    /// `private_refs` is on. **Fails closed on a mixed-mode vault**
    /// (P-004): private mode + a row with no name blob, or plain
    /// mode + a row with one, is a `VaultError`, never a silently
    /// wrong listing — run `tamias migrate --private-refs`.
    fn list(&self, env: &Env) -> Result<Vec<Ref>, VaultError> {
        let private = self.private_key.is_some();
        let mut refs = Vec::new();
        for (key, name) in self.list_raw(env)? {
            match (private, name) {
                (true, Some(blob)) => refs.push(self.open_name(&blob)?),
                (true, None) => {
                    return Err(VaultError::Backend(format!(
                        "mixed-mode vault: row {key:?} has no name blob — \
                         run `tamias migrate --private-refs`"
                    )));
                }
                (false, Some(_)) => {
                    return Err(VaultError::Backend(format!(
                        "row {key:?} was written with private_refs — set \
                         [vaults.*] private_refs = true to read it"
                    )));
                }
                (false, None) => refs.push(
                    Ref::from_path(&key)
                        .map_err(|e| VaultError::Backend(format!("stored key {e}")))?,
                ),
            }
        }
        refs.sort();
        Ok(refs)
    }

    fn rotate(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        self.write(env, rref, value, true)
    }
}

impl CfVault {
    /// PUT shared by `put` (`rotate:false`) and `rotate`
    /// (`rotate:true`); the server does the version bookkeeping.
    fn write(
        &self,
        env: &Env,
        rref: &Ref,
        value: Secret,
        rotate: bool,
    ) -> Result<SecretRecord, VaultError> {
        let blob = seal(&self.recipients, value.expose());
        let b64 = base64::engine::general_purpose::STANDARD.encode(blob);
        let key = self.storage_key(env, rref);
        let body = serde_json::json!({
            "ciphertext": b64,
            "name": self.name_blob(rref),
            "rotate": rotate,
        })
        .to_string();
        let (status, text) = self.request(env, "PUT", &self.item_url(env, &key), Some(body))?;
        match status {
            200 => {
                let row = Self::parse_row(&text)?;
                Ok(SecretRecord {
                    value,
                    version: row.version,
                    created_at: row.created_at,
                    rotated_at: row.rotated_at,
                })
            }
            404 => Err(VaultError::NotFound {
                rref: rref.path(),
                env: env.as_str().to_string(),
            }),
            other => Err(Self::error_msg(other, &text)),
        }
    }
}

/// `HMAC-SHA256(key, msg)` — the fixed 32-byte output. The expect
/// documents the invariant (HMAC accepts keys of any length), not a
/// reachable failure.
#[allow(clippy::unwrap_used)]
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&Sha256::digest(data))
}

/// Lowercase hex, no separators — the wire form for signatures and
/// storage keys.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Whole seconds since the epoch — the `ts` field of the TamiasHmac
/// scheme. A clock before the epoch degrades to 0, which simply
/// fails the worker's replay window.
fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `nonce` field of the TamiasHmac scheme — signed material that
/// makes two identical requests inside one `ts` second produce
/// different signatures, so a legit retry isn't a replay (F7). It
/// is *not* a secret (it travels in the header), only unique:
/// nanos + pid + a process-local sequence covers cross-process and
/// same-instant cases without pulling in an entropy dependency.
fn request_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{nanos:x}-{:x}-{:x}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// A base URL is trusted with credentials and ciphertext — it must
/// be `https://`, or `http://` to a loopback host (the test/mock
/// case). Anything else is a config error: `./tamias.toml` is the
/// trust root, and it must not be able to downgrade the wire.
fn validate_base_url(url: &str) -> Result<(), VaultError> {
    let host = url
        .strip_prefix("https://")
        .map(|h| (true, h))
        .or_else(|| url.strip_prefix("http://").map(|h| (false, h)))
        .and_then(|(secure, rest)| {
            let hostport = rest.split(['/', '?', '#']).next().unwrap_or_default();
            (!hostport.is_empty()).then_some((secure, hostport))
        });
    match host {
        Some((true, _)) => Ok(()),
        Some((false, hostport)) if is_loopback_host(hostport) => Ok(()),
        _ => Err(VaultError::Config(format!(
            "cf vault url {url:?} must be https:// (http:// is only \
             allowed for loopback hosts)"
        ))),
    }
}

/// `localhost`, `127.0.0.0/8`, or `[::1]` — with an optional `:port`.
fn is_loopback_host(hostport: &str) -> bool {
    if let Some(rest) = hostport.strip_prefix('[') {
        return rest.split(']').next() == Some("::1");
    }
    let host = hostport.split(':').next().unwrap_or_default();
    host.eq_ignore_ascii_case("localhost") || host.starts_with("127.")
}

/// The path component of `url` (no query) — what the TamiasHmac
/// signature covers. `request` only ever builds
/// `{base}/v1/…` URLs; this extracts the same `url.pathname` the
/// worker signs.
fn url_path(url: &str) -> &str {
    let after = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let path = match after.find('/') {
        Some(j) => &after[j..],
        None => "/",
    };
    &path[..path.find('?').unwrap_or(path.len())]
}

/// The private-refs PRF key: `HMAC-SHA256(identity_secret, domain)`.
/// Deriving from the identity means `ref_key`s move with the same
/// key that decrypts the values — and a different identity can't
/// compute or open them.
fn derive_private_key(identity: &x25519::Identity) -> [u8; 32] {
    hmac_sha256(
        identity.to_string().expose_secret().as_bytes(),
        PRIVATE_REFS_DOMAIN,
    )
}

/// Age-encrypt `plaintext` to `recipients` — per-secret ciphertext,
/// unlike `tamias-vault-age`'s whole-doc encryption (deliberate small
/// duplication rather than a shared crypto crate). In-memory
/// encryption cannot realistically fail — callers always pass ≥1
/// recipient and a `Vec` sink never errors — so the `expect`s
/// document invariants rather than guard reachable paths.
#[allow(clippy::unwrap_used)]
fn seal(recipients: &[x25519::Recipient], plaintext: &str) -> Vec<u8> {
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
            .expect("seal is only called with ≥1 recipient");
    let mut cipher = Vec::new();
    {
        let mut w = encryptor.wrap_output(&mut cipher).expect("in-memory wrap");
        w.write_all(plaintext.as_bytes()).expect("in-memory write");
        w.finish().expect("in-memory finish");
    }
    cipher
}

/// Decrypt one per-secret ciphertext blob with `identity`.
fn open(identity: &x25519::Identity, cipher: &[u8]) -> Result<String, VaultError> {
    let decryptor = age::Decryptor::new_buffered(cipher)
        .map_err(|e| VaultError::Crypto(format!("read ciphertext: {e}")))?;
    if decryptor.is_scrypt() {
        return Err(VaultError::Crypto(
            "passphrase-encrypted ciphertext; recipient key required".into(),
        ));
    }
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| VaultError::Crypto(format!("decrypt: {e}")))?;
    // Zeroizing so the plaintext scratch buffer is scrubbed on
    // drop — the returned String is moved out, not copied.
    let mut buf = zeroize::Zeroizing::new(String::new());
    reader.read_to_string(&mut buf)?;
    Ok(std::mem::take(&mut *buf))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn test_config(url: &str) -> CfVaultConfig {
        CfVaultConfig {
            url: url.to_string(),
            token_env: Some("TAMIAS_CF_TOKEN_TEST".into()),
            scoped: BTreeMap::new(),
            identity: x25519::Identity::generate(),
            extra_recipients: Vec::new(),
            private_refs: false,
        }
    }

    #[test]
    fn config_debug_shows_recipient_never_key_material() {
        let mut cfg = test_config("https://x.invalid");
        // canary credential values — a Debug dump must never carry
        // them, in any auth shape (HIGH-1)
        cfg.scoped
            .insert("dev".into(), Auth::Bearer("fake-bearer-canary".into()));
        cfg.scoped.insert(
            "prod".into(),
            Auth::Hmac {
                id: "ops/tok".into(),
                secret: "fake-hmac-canary".into(),
            },
        );
        let s = format!("{cfg:?}");
        assert!(s.contains("CfVaultConfig"), "{s}");
        assert!(s.contains("recipient"), "{s}");
        assert!(!s.contains("fake-bearer-canary"), "{s}");
        assert!(!s.contains("fake-hmac-canary"), "{s}");
        // the token_id is wire-visible metadata — it may show
        assert!(s.contains("ops/tok"), "{s}");
    }

    #[test]
    fn base_url_must_be_https_or_loopback_http() {
        let id = x25519::Identity::generate();
        let auth = || BTreeMap::from([("*".to_string(), Auth::Bearer("t".into()))]);
        // https anywhere is fine
        assert!(
            CfVault::with_auth("https://vault.example", auth(), id.clone(), &[], false).is_ok()
        );
        // http only on loopback — the test/mock case
        for ok in [
            "http://127.0.0.1:8787",
            "http://127.10.20.30",
            "http://localhost:8787",
            "http://LOCALHOST",
            "http://[::1]:8787",
        ] {
            assert!(
                CfVault::with_auth(ok, auth(), id.clone(), &[], false).is_ok(),
                "{ok}"
            );
        }
        // plaintext http off-loopback, missing scheme, and empty
        // host are all config errors — credentials must not cross
        for bad in [
            "http://vault.example",
            "http://example.com:8080/x",
            "vault.example",
            "ftp://127.0.0.1",
            "https://",
            "http://",
            "",
        ] {
            let e = CfVault::with_auth(bad, auth(), id.clone(), &[], false)
                .err()
                .unwrap_or_else(|| panic!("{bad} must be rejected"));
            assert!(e.to_string().contains("https://"), "{bad}: {e}");
        }
    }

    #[test]
    fn request_fails_fast_when_no_credential_scopes_the_env() {
        // cred_for's `?` — an env with no scoped token and no
        // "*" fallback is a clean Config error, not a request.
        let id = x25519::Identity::generate();
        let mut auth = BTreeMap::new();
        auth.insert("dev".to_string(), Auth::Bearer("fake".to_string()));
        let v = CfVault::with_auth("http://127.0.0.1:1", auth, id, &[], false).unwrap();
        let prod = Env::new("prod").unwrap();
        let e = v.request(&prod, "GET", "http://x", None).unwrap_err();
        assert!(e.to_string().contains("no cf token scoped"), "{e}");
    }

    #[test]
    fn seal_open_roundtrip_hides_value() {
        let id = x25519::Identity::generate();
        let blob = seal(&[id.to_public()], "fake-roundtrip");
        assert!(!String::from_utf8_lossy(&blob).contains("fake-roundtrip"));
        assert_eq!(open(&id, &blob).unwrap(), "fake-roundtrip");
        // a different identity can't open it
        let other = x25519::Identity::generate();
        assert!(open(&other, &blob).is_err());
    }

    #[test]
    fn open_rejects_scrypt_and_tampered_blobs() {
        let id = x25519::Identity::generate();
        // passphrase-encrypted → we hold no passphrase
        let scrypt = age::Encryptor::with_user_passphrase(age::secrecy::SecretString::from(
            "fake-pw".to_string(),
        ));
        let mut blob = Vec::new();
        {
            let mut w = scrypt.wrap_output(&mut blob).unwrap();
            w.write_all(b"x").unwrap();
            w.finish().unwrap();
        }
        let e = open(&id, &blob).unwrap_err();
        assert!(e.to_string().contains("passphrase"), "{e}");
        // not an age stream at all
        assert!(open(&id, b"garbage").is_err());
        // valid envelope but corrupted/truncated body → read fails
        let good = seal(&[id.to_public()], "fake-body");
        let cut = &good[..good.len() / 2];
        assert!(open(&id, cut).is_err());
    }

    #[test]
    fn request_rejects_unknown_method_body_shapes() {
        // `request` is the internal HTTP seam — a shape no caller
        // produces must be a clean error, never a panic
        let id = x25519::Identity::generate();
        let v = CfVault::with_identity("http://127.0.0.1:1", "t", id, &[]).unwrap();
        assert!(v.request(&Env::dev(), "PATCH", "http://x", None).is_err());
        assert!(v
            .request(&Env::dev(), "GET", "http://x", Some("{}".into()))
            .is_err());
        assert!(v.request(&Env::dev(), "PUT", "http://x", None).is_err());
        // transport death → the io map_err arm
        assert!(v
            .request(&Env::dev(), "GET", "http://127.0.0.1:1", None)
            .is_err());
    }

    #[test]
    fn request_serves_every_verb_shape() {
        use tamias_testkit::httpmock::{HttpMock, Request, Response};
        let mock = HttpMock::new(|_req: &Request| Response::json(200, "{}"));
        let id = x25519::Identity::generate();
        let v = CfVault::with_identity(mock.url(), "t", id, &[]).unwrap();
        for (method, body) in [("GET", None), ("DELETE", None), ("PUT", Some("{}".into()))] {
            let (status, _) = v.request(&Env::dev(), method, mock.url(), body).unwrap();
            assert_eq!(status, 200, "{method}");
        }
    }

    #[test]
    fn new_requires_credentials() {
        temp_env::with_var_unset("TAMIAS_CF_TOKEN_TEST", || {
            // unset, no scoped creds → error
            let mut cfg = test_config("https://x.invalid");
            let e = CfVault::new(&cfg).err().unwrap();
            assert!(e.to_string().contains("unset"), "{e}");
            // no token_env at all and no scoped → error
            cfg.token_env = None;
            let e = CfVault::new(&cfg).err().unwrap();
            assert!(e.to_string().contains("no credentials"), "{e}");
        });
        // set but empty → always an error
        temp_env::with_var("TAMIAS_CF_TOKEN_TEST", Some(""), || {
            let mut cfg = test_config("https://x.invalid");
            cfg.scoped.insert(
                "dev".into(),
                Auth::Hmac {
                    id: "ops/tok".into(),
                    secret: "fake".into(),
                },
            );
            let e = CfVault::new(&cfg).err().unwrap();
            assert!(e.to_string().contains("empty"), "{e}");
        });
        // unset but scoped creds exist → the fallback is optional
        temp_env::with_var_unset("TAMIAS_CF_TOKEN_TEST", || {
            let mut cfg = test_config("https://x.invalid");
            cfg.scoped.insert("dev".into(), Auth::Bearer("fake".into()));
            assert!(CfVault::new(&cfg).is_ok());
        });
    }

    #[test]
    fn new_and_with_identity_validate_inputs() {
        let id = x25519::Identity::generate();
        // a set token + resolved identity opens — no network I/O
        // yet (identity validation lives in the tamias-identity port)
        temp_env::with_var("TAMIAS_CF_TOKEN_TEST", Some("fake"), || {
            let cfg = test_config("https://x.invalid");
            assert!(CfVault::new(&cfg).is_ok());
        });
        // invalid extra recipient strings are config errors
        let e = CfVault::with_identity("https://x.invalid", "t", id.clone(), &["bad!!".into()])
            .err()
            .unwrap();
        assert!(e.to_string().contains("invalid recipient"), "{e}");
    }

    #[test]
    fn auth_header_shapes() {
        let id = x25519::Identity::generate();
        // bearer passes the token verbatim
        let v = CfVault::with_identity("https://h.invalid", "fake-tok", id.clone(), &[]).unwrap();
        let h = v.auth_header(&Env::dev(), "GET", "/v1/dev", b"").unwrap();
        assert_eq!(h, "Bearer fake-tok");
        // hmac: header is `TamiasHmac id, ts, nonce, sig` and the
        // signature recomputes over method+path+ts+nonce+body-sha
        let v = CfVault::with_auth(
            "https://h.invalid",
            BTreeMap::from([(
                "dev".to_string(),
                Auth::Hmac {
                    id: "ops/tok".into(),
                    secret: "fake-secret".into(),
                },
            )]),
            id,
            &[],
            false,
        )
        .unwrap();
        let h = v
            .auth_header(&Env::dev(), "PUT", "/v1/dev/a/b", b"{}")
            .unwrap();
        let parts: Vec<&str> = h
            .strip_prefix("TamiasHmac ")
            .unwrap()
            .split(",")
            .map(|s| s.trim())
            .collect();
        assert_eq!(parts.len(), 4, "{h}");
        assert_eq!(parts[0], "ops/tok");
        let ts: u64 = parts[1].parse().unwrap();
        assert!(ts > 0);
        assert!(!parts[2].is_empty(), "nonce present");
        let expect = hex_encode(&hmac_sha256(
            b"fake-secret",
            format!(
                "PUT\n/v1/dev/a/b\n{}\n{}\n{}",
                parts[1],
                parts[2],
                sha256_hex(b"{}")
            )
            .as_bytes(),
        ));
        assert_eq!(parts[3], expect);
        // a second header in the same second carries a different
        // nonce → a different signature (F7: retry ≠ replay)
        let h2 = v
            .auth_header(&Env::dev(), "PUT", "/v1/dev/a/b", b"{}")
            .unwrap();
        assert_ne!(h, h2);
        // an env with no scoped cred and no fallback → config error
        let e = v
            .auth_header(&Env::new("prod").unwrap(), "GET", "/v1/prod", b"")
            .unwrap_err();
        assert!(e.to_string().contains("no cf token"), "{e}");
    }

    #[test]
    fn url_path_extracts_the_signed_component() {
        assert_eq!(url_path("http://h/v1/dev/a/b"), "/v1/dev/a/b");
        assert_eq!(url_path("https://h:443/x?y=1"), "/x");
        assert_eq!(url_path("http://h"), "/");
        assert_eq!(url_path("/v1/dev"), "/v1/dev");
    }

    #[test]
    fn private_key_and_storage_key_are_deterministic() {
        let id = x25519::Identity::generate();
        let rref = Ref::new("tamias://billing/stripe").unwrap();
        let mk = |private: bool| {
            CfVault::with_auth(
                "https://h.invalid",
                BTreeMap::from([("*".to_string(), Auth::Bearer("t".into()))]),
                id.clone(),
                &[],
                private,
            )
            .unwrap()
        };
        let p = mk(true);
        let k1 = p.storage_key(&Env::dev(), &rref);
        assert_eq!(k1.len(), 64);
        assert!(k1.chars().all(|c| c.is_ascii_hexdigit()), "{k1}");
        // deterministic: same identity + env + ref → same key
        assert_eq!(k1, mk(true).storage_key(&Env::dev(), &rref));
        // env is part of the derivation
        assert_ne!(k1, p.storage_key(&Env::new("prod").unwrap(), &rref));
        // another identity → different key for the same ref
        let other = CfVault::with_auth(
            "https://h.invalid",
            BTreeMap::from([("*".to_string(), Auth::Bearer("t".into()))]),
            x25519::Identity::generate(),
            &[],
            true,
        )
        .unwrap();
        assert_ne!(k1, other.storage_key(&Env::dev(), &rref));
        // non-private mode is the bare ref path
        assert_eq!(mk(false).storage_key(&Env::dev(), &rref), "billing/stripe");
    }

    #[test]
    fn with_auth_rejects_empty_credentials() {
        let e = CfVault::with_auth(
            "https://x.invalid",
            BTreeMap::new(),
            x25519::Identity::generate(),
            &[],
            false,
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("no credentials"), "{e}");
    }

    #[test]
    fn open_name_rejects_garbage_and_foreign_blobs() {
        let id = x25519::Identity::generate();
        let v = CfVault::with_auth(
            "https://x.invalid",
            BTreeMap::from([("*".to_string(), Auth::Bearer("t".into()))]),
            id.clone(),
            &[],
            true,
        )
        .unwrap();
        // not base64 → backend error
        assert!(v.open_name("!!!").is_err());
        // base64 but not age → crypto error
        let e = v.open_name("Z2FyYmFnZQ==").unwrap_err();
        assert!(e.to_string().contains("name blob"), "{e}");
        // sealed to a different identity → can't open
        let other = x25519::Identity::generate();
        let blob =
            base64::engine::general_purpose::STANDARD.encode(seal(&[other.to_public()], "a/b"));
        assert!(v.open_name(&blob).is_err());
        // decrypts to a string that isn't a ref path → backend error
        let blob =
            base64::engine::general_purpose::STANDARD.encode(seal(&v.recipients, "bad key!"));
        let e = v.open_name(&blob).unwrap_err();
        assert!(e.to_string().contains("invalid ref"), "{e}");
        // and the happy path: our own blob names the ref
        let good = base64::engine::general_purpose::STANDARD.encode(seal(&v.recipients, "a/b"));
        assert_eq!(v.open_name(&good).unwrap().path(), "a/b");
    }

    #[test]
    fn error_msg_prefers_auth_hint_then_body() {
        // 401/403 get the token hint regardless of body
        for status in [401, 403] {
            let e = CfVault::error_msg(status, "{}");
            assert!(e.to_string().contains("unauthorized"), "{e}");
        }
        // a json error body is surfaced
        let e = CfVault::error_msg(500, r#"{"error":"d1 is down"}"#);
        assert!(e.to_string().contains("d1 is down"), "{e}");
        // non-json bodies degrade to a truncated dump
        let e = CfVault::error_msg(502, "not json at all");
        assert!(e.to_string().contains("not json at all"), "{e}");
    }

    #[test]
    fn open_surfaces_a_torn_body_and_bad_recipients() {
        let id = x25519::Identity::generate();
        let blob = seal(&[id.to_public()], "fake-value-long-enough-to-truncate");
        // valid header, torn body — decrypt starts, the read dies
        let torn = &blob[..blob.len() - 16];
        assert!(open(&id, torn).is_err());
        // an unparseable extra recipient → a config error, not a panic
        let e = CfVault::with_identity("https://x.invalid", "t", id, &["not-a-key".into()])
            .err()
            .unwrap();
        assert!(e.to_string().contains("invalid recipient"), "{e}");
    }
}
