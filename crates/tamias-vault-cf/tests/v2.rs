//! `tamias-vault-cf/2` features over real HTTP — scoped tokens +
//! TamiasHmac signing (P-003) and private ref metadata (P-004).
//! The in-memory mock enforces the same auth the worker does, so a
//! passing request *was* correctly signed and scoped. All secrets,
//! tokens, and refs are fake by rule.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use age::x25519;
use tamias_core::{Env, Ref, Secret};
use tamias_testkit::httpmock::{HttpMock, Request, Response};
use tamias_testkit::mocks::{self, ScopedToken};
use tamias_vault::{Vault, VaultError};
use tamias_vault_cf::{Auth, CfVault};

const TOKEN: &str = "test-token-not-real";

fn rref(s: &str) -> Ref {
    Ref::new(s).unwrap()
}

fn bearer_auth_map() -> BTreeMap<String, Auth> {
    BTreeMap::from([("*".to_string(), Auth::Bearer(TOKEN.to_string()))])
}

/// A private-refs vault over `mock` — bearer auth, `private_refs`
/// key derivation on.
fn private_vault(mock: &HttpMock, identity: x25519::Identity) -> CfVault {
    CfVault::with_auth(mock.url(), bearer_auth_map(), identity, &[], true).unwrap()
}

#[test]
fn scoped_tokens_sign_and_scope_per_env() {
    let tokens = &[
        ScopedToken {
            id: "ops/cf-token-dev",
            secret: "fake-dev-token",
            scopes: &[("dev", true, true)],
        },
        ScopedToken {
            id: "ops/cf-token-prod",
            secret: "fake-prod-token",
            scopes: &[("prod", true, false)],
        },
    ];
    let mock = mocks::vault_wire_v2(tokens, None);
    let v = CfVault::with_auth(
        mock.url(),
        BTreeMap::from([
            (
                "dev".to_string(),
                Auth::Hmac {
                    id: "ops/cf-token-dev".into(),
                    secret: "fake-dev-token".into(),
                },
            ),
            (
                "prod".to_string(),
                Auth::Hmac {
                    id: "ops/cf-token-prod".into(),
                    secret: "fake-prod-token".into(),
                },
            ),
        ]),
        x25519::Identity::generate(),
        &[],
        false,
    )
    .unwrap();

    // dev: read+write — the mock verified the TamiasHmac signature.
    v.put(&Env::dev(), &rref("tamias://a/b"), Secret::new("fake"))
        .unwrap();
    let rec = v.get(&Env::dev(), &rref("tamias://a/b")).unwrap().unwrap();
    assert_eq!(rec.value.expose(), "fake");
    assert_eq!(v.list(&Env::dev()).unwrap(), vec![rref("tamias://a/b")]);
    v.rotate(&Env::dev(), &rref("tamias://a/b"), Secret::new("fake-2"))
        .unwrap();
    assert!(v.rm(&Env::dev(), &rref("tamias://a/b")).unwrap());

    // prod: read-only — writes are 403 (scope), reads are fine.
    let e = v
        .put(
            &Env::new("prod").unwrap(),
            &rref("tamias://x/y"),
            Secret::new("f"),
        )
        .unwrap_err();
    assert!(e.to_string().contains("unauthorized"), "{e}");
    let e = v
        .rm(&Env::new("prod").unwrap(), &rref("tamias://x/y"))
        .unwrap_err();
    assert!(e.to_string().contains("unauthorized"), "{e}");
    let got = v
        .get(&Env::new("prod").unwrap(), &rref("tamias://x/y"))
        .unwrap();
    assert!(got.is_none());
    assert!(v.list(&Env::new("prod").unwrap()).unwrap().is_empty());

    // an env no credential covers → clean config error, no panic.
    let e = v.list(&Env::new("staging").unwrap()).unwrap_err();
    assert!(e.to_string().contains("no cf token"), "{e}");
}

