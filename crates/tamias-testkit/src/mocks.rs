//! Protocol fakes on top of [`HttpMock`] — in-memory reference
//! implementations of the two remote surfaces tamias talks to:
//!
//! - [`vault_wire`] / [`vault_wire_v2`] — the `tamias-vault-cf` wire
//!   protocol, the same semantics `tamias-vault-cf/worker/index.js`
//!   implements against D1. Stores base64 ciphertext rows only —
//!   never plaintext.
//! - [`cf_secrets_store`] — the CF API v4 Secrets Store envelope
//!   (`{success, errors, result}`) that `tamias-deploy-cf` pushes to.
//!   A real Secrets Store *is* a plaintext sink, so this fake
//!   records what it receives (fake values only, by rule).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::httpmock::{HttpMock, Request, Response};

/// One stored vault row (ciphertext and `name` are opaque to the
/// store — the private-refs blob travels verbatim).
#[derive(Clone)]
struct VaultRow {
    ciphertext: String,
    name: Option<String>,
    version: u64,
    created_at: String,
    rotated_at: Option<String>,
}

fn now() -> String {
    // rfc3339 formatting of a valid datetime cannot fail
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 formatting is infallible")
}

fn row_json(s: &VaultRow) -> String {
    json!({
        "version": s.version,
        "created_at": s.created_at,
        "rotated_at": s.rotated_at,
    })
    .to_string()
}

/// One scoped credential the v2 mock recognizes — the test-side of
/// the Worker's `TAMIAS_VAULT_TOKENS` JSON map (P-003).
#[derive(Clone)]
pub struct ScopedToken {
    /// The wire `token_id` (`TamiasHmac <id>, …`).
    pub id: &'static str,
    /// The shared secret — a fake value, by rule.
    pub secret: &'static str,
    /// `(env|"*", read, write)` grants — env entries win over `"*"`.
    pub scopes: &'static [(&'static str, bool, bool)],
}

fn scope_map(scopes: &[(&str, bool, bool)]) -> BTreeMap<String, (bool, bool)> {
    scopes
        .iter()
        .map(|(e, r, w)| (e.to_string(), (*r, *w)))
        .collect()
}

fn scope_allows(scopes: &BTreeMap<String, (bool, bool)>, env: &str, write: bool) -> bool {
    let (read, w) = scopes
        .get(env)
        .or_else(|| scopes.get("*"))
        .copied()
        .unwrap_or((false, false));
    if write {
        w
    } else {
        read
    }
}

/// XOR-fold constant-time equality — the mock mirrors the worker's
/// compare (P-006): no early exit, length folded in.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// `HMAC-SHA256(key, msg)`; the expect documents the invariant that
/// HMAC accepts keys of any length.
#[allow(clippy::unwrap_used)]
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The worker's `authenticate` — Bearer against every configured
/// token (and the legacy one), `TamiasHmac id, ts, nonce, sig`
/// against the scoped table with the ±60s replay window. The
/// pre-nonce `id, ts, sig` form stays accepted (migration), signed
/// without the nonce line. A verified sig may be used once —
/// `seen` is the mock's `used_sig` table. Returns the caller's
/// scope map or `None`.
fn authenticate(
    req: &Request,
    path: &str,
    tokens: &[ScopedToken],
    legacy: Option<&str>,
    seen: &Mutex<std::collections::HashSet<String>>,
) -> Option<BTreeMap<String, (bool, bool)>> {
    let header = req.header("authorization")?;
    if let Some(candidate) = header.strip_prefix("Bearer ") {
        let mut scopes = None;
        for t in tokens {
            if ct_eq(candidate.as_bytes(), t.secret.as_bytes()) {
                scopes = Some(scope_map(t.scopes));
            }
        }
        if let Some(l) = legacy {
            if ct_eq(candidate.as_bytes(), l.as_bytes()) {
                scopes = Some(BTreeMap::from([("*".to_string(), (true, true))]));
            }
        }
        return scopes;
    }
    if let Some(rest) = header.strip_prefix("TamiasHmac ") {
        let parts: Vec<&str> = rest.split(',').map(|s| s.trim()).collect();
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let (id, ts) = (parts[0], parts[1]);
        let nonce = if parts.len() == 4 { parts[2] } else { "" };
        let sig = if parts.len() == 4 { parts[3] } else { parts[2] };
        if parts.len() == 4 && nonce.is_empty() {
            return None;
        }
        let t: u64 = ts.parse().ok()?;
        if unix_now().abs_diff(t) > 60 {
            return None;
        }
        let entry = tokens.iter().find(|t| t.id == id)?;
        let body_sha = hex_encode(&Sha256::digest(&req.body));
        let msg = if parts.len() == 4 {
            format!("{}\n{}\n{}\n{}\n{}", req.method, path, ts, nonce, body_sha)
        } else {
            format!("{}\n{}\n{}\n{}", req.method, path, ts, body_sha)
        };
        let expect = hex_encode(&hmac_sha256(entry.secret.as_bytes(), msg.as_bytes()));
        if !ct_eq(sig.as_bytes(), expect.as_bytes()) {
            return None;
        }
        // Replay guard (worker parity): a verified sig is spent —
        // a second sighting fails auth.
        if !seen.lock().unwrap().insert(sig.to_string()) {
            return None;
        }
        return Some(scope_map(entry.scopes));
    }
    None
}

