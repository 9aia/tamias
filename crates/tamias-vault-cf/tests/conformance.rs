//! `tamias-vault-cf` against the shared vault conformance suite —
//! every test goes through real HTTP to the in-memory wire mock
//! (the same semantics `worker/index.js` implements against D1).

#![allow(clippy::unwrap_used)]

use age::x25519;
use tamias_core::{Env, Ref, Secret};
use tamias_testkit::httpmock::HttpMock;
use tamias_testkit::mocks;
use tamias_vault::{SecretRecord, Vault, VaultError};
use tamias_vault_cf::CfVault;

/// The token every request must bear — a fake, obviously.
const TOKEN: &str = "test-token-not-real";

/// A `CfVault` plus the mock server that keeps it reachable — the
/// mock must live as long as the vault under test.
struct VaultOnMock {
    mock: HttpMock,
    inner: CfVault,
}

fn fresh_vault() -> VaultOnMock {
    let mock = mocks::vault_wire(TOKEN);
    let identity = x25519::Identity::generate();
    let inner = CfVault::with_identity(mock.url(), TOKEN, identity, &[]).unwrap();
    VaultOnMock { mock, inner }
}

impl Vault for VaultOnMock {
    fn get(&self, env: &Env, rref: &Ref) -> Result<Option<SecretRecord>, VaultError> {
        self.inner.get(env, rref)
    }
    fn put(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        self.inner.put(env, rref, value)
    }
    fn rm(&self, env: &Env, rref: &Ref) -> Result<bool, VaultError> {
        self.inner.rm(env, rref)
    }
    fn list(&self, env: &Env) -> Result<Vec<Ref>, VaultError> {
        self.inner.list(env)
    }
    fn rotate(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError> {
        self.inner.rotate(env, rref, value)
    }
}

tamias_testkit::vault_conformance!(fresh_vault);

#[test]
fn bad_token_is_an_error_not_a_panic() {
    let mock = mocks::vault_wire(TOKEN);
    let identity = x25519::Identity::generate();
    let v = CfVault::with_identity(mock.url(), "wrong-token", identity, &[]).unwrap();
    let err = v
        .get(&Env::dev(), &Ref::new("tamias://x/y").unwrap())
        .unwrap_err();
    assert!(err.to_string().contains("unauthorized"), "{err}");
}

#[test]
fn unreachable_host_is_io_error_not_panic() {
    let identity = x25519::Identity::generate();
    // A port nothing listens on — connection refused is a clean
    // VaultError, never a panic or a hang.
    let v = CfVault::with_identity("http://127.0.0.1:1", "t", identity, &[]).unwrap();
    match v.list(&Env::dev()) {
        Err(VaultError::Io(_)) => {}
        other => panic!("expected VaultError::Io, got {other:?}"),
    }
}

#[test]
fn ciphertext_only_on_the_wire() {
    let vault = fresh_vault();
    vault
        .inner
        .put(
            &Env::dev(),
            &Ref::new("tamias://billing/stripe").unwrap(),
            Secret::new(tamias_testkit::FAKE),
        )
        .unwrap();
    // Fetch the stored row as the server serves it — the body must
    // carry ciphertext only, never the plaintext.
    let raw = ureq::get(format!("{}/v1/dev/billing/stripe", vault.mock.url()))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    assert!(
        !raw.contains(tamias_testkit::FAKE),
        "wire carried plaintext"
    );
}

#[test]
fn malformed_requests_get_wire_errors_not_panics() {
    // The mock itself is protocol code — bad shapes exercise its
    // error arms rather than the vault client's.
    let mock = mocks::vault_wire(TOKEN);
    let auth = format!("Bearer {TOKEN}");
    let url = mock.url();
    let status = |r: Result<ureq::http::Response<ureq::Body>, ureq::Error>| match r {
        Ok(res) => res.status().as_u16(),
        Err(ureq::Error::StatusCode(code)) => code,
        Err(e) => panic!("transport error: {e}"),
    };
    // path outside /v1/ → the prefix 404
    let r = ureq::get(format!("{url}/nope"))
        .header("Authorization", &auth)
        .config()
        .http_status_as_error(false)
        .build()
        .call();
    assert_eq!(status(r), 404);
    // "/v1/" — empty env segment → 404
    let r = ureq::get(format!("{url}/v1/"))
        .header("Authorization", &auth)
        .config()
        .http_status_as_error(false)
        .build()
        .call();
    assert_eq!(status(r), 404);
    // PUT without ciphertext → 400
    let r = ureq::put(format!("{url}/v1/dev/x/y"))
        .header("Authorization", &auth)
        .content_type("application/json")
        .config()
        .http_status_as_error(false)
        .build()
        .send("{}");
    assert_eq!(status(r), 400);
    // a method the wire doesn't route → 405
    let r = ureq::patch(format!("{url}/v1/dev/x/y"))
        .header("Authorization", &auth)
        .content_type("application/json")
        .config()
        .http_status_as_error(false)
        .build()
        .send("{}");
    assert_eq!(status(r), 405);
}
