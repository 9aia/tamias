//! `tamias-vault-cf` failure modes over real HTTP — malformed rows,
//! wrong statuses, torn responses. A remote store can say almost
//! anything; every surprise must be a clean `VaultError`, never a
//! panic. Values and tokens are fake by rule.

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::net::TcpListener;

use age::x25519;
use tamias_core::{Env, Ref, Secret};
use tamias_testkit::httpmock::{HttpMock, Response};
use tamias_testkit::mocks;
use tamias_vault::Vault;
use tamias_vault_cf::CfVault;

const TOKEN: &str = "test-token-not-real";

fn vault_on(mock: &HttpMock) -> CfVault {
    CfVault::with_identity(mock.url(), TOKEN, x25519::Identity::generate(), &[]).unwrap()
}

fn rref() -> Ref {
    Ref::new("tamias://x/y").unwrap()
}

/// A canned-response vault mock — whatever the handler says goes.
fn canned(
    f: impl Fn(&tamias_testkit::httpmock::Request) -> Response + Send + Sync + 'static,
) -> HttpMock {
    HttpMock::new(f)
}

#[test]
fn get_surfaces_row_shape_and_crypto_failures() {
    // 200 with non-row json → parse error
    let mock = canned(|_| Response::json(200, "not a row"));
    assert!(vault_on(&mock).get(&Env::dev(), &rref()).is_err());

    // 200 row missing the ciphertext field → backend error
    let mock = canned(|_| {
        Response::json(
            200,
            r#"{"version":1,"created_at":"2024-01-01T00:00:00Z","rotated_at":null}"#,
        )
    });
    let e = vault_on(&mock).get(&Env::dev(), &rref()).unwrap_err();
    assert!(e.to_string().contains("ciphertext"), "{e}");

    // ciphertext that isn't base64 → decode error
    let mock = canned(|_| {
        Response::json(
            200,
            r#"{"ciphertext":"!!!not-b64!!!","version":1,"created_at":"2024-01-01T00:00:00Z","rotated_at":null}"#,
        )
    });
    assert!(vault_on(&mock).get(&Env::dev(), &rref()).is_err());

    // valid b64 but not age ciphertext → crypto error
    let mock = canned(|_| {
        Response::json(
            200,
            r#"{"ciphertext":"Z2FyYmFnZQ==","version":1,"created_at":"2024-01-01T00:00:00Z","rotated_at":null}"#,
        )
    });
    assert!(vault_on(&mock).get(&Env::dev(), &rref()).is_err());

    // a 500 → the error body is surfaced
    let mock = canned(|_| Response::json(500, r#"{"error":"d1 gone"}"#));
    let e = vault_on(&mock).get(&Env::dev(), &rref()).unwrap_err();
    assert!(e.to_string().contains("d1 gone"), "{e}");
}

#[test]
fn rm_list_and_write_surface_their_error_shapes() {
    // rm: 200 with non-RmBody json → parse error
    let mock = canned(|_| Response::json(200, "42"));
    assert!(vault_on(&mock).rm(&Env::dev(), &rref()).is_err());

    // rm: non-2xx → the error body is surfaced
    let mock = canned(|_| Response::json(500, r#"{"error":"rm boom"}"#));
    let e = vault_on(&mock).rm(&Env::dev(), &rref()).unwrap_err();
    assert!(e.to_string().contains("rm boom"), "{e}");

    // list: 200 with non-ListBody json → parse error
    let mock = canned(|_| Response::json(200, "[]"));
    assert!(vault_on(&mock).list(&Env::dev()).is_err());

    // list: a stored key that isn't a valid ref → backend error
    let mock = canned(|_| Response::json(200, r#"{"refs":["bad key!"]}"#));
    let e = vault_on(&mock).list(&Env::dev()).unwrap_err();
    assert!(e.to_string().contains("stored key"), "{e}");

    // list: non-2xx → error surfaced
    let mock = canned(|_| Response::json(500, r#"{"error":"list boom"}"#));
    assert!(vault_on(&mock).list(&Env::dev()).is_err());

    // write: rotate a missing ref → 404 maps to VaultError::NotFound
    let mock = mocks::vault_wire(TOKEN);
    let v = vault_on(&mock);
    let err = v
        .rotate(&Env::dev(), &rref(), Secret::new("fake"))
        .unwrap_err();
    assert!(
        matches!(err, tamias_vault::VaultError::NotFound { .. }),
        "{err}"
    );

    // write: non-2xx other than 404 → error surfaced
    let mock = canned(|_| Response::json(500, r#"{"error":"put boom"}"#));
    assert!(vault_on(&mock)
        .put(&Env::dev(), &rref(), Secret::new("f"))
        .is_err());
    // a 200 that doesn't parse as a row → parse error
    let mock = canned(|_| Response::json(200, "nope"));
    assert!(vault_on(&mock)
        .put(&Env::dev(), &rref(), Secret::new("f"))
        .is_err());
}

#[test]
fn transport_failures_are_io_errors_per_verb() {
    // nothing listens → each verb's request reports VaultError::Io
    let v = CfVault::with_identity("http://127.0.0.1:1", "t", x25519::Identity::generate(), &[])
        .unwrap();
    assert!(matches!(
        v.get(&Env::dev(), &rref()),
        Err(tamias_vault::VaultError::Io(_))
    ));
    assert!(matches!(
        v.rm(&Env::dev(), &rref()),
        Err(tamias_vault::VaultError::Io(_))
    ));
    assert!(matches!(
        v.put(&Env::dev(), &rref(), Secret::new("f")),
        Err(tamias_vault::VaultError::Io(_))
    ));
    assert!(matches!(
        v.list(&Env::dev()),
        Err(tamias_vault::VaultError::Io(_))
    ));
}

#[test]
fn a_body_short_of_content_length_is_an_io_error() {
    // One-shot server that promises 100 bytes, sends 5, then
    // drops — the body read must fail, not truncate silently.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = std::io::Read::read(&mut s, &mut buf);
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort");
    });
    let v = CfVault::with_identity(
        &format!("http://127.0.0.1:{port}"),
        "t",
        x25519::Identity::generate(),
        &[],
    )
    .unwrap();
    assert!(matches!(
        v.get(&Env::dev(), &rref()),
        Err(tamias_vault::VaultError::Io(_))
    ));
}