/// The `tamias-vault-cf/1` wire: single bearer token, all envs — kept
/// for the legacy-auth path; the store side is already v2-shaped
/// (`name` column, keyed list items).
pub fn vault_wire(token: &'static str) -> HttpMock {
    vault_wire_v2(&[], Some(token))
}

/// The `tamias-vault-cf/2` wire protocol, in memory: scoped tokens
/// (`Bearer` against any entry's secret, `TamiasHmac` signed requests
/// per token_id), per-env `{read, write}` scope enforcement,
/// per-env keyspace, `name`-blob pass-through, and server-side
/// version/created_at/rotated_at bookkeeping (the client's rotate
/// is a `PUT` with `"rotate": true`, 404 on a missing key).
pub fn vault_wire_v2(tokens: &'static [ScopedToken], legacy: Option<&'static str>) -> HttpMock {
    let store: Arc<Mutex<BTreeMap<(String, String), VaultRow>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    // The worker's `used_sig` table — spent signatures.
    let seen: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    HttpMock::new(move |req: &Request| {
        let path = req.path.split('?').next().unwrap_or("");
        let scopes = match authenticate(req, path, tokens, legacy, &seen) {
            Some(s) => s,
            None => return Response::json(401, r#"{"error":"unauthorized"}"#),
        };
        let rest = match path.strip_prefix("/v1/") {
            Some(r) => r,
            None => return Response::json(404, r#"{"error":"no such route"}"#),
        };
        // /v1/{env} lists; /v1/{env}/{key…} addresses one record.
        let (env, rref) = match rest.split_once('/') {
            Some((env, rref)) => (env.to_string(), Some(rref.to_string())),
            None => (rest.to_string(), None),
        };
        if env.is_empty() {
            return Response::json(404, r#"{"error":"no such route"}"#);
        }
        let write = matches!(req.method.as_str(), "PUT" | "DELETE");
        if !scope_allows(&scopes, &env, write) {
            return Response::json(403, r#"{"error":"token scope denies this env/verb"}"#);
        }
        let mut store = store.lock().unwrap();
        match (req.method.as_str(), rref) {
            ("GET", None) => {
                let refs: Vec<serde_json::Value> = store
                    .iter()
                    .filter(|((e, _), _)| e == &env)
                    .map(|((_, r), s)| json!({ "key": r, "name": s.name }))
                    .collect();
                Response::json(200, json!({ "refs": refs }).to_string())
            }
            ("GET", Some(rref)) => match store.get(&(env, rref)) {
                Some(s) => Response::json(
                    200,
                    json!({
                        "ciphertext": s.ciphertext,
                        "name": s.name,
                        "version": s.version,
                        "created_at": s.created_at,
                        "rotated_at": s.rotated_at,
                    })
                    .to_string(),
                ),
                None => Response::json(404, r#"{"error":"not found"}"#),
            },
            ("PUT", Some(rref)) => {
                let body: serde_json::Value =
                    serde_json::from_str(&req.text()).unwrap_or(json!({}));
                let ciphertext = match body["ciphertext"].as_str() {
                    Some(c) if !c.is_empty() => c.to_string(),
                    _ => return Response::json(400, r#"{"error":"missing ciphertext"}"#),
                };
                let name = body["name"].as_str().map(str::to_string);
                let rotate = body["rotate"].as_bool().unwrap_or(false);
                let key = (env, rref);
                match (store.get(&key), rotate) {
                    (None, true) => Response::json(404, r#"{"error":"not found"}"#),
                    (None, false) => {
                        let s = VaultRow {
                            ciphertext,
                            name,
                            version: 1,
                            created_at: now(),
                            rotated_at: None,
                        };
                        let out = row_json(&s);
                        store.insert(key, s);
                        Response::json(200, out)
                    }
                    (Some(old), rot) => {
                        // L8 (worker parity): a name blob may
                        // neither be dropped nor gained in place
                        if (old.name.is_some()) != name.is_some() {
                            return Response::json(
                                409,
                                r#"{"error":"name blob mismatch — mixed-mode PUT"}"#,
                            );
                        }
                        let s = VaultRow {
                            ciphertext,
                            name,
                            version: old.version + 1,
                            created_at: old.created_at.clone(),
                            rotated_at: if rot {
                                Some(now())
                            } else {
                                old.rotated_at.clone()
                            },
                        };
                        let out = row_json(&s);
                        store.insert(key, s);
                        Response::json(200, out)
                    }
                }
            }
            ("DELETE", Some(rref)) => {
                let removed = store.remove(&(env, rref)).is_some();
                Response::json(200, json!({ "removed": removed }).to_string())
            }
            _ => Response::json(405, r#"{"error":"method not allowed"}"#),
        }
    })
}

/// A fake CF API v4 Secrets Store. Routes under
/// `/accounts/{account}/secrets_store/stores/{store}/secrets`:
/// `GET ?search=` lists (name-substring), `POST` creates from an
/// array body, `PATCH /{id}` updates, `DELETE /{id}` removes —
/// all inside the `{success, errors, result}` envelope.
///
/// Returns the running mock plus the request log for assertions.
pub fn cf_secrets_store(
    account: &'static str,
    store_id: &'static str,
    token: &'static str,
) -> (HttpMock, Arc<Mutex<Vec<Request>>>) {
    let seen: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
    let store: Arc<Mutex<BTreeMap<String, serde_json::Value>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    let next_id = Arc::new(Mutex::new(0u64));
    let mock = {
        let seen = seen.clone();
        HttpMock::new(move |req: &Request| {
            seen.lock().unwrap().push(req.clone());
            if req.header("authorization") != Some(format!("Bearer {token}").as_str()) {
                return Response::json(
                    403,
                    r#"{"success":false,"errors":[{"code":1000,"message":"bad token"}],"result":null}"#,
                );
            }
            let ok = |result: serde_json::Value| {
                Response::json(
                    200,
                    json!({"success":true,"errors":[],"result":result}).to_string(),
                )
            };
            let base = format!("/accounts/{account}/secrets_store/stores/{store_id}/secrets");
            let (path, query) = match req.path.split_once('?') {
                Some((p, q)) => (p, q),
                None => (req.path.as_str(), ""),
            };
            let suffix = path.strip_prefix(&base);
            let mut store = store.lock().unwrap();
            match (req.method.as_str(), suffix) {
                ("GET", Some("")) => {
                    let want = query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("search="))
                        .unwrap_or("");
                    let result: Vec<serde_json::Value> = store
                        .values()
                        .filter(|s| {
                            want.is_empty() || s["name"].as_str().unwrap_or("").contains(want)
                        })
                        .map(|s| json!({"id": s["id"], "name": s["name"], "status": "active"}))
                        .collect();
                    ok(json!(result))
                }
                ("POST", Some("")) => {
                    let body: serde_json::Value =
                        serde_json::from_str(&req.text()).unwrap_or(json!([]));
                    let mut results = Vec::new();
                    for item in body.as_array().cloned().unwrap_or_default() {
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let mut id = next_id.lock().unwrap();
                        *id += 1;
                        let id = format!("id{:04}", *id);
                        store.insert(
                            id.clone(),
                            json!({"id": id, "name": name, "value": item["value"]}),
                        );
                        results.push(json!({"id": id, "name": name}));
                    }
                    ok(json!(results))
                }
                ("PATCH", Some(suf)) | ("DELETE", Some(suf)) => {
                    let id = suf.trim_start_matches('/');
                    match store.get(&id.to_string()) {
                        None => Response::json(
                            404,
                            r#"{"success":false,"errors":[{"code":1007,"message":"not found"}],"result":null}"#,
                        ),
                        Some(_) if req.method == "DELETE" => {
                            store.remove(&id.to_string());
                            ok(json!({"id": id}))
                        }
                        Some(old) => {
                            let body: serde_json::Value =
                                serde_json::from_str(&req.text()).unwrap_or_default();
                            let mut new = old.clone();
                            new["value"] = body["value"].clone();
                            store.insert(id.to_string(), new.clone());
                            ok(json!({"id": id, "name": new["name"]}))
                        }
                    }
                }
                _ => Response::json(404, r#"{"success":false,"errors":[],"result":null}"#),
            }
        })
    };
    (mock, seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Fire one raw HTTP request at `mock` and read the raw reply.
    fn exchange(mock: &HttpMock, request: &str) -> String {
        let addr = mock.url().trim_start_matches("http://");
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    const AUTH: &str = "Authorization: Bearer tok\r\n";

    /// A correct `TamiasHmac` header for `req` under `id`/`secret` —
    /// mirrors the client signer (4-field form with a fresh nonce),
    /// for exercising the mock's own verification path.
    fn hmac_auth(id: &str, secret: &str, method: &str, path: &str, body: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let ts = unix_now();
        let nonce = format!("test-nonce-{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let msg = format!(
            "{}\n{}\n{}\n{}\n{}",
            method,
            path,
            ts,
            nonce,
            hex_encode(&Sha256::digest(body.as_bytes()))
        );
        let sig = hex_encode(&hmac_sha256(secret.as_bytes(), msg.as_bytes()));
        format!("Authorization: TamiasHmac {id}, {ts}, {nonce}, {sig}\r\n")
    }

    #[test]
    fn vault_wire_rejects_bad_routes_and_shapes() {
        let mock = vault_wire("tok");
        // no /v1/ prefix → 404
        let out = exchange(&mock, &format!("GET /bogus HTTP/1.1\r\n{AUTH}\r\n"));
        assert!(out.contains("404"), "{out}");
        // /v1/ with an empty env → 404
        let out = exchange(&mock, &format!("GET /v1/ HTTP/1.1\r\n{AUTH}\r\n"));
        assert!(out.contains("404"), "{out}");
        // PUT without a ciphertext field → 400
        let out = exchange(
            &mock,
            &format!("PUT /v1/dev/x/y HTTP/1.1\r\n{AUTH}Content-Length: 2\r\n\r\n{{}}"),
        );
        assert!(out.contains("400"), "{out}");
        // a method the protocol doesn't know → 405
        let out = exchange(&mock, &format!("PATCH /v1/dev/x HTTP/1.1\r\n{AUTH}\r\n"));
        assert!(out.contains("405"), "{out}");
        // sanity: a real list still works
        let out = exchange(&mock, &format!("GET /v1/dev HTTP/1.1\r\n{AUTH}\r\n"));
        assert!(out.contains("200") && out.contains("\"refs\""), "{out}");
    }

    #[test]
    fn authenticate_scores_a_valid_sig_and_denies_a_bad_one() {
        let tokens = &[ScopedToken {
            id: "ops/tok-dev",
            secret: "fake-secret",
            scopes: &[("dev", true, true)],
        }];
        let req = |method: &str, path: &str, auth: String, body: &str| Request {
            method: method.to_string(),
            path: path.to_string(),
            headers: vec![(
                "authorization".into(),
                auth.trim_start_matches("Authorization: ")
                    .trim_end()
                    .to_string(),
            )],
            body: body.as_bytes().to_vec(),
        };
        let seen = Mutex::new(std::collections::HashSet::new());
        // a correct signature → Some(scopes)
        let auth = hmac_auth("ops/tok-dev", "fake-secret", "GET", "/v1/dev", "");
        let scopes = authenticate(
            &req("GET", "/v1/dev", auth.clone(), ""),
            "/v1/dev",
            tokens,
            None,
            &seen,
        );
        assert!(scopes.is_some());
        // the identical signed request again → a spent sig → None
        // (the worker's used_sig replay guard)
        assert!(authenticate(
            &req("GET", "/v1/dev", auth, ""),
            "/v1/dev",
            tokens,
            None,
            &seen
        )
        .is_none());
        // a wrong signature → None
        let auth = hmac_auth("ops/tok-dev", "wrong", "GET", "/v1/dev", "");
        assert!(authenticate(
            &req("GET", "/v1/dev", auth, ""),
            "/v1/dev",
            tokens,
            None,
            &seen
        )
        .is_none());
        // a 4-field header with an empty nonce → the degenerate
        // form is rejected outright
        assert!(authenticate(
            &req(
                "GET",
                "/v1/dev",
                "Authorization: TamiasHmac ops/tok-dev, 1, , deadbeef\n".into(),
                ""
            ),
            "/v1/dev",
            tokens,
            None,
            &seen
        )
        .is_none());
        // header present but neither scheme → None
        assert!(authenticate(
            &req(
                "GET",
                "/v1/dev",
                "Authorization: Basic e30=
"
                .into(),
                ""
            ),
            "/v1/dev",
            tokens,
            None,
            &seen
        )
        .is_none());
    }

    #[test]
    fn vault_wire_v2_scopes_and_signs() {
        let tokens = &[ScopedToken {
            id: "ops/tok-dev",
            secret: "fake-secret",
            scopes: &[("dev", true, true), ("prod", true, false)],
        }];
        let mock = vault_wire_v2(tokens, None);
        // bearer against a scoped secret → its scopes apply
        let out = exchange(
            &mock,
            "GET /v1/dev HTTP/1.1\r\nAuthorization: Bearer fake-secret\r\n\r\n",
        );
        assert!(out.contains("200"), "{out}");
        // prod is read-only for this token → PUT is 403
        let out = exchange(
            &mock,
            "PUT /v1/prod/x/y HTTP/1.1\r\nAuthorization: Bearer fake-secret\r\nContent-Length: 19\r\n\r\n{\"ciphertext\":\"aa\"}",
        );
        assert!(out.contains("403"), "{out}");
        // a valid TamiasHmac signature reads prod
        let auth = hmac_auth("ops/tok-dev", "fake-secret", "GET", "/v1/prod", "");
        let out = exchange(&mock, &format!("GET /v1/prod HTTP/1.1\r\n{auth}\r\n"));
        assert!(out.contains("200"), "{out}");
        // a signature over the wrong path → 401
        let auth = hmac_auth("ops/tok-dev", "fake-secret", "GET", "/v1/dev", "");
        let out = exchange(&mock, &format!("GET /v1/prod HTTP/1.1\r\n{auth}\r\n"));
        assert!(out.contains("401"), "{out}");
        // unknown token id → 401
        let auth = hmac_auth("ghost", "fake-secret", "GET", "/v1/dev", "");
        let out = exchange(&mock, &format!("GET /v1/dev HTTP/1.1\r\n{auth}\r\n"));
        assert!(out.contains("401"), "{out}");
        // a malformed TamiasHmac header (wrong field count) → 401
        let out = exchange(
            &mock,
            "GET /v1/dev HTTP/1.1\r\nAuthorization: TamiasHmac only, two\r\n\r\n",
        );
        assert!(out.contains("401"), "{out}");
        // a stale timestamp — outside the ±60s replay window → 401
        let stale = unix_now() - 3600;
        let msg = format!(
            "GET\n/v1/dev\n{}\n{}",
            stale,
            hex_encode(&Sha256::digest(b""))
        );
        let sig = hex_encode(&hmac_sha256(b"fake-secret", msg.as_bytes()));
        let out = exchange(
            &mock,
            &format!(
                "GET /v1/dev HTTP/1.1\r\nAuthorization: TamiasHmac ops/tok-dev, {stale}, {sig}\r\n\r\n"
            ),
        );
        assert!(out.contains("401"), "{out}");
        // a garbage (non-numeric) ts → 401 too
        let out = exchange(
            &mock,
            "GET /v1/dev HTTP/1.1\r\nAuthorization: TamiasHmac ops/tok-dev, not-a-ts, deadbeef\r\n\r\n",
        );
        assert!(out.contains("401"), "{out}");
        // the pre-nonce 3-field form still verifies — the migration
        // path (signed without the nonce line)
        let ts = unix_now();
        let msg = format!("GET\n/v1/dev\n{}\n{}", ts, hex_encode(&Sha256::digest(b"")));
        let sig = hex_encode(&hmac_sha256(b"fake-secret", msg.as_bytes()));
        let out = exchange(
            &mock,
            &format!(
                "GET /v1/dev HTTP/1.1\r\nAuthorization: TamiasHmac ops/tok-dev, {ts}, {sig}\r\n\r\n"
            ),
        );
        assert!(out.contains("200"), "{out}");
    }

    #[test]
    fn cf_store_handles_missing_ids_and_odd_methods() {
        let (mock, _) = cf_secrets_store("acc", "sto", "tok");
        let base = "/accounts/acc/secrets_store/stores/sto/secrets";
        // DELETE of an id that was never created → 404 envelope
        let out = exchange(
            &mock,
            &format!("DELETE {base}/id9999 HTTP/1.1\r\n{AUTH}\r\n"),
        );
        assert!(out.contains("404"), "{out}");
        // a method+suffix no arm claims → the 404 fallthrough
        let out = exchange(&mock, &format!("GET {base}/id0001 HTTP/1.1\r\n{AUTH}\r\n"));
        assert!(out.contains("404"), "{out}");
        let out = exchange(&mock, &format!("PUT {base} HTTP/1.1\r\n{AUTH}\r\n"));
        assert!(out.contains("404"), "{out}");
    }
}
