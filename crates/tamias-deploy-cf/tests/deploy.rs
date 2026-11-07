//! `tamias-deploy-cf` against a mock of the CF API v4 Secrets Store
//! surface — request construction, upsert semantics, and the
//! envelope error path. Values are fake by rule.

#![allow(clippy::unwrap_used)]

use tamias_core::{Env, Ref, Secret};
use tamias_deploy::Deploy;
use tamias_deploy_cf::CfSecretsStore;
use tamias_testkit::mocks;

const FAKE: &str = "fake-value-not-a-real-secret";
const TOKEN: &str = "fake-cf-token";

fn target(url: &str) -> CfSecretsStore {
    CfSecretsStore::with_token("prod", "acc", "sto", TOKEN, Some(url)).unwrap()
}

#[test]
fn push_creates_then_updates() {
    let (mock, seen) = mocks::cf_secrets_store("acc", "sto", TOKEN);
    let t = target(mock.url());
    let env = Env::new("prod").unwrap();
    let rref = Ref::new("tamias://billing/stripe").unwrap();

    let report = t.push(&env, &rref, &Secret::new(FAKE)).unwrap();
    assert_eq!(report.name, "prod_billing_stripe");
    assert!(report.created);

    // create used POST with the array body carrying the value
    let post = seen
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.method == "POST")
        .unwrap()
        .text();
    assert!(post.contains(FAKE), "POST body: {post}");
    assert!(post.contains("prod_billing_stripe"), "{post}");

    // second push: search finds it → PATCH on the id path
    let report = t.push(&env, &rref, &Secret::new("fake-v2")).unwrap();
    assert!(!report.created);
    let patch = seen
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.method == "PATCH")
        .unwrap()
        .clone();
    assert!(patch.path.contains("/secrets/id0001"), "{:?}", patch.path);
    assert!(patch.text().contains("fake-v2"));
}

#[test]
fn remove_true_then_false() {
    let (mock, _seen) = mocks::cf_secrets_store("acc", "sto", TOKEN);
    let t = target(mock.url());
    let env = Env::new("prod").unwrap();
    let rref = Ref::new("tamias://x/y").unwrap();
    t.push(&env, &rref, &Secret::new(FAKE)).unwrap();
    assert!(t.remove(&env, &rref).unwrap());
    assert!(!t.remove(&env, &rref).unwrap());
}

#[test]
fn bad_token_maps_to_unauthorized() {
    let (mock, _seen) = mocks::cf_secrets_store("acc", "sto", TOKEN);
    let t = CfSecretsStore::with_token("prod", "acc", "sto", "wrong", Some(mock.url())).unwrap();
    let err = t
        .push(
            &Env::dev(),
            &Ref::new("tamias://x/y").unwrap(),
            &Secret::new(FAKE),
        )
        .unwrap_err();
    assert!(
        matches!(err, tamias_deploy::DeployError::Unauthorized(_)),
        "{err}"
    );
}

#[test]
fn sink_names_are_stable() {
    let env = Env::new("prod").unwrap();
    let rref = Ref::new("tamias://billing/stripe-key").unwrap();
    assert_eq!(
        CfSecretsStore::sink_name(&env, &rref),
        "prod_billing_stripe_key"
    );
}

/// A server that answers the first request with `first`, then
/// drops every later connection mid-flight — the deterministic
/// way to make a *specific* verb (PATCH/POST/DELETE) hit a
/// transport error after `find_id`'s GET succeeded.
fn die_after_first(first: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for i in 0..2 {
            if let Ok((mut s, _)) = listener.accept() {
                if i == 0 {
                    use std::io::Write;
                    let mut buf = [0u8; 8192];
                    let _ = std::io::Read::read(&mut s, &mut buf);
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            first.len(),
                            first
                        )
                        .as_bytes(),
                    );
                } // second conn: drop — connection dies mid-request
            }
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// A mock whose GET search finds an existing secret — every later
/// verb's response is `answer`ed by the caller.
fn mock_with_existing(
    answer: &'static str,
) -> (
    tamias_testkit::httpmock::HttpMock,
    std::sync::Arc<std::sync::Mutex<Vec<tamias_testkit::httpmock::Request>>>,
) {
    use tamias_testkit::httpmock::{HttpMock, Request, Response};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s = seen.clone();
    let mock = HttpMock::new(move |req: &Request| {
        s.lock().unwrap().push(req.clone());
        if req.method == "GET" {
            Response::json(
                200,
                r#"{"success":true,"errors":[],"result":[{"id":"id0001","name":"dev_x_y"}]}"#,
            )
        } else {
            Response::json(200, answer)
        }
    });
    (mock, seen)
}