#[test]
fn bearer_is_still_accepted_during_migration() {
    // The v2 mock honors Bearer against a scoped token's secret —
    // the migration path the worker keeps open (P-003).
    let tokens = &[ScopedToken {
        id: "ops/tok",
        secret: "fake-scoped-secret",
        scopes: &[("dev", true, true)],
    }];
    let mock = mocks::vault_wire_v2(tokens, None);
    let v = CfVault::with_auth(
        mock.url(),
        BTreeMap::from([("*".to_string(), Auth::Bearer("fake-scoped-secret".into()))]),
        x25519::Identity::generate(),
        &[],
        false,
    )
    .unwrap();
    v.put(&Env::dev(), &rref("tamias://a/b"), Secret::new("fake"))
        .unwrap();
    assert_eq!(v.list(&Env::dev()).unwrap().len(), 1);
}

#[test]
fn private_refs_hide_names_and_roundtrip() {
    let mock = mocks::vault_wire(TOKEN);
    let identity = x25519::Identity::generate();
    let v = private_vault(&mock, identity);

    v.put(
        &Env::dev(),
        &rref("tamias://billing/stripe"),
        Secret::new(tamias_testkit::FAKE),
    )
    .unwrap();
    v.put(&Env::dev(), &rref("tamias://ci/key"), Secret::new("fake-2"))
        .unwrap();

    // list maps keys back through the name blobs.
    let refs = v.list(&Env::dev()).unwrap();
    assert_eq!(
        refs,
        vec![rref("tamias://billing/stripe"), rref("tamias://ci/key")]
    );
    // get/rm/rotate work against the derived keys.
    let rec = v
        .get(&Env::dev(), &rref("tamias://billing/stripe"))
        .unwrap()
        .unwrap();
    assert_eq!(rec.value.expose(), tamias_testkit::FAKE);
    v.rotate(&Env::dev(), &rref("tamias://ci/key"), Secret::new("fake-3"))
        .unwrap();
    assert!(v.rm(&Env::dev(), &rref("tamias://ci/key")).unwrap());

    // The wire view is opaque: the ref name appears nowhere — the
    // storage key is derived hex, the name blob is ciphertext, and
    // the value is ciphertext as always.
    let raw = ureq::get(format!("{}/v1/dev", mock.url()))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    assert!(raw.contains("\"key\""), "{raw}");
    assert!(!raw.contains("billing/stripe"), "ref leaked: {raw}");
    assert!(!raw.contains(tamias_testkit::FAKE), "value leaked: {raw}");
    // same opacity on the single-row GET — fetch the derived key
    // from the raw listing.
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let key = body["refs"][0]["key"].as_str().unwrap();
    assert_eq!(key.len(), 64, "{key}");
    assert!(key.chars().all(|c| c.is_ascii_hexdigit()), "{key}");
    let row = ureq::get(format!("{}/v1/dev/{key}", mock.url()))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    assert!(!row.contains("billing/stripe"), "ref leaked: {row}");
    assert!(!row.contains(tamias_testkit::FAKE), "value leaked: {row}");
}

#[test]
fn mixed_mode_vaults_fail_closed_both_ways() {
    let mock = mocks::vault_wire(TOKEN);
    let identity = x25519::Identity::generate();

    // plain rows + a private-refs client → list refuses (the rows
    // can't be mapped back — migrate first).
    let plain = CfVault::with_identity(mock.url(), TOKEN, identity.clone(), &[]).unwrap();
    plain
        .put(&Env::dev(), &rref("tamias://a/b"), Secret::new("fake"))
        .unwrap();
    let pv = private_vault(&mock, identity.clone());
    let e = pv.list(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("mixed-mode"), "{e}");
    assert!(e.to_string().contains("migrate --private-refs"), "{e}");

    // and the mirror: private rows + a plain client → the keys are
    // opaque hex, so reading them as refs would be silently wrong.
    let mock2 = mocks::vault_wire(TOKEN);
    let pv2 = private_vault(&mock2, identity.clone());
    pv2.put(&Env::dev(), &rref("tamias://a/b"), Secret::new("fake"))
        .unwrap();
    let plain2 = CfVault::with_identity(mock2.url(), TOKEN, identity, &[]).unwrap();
    let e = plain2.list(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("private_refs"), "{e}");
}

