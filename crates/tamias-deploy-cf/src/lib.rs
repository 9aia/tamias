//! tamias-deploy-cf — push secrets to a Cloudflare **account Secrets
//! Store** via the CF API v4 (`tamias-deploy` impl).
//!
//! The Secrets Store is the canonical write-only sink: values go in
//! and can never be fetched back — Workers bind to them directly.
//! `tamias deploy` pushes the *plaintext* value (decrypted at the ops
//! edge): the age-ciphertext boundary covers tamias's own vaults; an
//! external store that must serve values to runtimes is outside it.
//!
//! ## API surface used
//!
//! `GET`/`POST`/`PATCH`/`DELETE` under
//! `{base}/accounts/{account_id}/secrets_store/stores/{store_id}/secrets`
//! — CF envelope `{success, errors, result}` on every response.
//! Upsert = `GET ?search={name}` → `PATCH /{id}` when an exact-name
//! row exists, else `POST` a create body.
//!
//! The API token arrives via the env var named by `token_env`
//! (default `CLOUDFLARE_API_TOKEN`) — never inline in config.
//! Blocking `ureq`, no async runtime (AGENTS.md).

use std::time::Duration;

use tamias_core::{Env, Ref, Secret};
use tamias_deploy::{Deploy, DeployError, PushReport};
use ureq::Agent;

/// The default CF API base.
pub const DEFAULT_BASE: &str = "https://api.cloudflare.com/client/v4";

/// The default token env var when config doesn't name one.
pub const DEFAULT_TOKEN_ENV: &str = "CLOUDFLARE_API_TOKEN";

/// Response bodies are small CF envelopes — 4 MiB caps a hostile
/// or broken endpoint's memory pressure (L7).
const MAX_RESPONSE_BODY: u64 = 4 * 1024 * 1024;

/// A [`Deploy`] target backed by a CF account Secrets Store.
#[derive(Debug)]
pub struct CfSecretsStore {
    name: String,
    agent: Agent,
    base: String,
    auth: String,
    account_id: String,
    store_id: String,
}

impl CfSecretsStore {
    /// Build a target from config values. `token_env` names the env
    /// var holding a `Secrets Store Write` API token; `base_url`
    /// overrides the API base (tests, gateways).
    ///
    /// Construction does **no** network I/O — `push`/`remove` do.
    pub fn new(
        name: &str,
        account_id: &str,
        store_id: &str,
        token_env: &str,
        base_url: Option<&str>,
    ) -> Result<Self, DeployError> {
        let token = std::env::var(token_env).map_err(|_| {
            DeployError::Config(format!(
                "env var {token_env} is unset — deploy tokens come via env, never config"
            ))
        })?;
        if token.is_empty() {
            return Err(DeployError::Config(format!(
                "env var {token_env} is empty — deploy tokens come via env, never config"
            )));
        }
        Self::with_token(name, account_id, store_id, &token, base_url)
    }

    /// Build with an explicit token — the test/embedding path.
    ///
    /// `base_url` must be `https://` — a plaintext or attacker URL
    /// would receive the API token *and* every pushed value.
    /// Loopback `http://` is permitted for tests/mocks.
    pub fn with_token(
        name: &str,
        account_id: &str,
        store_id: &str,
        token: &str,
        base_url: Option<&str>,
    ) -> Result<Self, DeployError> {
        if let Some(base) = base_url {
            validate_base_url(base)?;
        }
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .into();
        Ok(CfSecretsStore {
            name: name.to_string(),
            agent,
            base: base_url
                .unwrap_or(DEFAULT_BASE)
                .trim_end_matches('/')
                .to_string(),
            auth: format!("Bearer {token}"),
            account_id: account_id.to_string(),
            store_id: store_id.to_string(),
        })
    }