#[test]
fn find_id_treats_malformed_and_missing_results_as_absent() {
    use tamias_testkit::httpmock::{HttpMock, Response};
    // result rows without a matching name → create path (POST)
    let mock = HttpMock::new(|req: &tamias_testkit::httpmock::Request| {
        if req.method == "GET" {
            Response::json(
                200,
                r#"{"success":true,"errors":[],"result":[{"id":"id9","name":"other_name"},{"name":"dev_x_y"}]}"#,
            )
        } else {
            Response::json(200, r#"{"success":true,"errors":[],"result":[]}"#)
        }
    });
    let t = target(mock.url());
    let report = t
        .push(
            &Env::dev(),
            &Ref::new("tamias://x/y").unwrap(),
            &Secret::new(FAKE),
        )
        .unwrap();
    assert!(report.created, "no usable id → POST create, not PATCH");
}

#[test]
fn envelope_failures_on_each_verb_are_errors() {
    let env = Env::dev();
    let r = Ref::new("tamias://x/y").unwrap();
    // PATCH → envelope success:false → clean error
    let (mock, _) = mock_with_existing(r#"{"success":false,"errors":[{"message":"read-only"}]}"#);
    let t = target(mock.url());
    let e = t.push(&env, &r, &Secret::new(FAKE)).unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");
    // DELETE → same
    let e = t.remove(&env, &r).unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");
}

#[test]
fn post_and_delete_envelope_failures_are_errors() {
    use tamias_testkit::httpmock::{HttpMock, Response};
    // GET finds nothing, POST answers success:false
    let mock = HttpMock::new(|req: &tamias_testkit::httpmock::Request| {
        if req.method == "GET" {
            Response::json(200, r#"{"success":true,"errors":[],"result":[]}"#)
        } else {
            Response::json(500, r#"{"success":false,"errors":[{"message":"quota"}]}"#)
        }
    });
    let t = target(mock.url());
    let e = t
        .push(
            &Env::dev(),
            &Ref::new("tamias://x/y").unwrap(),
            &Secret::new(FAKE),
        )
        .unwrap_err();
    assert!(e.to_string().contains("quota"), "{e}");
}

#[test]
fn transport_death_on_patch_post_and_delete_are_io_errors() {
    let env = Env::dev();
    let r = Ref::new("tamias://x/y").unwrap();
    // GET finds the secret, PATCH conn dies → Io, not a panic
    let url = die_after_first(
        r#"{"success":true,"errors":[],"result":[{"id":"id0001","name":"dev_x_y"}]}"#,
    );
    let t = target(&url);
    assert!(matches!(
        t.push(&env, &r, &Secret::new(FAKE)),
        Err(tamias_deploy::DeployError::Io(_))
    ));
    // GET finds nothing, POST conn dies → Io
    let url = die_after_first(r#"{"success":true,"errors":[],"result":[]}"#);
    let t = target(&url);
    assert!(matches!(
        t.push(&env, &r, &Secret::new(FAKE)),
        Err(tamias_deploy::DeployError::Io(_))
    ));
    // GET finds the secret, DELETE conn dies → Io
    let url = die_after_first(
        r#"{"success":true,"errors":[],"result":[{"id":"id0001","name":"dev_x_y"}]}"#,
    );
    let t = target(&url);
    assert!(matches!(
        t.remove(&env, &r),
        Err(tamias_deploy::DeployError::Io(_))
    ));
}

#[test]
fn a_body_short_of_content_length_is_an_io_error() {
    // server promises 100 bytes, sends 5, drops — the read fails
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        use std::io::Write;
        let mut buf = [0u8; 4096];
        let _ = std::io::Read::read(&mut s, &mut buf);
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort");
    });
    let t = target(&format!("http://127.0.0.1:{port}"));
    let e = t
        .push(
            &Env::dev(),
            &Ref::new("tamias://x/y").unwrap(),
            &Secret::new(FAKE),
        )
        .unwrap_err();
    assert!(matches!(e, tamias_deploy::DeployError::Io(_)), "{e}");
}

#[test]
fn malformed_requests_get_cf_errors_not_panics() {
    let (mock, _seen) = mocks::cf_secrets_store("acc", "sto", TOKEN);
    let auth = format!("Bearer {TOKEN}");
    let url = mock.url().to_string();
    let base = format!("{url}/accounts/acc/secrets_store/stores/sto/secrets");
    let status = |r: Result<ureq::http::Response<ureq::Body>, ureq::Error>| match r {
        Ok(res) => res.status().as_u16(),
        Err(ureq::Error::StatusCode(code)) => code,
        Err(e) => panic!("transport error: {e}"),
    };
    // PATCH/DELETE an id the store doesn't hold → the 404 arm
    let r = ureq::patch(format!("{base}/id9999"))
        .header("Authorization", &auth)
        .content_type("application/json")
        .config()
        .http_status_as_error(false)
        .build()
        .send("{}");
    assert_eq!(status(r), 404);
    let r = ureq::delete(format!("{base}/id9998"))
        .header("Authorization", &auth)
        .config()
        .http_status_as_error(false)
        .build()
        .call();
    assert_eq!(status(r), 404);
    // a route shape the mock doesn't route → the catch-all 404
    let r = ureq::put(format!("{base}/id9997"))
        .header("Authorization", &auth)
        .content_type("application/json")
        .config()
        .http_status_as_error(false)
        .build()
        .send("{}");
    assert_eq!(status(r), 404);
}