#[test]
fn migrate_private_refs_rekeys_rows_and_preserves_values() {
    let mock = mocks::vault_wire(TOKEN);
    let identity = x25519::Identity::generate();

    // Seed a v1-style vault: plaintext ref keys, no name blobs.
    let plain = CfVault::with_identity(mock.url(), TOKEN, identity.clone(), &[]).unwrap();
    plain
        .put(&Env::dev(), &rref("tamias://a/b"), Secret::new("fake-a"))
        .unwrap();
    plain
        .put(&Env::dev(), &rref("tamias://c/d"), Secret::new("fake-c"))
        .unwrap();
    // a second env to prove envs stay separate keyspaces
    plain
        .put(
            &Env::new("prod").unwrap(),
            &rref("tamias://a/b"),
            Secret::new("fake-p"),
        )
        .unwrap();

    let pv = private_vault(&mock, identity);
    // migrating a non-private vault must refuse
    let e = plain.migrate_private_refs(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("private_refs"), "{e}");

    let rep = pv.migrate_private_refs(&Env::dev()).unwrap();
    assert_eq!(rep.rewritten.len(), 2, "{:?}", rep);
    assert_eq!(rep.already_private, 0);

    // Second run: everything is already private → no rewrites.
    let rep2 = pv.migrate_private_refs(&Env::dev()).unwrap();
    assert!(rep2.rewritten.is_empty());
    assert_eq!(rep2.already_private, 2);

    // Refs and values survive — ciphertext moved verbatim.
    let refs = pv.list(&Env::dev()).unwrap();
    assert_eq!(refs, vec![rref("tamias://a/b"), rref("tamias://c/d")]);
    assert_eq!(
        pv.get(&Env::dev(), &rref("tamias://a/b"))
            .unwrap()
            .unwrap()
            .value
            .expose(),
        "fake-a"
    );
    assert_eq!(
        pv.get(&Env::dev(), &rref("tamias://c/d"))
            .unwrap()
            .unwrap()
            .value
            .expose(),
        "fake-c"
    );
    // prod was untouched — its plain row still reads under a plain client.
    let plain2 =
        CfVault::with_identity(mock.url(), TOKEN, x25519::Identity::generate(), &[]).unwrap();
    let refs = plain2.list(&Env::new("prod").unwrap()).unwrap();
    assert_eq!(refs, vec![rref("tamias://a/b")]);

    // The old plaintext keys are gone from the dev keyspace.
    for stale in ["a/b", "c/d"] {
        let st = ureq::get(format!("{}/v1/dev/{stale}", mock.url()))
            .header("Authorization", &format!("Bearer {TOKEN}"))
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(st, 404, "stale key {stale} still readable");
    }
}

#[test]
fn private_refs_get_404_and_rotate_missing() {
    let mock = mocks::vault_wire(TOKEN);
    let v = private_vault(&mock, x25519::Identity::generate());
    assert!(v
        .get(&Env::dev(), &rref("tamias://no/thing"))
        .unwrap()
        .is_none());
    assert!(!v.rm(&Env::dev(), &rref("tamias://no/thing")).unwrap());
    let e = v
        .rotate(&Env::dev(), &rref("tamias://no/thing"), Secret::new("f"))
        .unwrap_err();
    assert!(matches!(e, VaultError::NotFound { .. }), "{e}");
}

/// A canned-response mock — whatever the handler says goes.
fn canned(f: impl Fn(&Request) -> Response + Send + Sync + 'static) -> HttpMock {
    HttpMock::new(f)
}