    /// The sink-side name for `(env, rref)`:
    /// `{env}_{path}`, non-`[A-Za-z0-9_]` chars → `_`. Stable and
    /// collision-light; e.g. `(prod, billing/stripe)` →
    /// `prod_billing_stripe`.
    pub fn sink_name(env: &Env, rref: &Ref) -> String {
        let mut n = format!("{}_{}", env.as_str(), rref.path());
        n.make_ascii_lowercase();
        n.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn secrets_url(&self) -> String {
        format!(
            "{}/accounts/{}/secrets_store/stores/{}/secrets",
            self.base, self.account_id, self.store_id
        )
    }

    /// One JSON round-trip; returns `(status, body)`. (ureq is
    /// typestate: GET/DELETE and body-carrying verbs are different
    /// builder types, so each arm is self-contained.)
    fn request(
        &self,
        method: &str,
        url: &str,
        query: Option<(&str, &str)>,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, String), DeployError> {
        let io = |e: ureq::Error| DeployError::Io(format!("{method} {url}: {e}"));
        let has_body = body.is_some();
        let mut res = match (method, body) {
            ("GET", None) => {
                let rb = self.agent.get(url).header("Authorization", &self.auth);
                let rb = match query {
                    Some((k, v)) => rb.query(k, v),
                    None => rb,
                };
                rb.config()
                    .http_status_as_error(false)
                    .build()
                    .call()
                    .map_err(io)
            }
            ("DELETE", None) => self
                .agent
                .delete(url)
                .header("Authorization", &self.auth)
                .config()
                .http_status_as_error(false)
                .build()
                .call()
                .map_err(io),
            ("POST", Some(b)) => self
                .agent
                .post(url)
                .header("Authorization", &self.auth)
                .config()
                .http_status_as_error(false)
                .build()
                .content_type("application/json")
                .send(b.to_string())
                .map_err(io),
            ("PATCH", Some(b)) => self
                .agent
                .patch(url)
                .header("Authorization", &self.auth)
                .config()
                .http_status_as_error(false)
                .build()
                .content_type("application/json")
                .send(b.to_string())
                .map_err(io),
            _ => {
                return Err(DeployError::Backend(format!(
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
            .map_err(|e| DeployError::Io(format!("{method} {url}: read body: {e}")))?;
        Ok((status, text))
    }

    /// Parse the CF envelope; on failure surfaces the errors array.
    fn envelope(status: u16, body: &str) -> Result<serde_json::Value, DeployError> {
        if status == 401 || status == 403 {
            return Err(DeployError::Unauthorized(format!(
                "HTTP {status} — check the token env var and Secrets Store permissions"
            )));
        }
        let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
            DeployError::Backend(format!("CF returned non-json (HTTP {status}): {e}"))
        })?;
        if v["success"].as_bool() != Some(true) {
            let errs: Vec<String> = v["errors"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e["message"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            return Err(DeployError::Backend(format!(
                "CF API HTTP {status}: {}",
                if errs.is_empty() {
                    body.chars().take(200).collect()
                } else {
                    errs.join("; ")
                }
            )));
        }
        Ok(v)
    }

    /// The store-internal id of the secret named `name`, if any.
    fn find_id(&self, name: &str) -> Result<Option<String>, DeployError> {
        let url = self.secrets_url();
        let (status, body) = self.request("GET", &url, Some(("search", name)), None)?;
        let v = Self::envelope(status, &body)?;
        let found = v["result"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["name"].as_str() == Some(name))
            .and_then(|s| s["id"].as_str().map(str::to_string));
        Ok(found)
    }
}

/// A base URL receives the API token and every pushed value — it
/// must be `https://`, or `http://` to a loopback host (the
/// test/mock case). Anything else is a config error: `tamias.toml`
/// is the trust root and must not downgrade the wire.
fn validate_base_url(url: &str) -> Result<(), DeployError> {
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
        _ => Err(DeployError::Config(format!(
            "deploy base_url {url:?} must be https:// (http:// is only \
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

impl Deploy for CfSecretsStore {
    fn name(&self) -> &str {
        &self.name
    }

    fn push(&self, env: &Env, rref: &Ref, value: &Secret) -> Result<PushReport, DeployError> {
        let name = Self::sink_name(env, rref);
        match self.find_id(&name)? {
            Some(id) => {
                let url = format!("{}/{}", self.secrets_url(), id);
                let body = serde_json::json!({
                    "name": name,
                    "value": value.expose(),
                });
                let (status, text) = self.request("PATCH", &url, None, Some(body))?;
                Self::envelope(status, &text)?;
                Ok(PushReport {
                    name,
                    created: false,
                })
            }
            None => {
                let body = serde_json::json!([{
                    "name": name,
                    "value": value.expose(),
                    "comment": "pushed by tamias deploy",
                    "scopes": ["workers"],
                }]);
                let (status, text) = self.request("POST", &self.secrets_url(), None, Some(body))?;
                Self::envelope(status, &text)?;
                Ok(PushReport {
                    name,
                    created: true,
                })
            }
        }
    }

    fn remove(&self, env: &Env, rref: &Ref) -> Result<bool, DeployError> {
        let name = Self::sink_name(env, rref);
        match self.find_id(&name)? {
            Some(id) => {
                let url = format!("{}/{}", self.secrets_url(), id);
                let (status, text) = self.request("DELETE", &url, None, None)?;
                Self::envelope(status, &text)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn target() -> CfSecretsStore {
        CfSecretsStore::with_token("prod", "acc", "sto", "fake", Some("http://127.0.0.1:1"))
            .unwrap()
    }

    #[test]
    fn new_reads_the_token_env_var() {
        temp_env::with_var_unset("TAMIAS_TEST_CF_DEPLOY_TOKEN", || {
            let e = CfSecretsStore::new("p", "a", "s", "TAMIAS_TEST_CF_DEPLOY_TOKEN", None)
                .unwrap_err();
            assert!(e.to_string().contains("unset"), "{e}");
        });
        temp_env::with_var("TAMIAS_TEST_CF_DEPLOY_TOKEN", Some(""), || {
            let e = CfSecretsStore::new("p", "a", "s", "TAMIAS_TEST_CF_DEPLOY_TOKEN", None)
                .unwrap_err();
            assert!(e.to_string().contains("empty"), "{e}");
        });
        temp_env::with_var("TAMIAS_TEST_CF_DEPLOY_TOKEN", Some("fake-tok"), || {
            let t =
                CfSecretsStore::new("p", "a", "s", "TAMIAS_TEST_CF_DEPLOY_TOKEN", None).unwrap();
            assert_eq!(t.name(), "p");
            assert_eq!(t.base, DEFAULT_BASE);
        });
    }

    #[test]
    fn base_url_must_be_https_or_loopback_http() {
        // a plaintext-HTTP non-loopback endpoint would receive the
        // token and every pushed value — config must refuse it
        for bad in [
            "http://api.example.com",
            "http://example.com:8080/x",
            "api.example.com",
            "https://",
            "",
        ] {
            let e = CfSecretsStore::with_token("p", "a", "s", "fake", Some(bad))
                .err()
                .unwrap_or_else(|| panic!("{bad} must be rejected"));
            assert!(e.to_string().contains("https://"), "{bad}: {e}");
        }
        // https anywhere, http on loopback — both accepted
        for ok in [
            "https://api.cloudflare.com/client/v4",
            "http://127.0.0.1:8787",
            "http://localhost",
            "http://[::1]:8787",
        ] {
            assert!(
                CfSecretsStore::with_token("p", "a", "s", "fake", Some(ok)).is_ok(),
                "{ok}"
            );
        }
        // None → the https default
        assert!(CfSecretsStore::with_token("p", "a", "s", "fake", None).is_ok());
        // and `new` enforces the same rule on the config path
        temp_env::with_var("TAMIAS_TEST_CF_DEPLOY_TOKEN", Some("fake"), || {
            let e = CfSecretsStore::new(
                "p",
                "a",
                "s",
                "TAMIAS_TEST_CF_DEPLOY_TOKEN",
                Some("http://api.example.com"),
            )
            .err()
            .unwrap();
            assert!(e.to_string().contains("https://"), "{e}");
        });
    }

    #[test]
    fn request_rejects_unknown_method_body_shapes() {
        // the internal seam: shapes no caller produces are clean
        // errors, never panics
        let t = target();
        assert!(t.request("PUT", "http://x", None, None).is_err());
        assert!(t
            .request("GET", "http://x", None, Some(serde_json::json!({})))
            .is_err());
        // a GET without a query works the no-query arm — it still
        // fails transport here since nothing listens
        assert!(t.request("GET", "http://127.0.0.1:1", None, None).is_err());
        // a GET *with* a query works the query arm — same transport
        // failure downstream
        assert!(t
            .request("GET", "http://127.0.0.1:1", Some(("name", "x")), None)
            .is_err());
    }

    #[test]
    fn a_body_short_of_content_length_is_an_io_error() {
        // server promises 100 bytes, sends 5, drops — the
        // read_to_string arm fires, not a transport error
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            use std::io::Write;
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut s, &mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort");
        });
        let t = CfSecretsStore::with_token(
            "prod",
            "acc",
            "sto",
            "fake",
            Some(&format!("http://127.0.0.1:{port}")),
        )
        .unwrap();
        let e = t.request("GET", &format!("http://127.0.0.1:{port}"), None, None);
        assert!(e.is_err());
    }

    #[test]
    fn request_serves_every_verb_shape() {
        use tamias_testkit::httpmock::{HttpMock, Request, Response};
        let mock = HttpMock::new(|_req: &Request| Response::json(200, "{}"));
        let t = CfSecretsStore::with_token("prod", "acc", "sto", "fake", Some(mock.url())).unwrap();
        for (method, body) in [
            ("GET", None),
            ("DELETE", None),
            ("POST", Some(serde_json::json!({}))),
            ("PATCH", Some(serde_json::json!({}))),
        ] {
            let (status, _) = t.request(method, mock.url(), None, body).unwrap();
            assert_eq!(status, 200, "{method}");
        }
    }

    #[test]
    fn envelope_reads_the_cf_contract() {
        // 401/403 → Unauthorized regardless of body
        for status in [401, 403] {
            let e = CfSecretsStore::envelope(status, "{}").unwrap_err();
            assert!(format!("{e:?}").starts_with("Unauthorized"), "{e}");
        }
        // non-json body → backend error quoting the status
        let e = CfSecretsStore::envelope(502, "proxy says no").unwrap_err();
        assert!(e.to_string().contains("non-json"), "{e}");
        // success:false with an errors array → joined messages
        let e = CfSecretsStore::envelope(
            400,
            r#"{"success":false,"errors":[{"message":"m1"},{"message":"m2"}],"result":null}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("m1; m2"), "{e}");
        // success:false with no errors → falls back to the raw body
        let e = CfSecretsStore::envelope(500, r#"{"success":false,"errors":[]}"#).unwrap_err();
        assert!(e.to_string().contains("success"), "{e}");
        // success:true parses through
        let v = CfSecretsStore::envelope(200, r#"{"success":true,"result":[1]}"#).unwrap();
        assert_eq!(v["result"][0], 1);
    }

    #[test]
    fn find_id_and_transport_errors_are_clean() {
        // nothing listens → find_id reports Io through push/remove
        let t = target();
        let env = Env::dev();
        let r = Ref::new("tamias://x/y").unwrap();
        let e = t.push(&env, &r, &Secret::new("fake")).unwrap_err();
        assert!(format!("{e:?}").starts_with("Io"), "{e}");
        let e = t.remove(&env, &r).unwrap_err();
        assert!(format!("{e:?}").starts_with("Io"), "{e}");
    }
}