#[test]
fn private_get_fails_closed_on_missing_or_lying_names() {
    // a private-mode GET on a row with no name → mixed-mode error
    let mock = canned(|_| {
        Response::json(
            200,
            r#"{"ciphertext":"Z2FyYmFnZQ==","name":null,"version":1,"created_at":"2024-01-01T00:00:00Z","rotated_at":null}"#,
        )
    });
    let v = private_vault(&mock, x25519::Identity::generate());
    let e = v.get(&Env::dev(), &rref("tamias://a/b")).unwrap_err();
    assert!(e.to_string().contains("mixed-mode"), "{e}");

    // a row whose name blob decrypts to a *different* ref — the
    // store is lying; the value must never come out. Build the
    // blob on a donor mock, then serve it for another key.
    let donor_mock = mocks::vault_wire(TOKEN);
    let identity = x25519::Identity::generate();
    let donor = private_vault(&donor_mock, identity.clone());
    donor
        .put(&Env::dev(), &rref("tamias://a/b"), Secret::new("f"))
        .unwrap();
    let raw = ureq::get(format!("{}/v1/dev", donor_mock.url()))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    let name = serde_json::from_str::<serde_json::Value>(&raw).unwrap()["refs"][0]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let mock2 = canned(move |_| {
        Response::json(
            200,
            format!(
                r#"{{"ciphertext":"Z2FyYmFnZQ==","name":"{name}","version":1,"created_at":"2024-01-01T00:00:00Z","rotated_at":null}}"#
            ),
        )
    });
    let v2 = private_vault(&mock2, identity);
    let e = v2.get(&Env::dev(), &rref("tamias://c/d")).unwrap_err();
    assert!(e.to_string().contains("lying"), "{e}");
}

#[test]
fn migrate_surfaces_every_verb_failure_and_bad_keys() {
    // a stored key that isn't a ref can't migrate
    let mock = canned(|_| Response::json(200, r#"{"refs":[{"key":"bad key!","name":null}]}"#));
    let v = private_vault(&mock, x25519::Identity::generate());
    let e = v.migrate_private_refs(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("can't migrate"), "{e}");

    // list fails → propagate
    let mock = canned(|_| Response::json(500, r#"{"error":"list boom"}"#));
    let v = private_vault(&mock, x25519::Identity::generate());
    assert!(v.migrate_private_refs(&Env::dev()).is_err());

    // GET non-200 → error surfaced
    let mock = canned(|req| {
        if req.path == "/v1/dev" {
            Response::json(200, r#"{"refs":[{"key":"a/b","name":null}]}"#)
        } else {
            Response::json(500, r#"{"error":"get boom"}"#)
        }
    });
    let v = private_vault(&mock, x25519::Identity::generate());
    let e = v.migrate_private_refs(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("get boom"), "{e}");

    // GET 200 without ciphertext → backend error
    let mock = canned(|req| {
        if req.path == "/v1/dev" {
            Response::json(200, r#"{"refs":[{"key":"a/b","name":null}]}"#)
        } else {
            Response::json(
                200,
                r#"{"version":1,"created_at":"2024-01-01T00:00:00Z","rotated_at":null}"#,
            )
        }
    });
    let v = private_vault(&mock, x25519::Identity::generate());
    let e = v.migrate_private_refs(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("ciphertext"), "{e}");

    let row = r#"{"ciphertext":"Z2FyYmFnZQ==","version":1,"created_at":"2024-01-01T00:00:00Z","rotated_at":null}"#;
    // PUT fails → error surfaced, old row untouched
    let mock = canned(move |req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/dev") => Response::json(200, r#"{"refs":[{"key":"a/b","name":null}]}"#),
        ("GET", _) => Response::json(200, row),
        ("PUT", _) => Response::json(500, r#"{"error":"put boom"}"#),
        _ => Response::json(404, r#"{"error":"?"}"#),
    });
    let v = private_vault(&mock, x25519::Identity::generate());
    let e = v.migrate_private_refs(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("put boom"), "{e}");

    // DELETE fails → error surfaced after the new row landed
    let mock = canned(move |req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/dev") => Response::json(200, r#"{"refs":[{"key":"a/b","name":null}]}"#),
        ("GET", _) => Response::json(200, row),
        ("PUT", _) => Response::json(
            200,
            r#"{"version":1,"created_at":"2024-01-01T00:00:00Z","rotated_at":null}"#,
        ),
        _ => Response::json(500, r#"{"error":"del boom"}"#),
    });
    let v = private_vault(&mock, x25519::Identity::generate());
    let e = v.migrate_private_refs(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("del boom"), "{e}");
}
