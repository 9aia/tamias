//! The policy decision point — an *external* engine (peregrin's
//! job, never tamias's) that `tamias serve` consults before every
//! vault-touching op — and before `list`/`check`, which
//! materialize too (record decrypts, scoped-token gets, vault
//! probes).
//!
//! One driver: `exec`. The configured command runs via `sh -c`
//! once per decision; the [`PdpRequest`] goes in on stdin as one
//! JSON line, the verdict comes back on stdout —
//! `{"allow": bool, "reason"?: string, "obligations"?: [string]}`.
//! Spawn failure, non-zero exit, missing `allow`, unparseable
//! output, a timeout, or an oversized verdict are all
//! [`Decision::Unreachable`] — what that means is the caller's
//! `[serve.pdp] allow_on_error` flag.
//!
//! Hard bounds keep a misbehaving PDP from taking the broker
//! with it, all without leaving `std`:
//!
//! - **Deadline** — the child is polled with `try_wait` against
//!   `[serve.pdp] timeout_ms` (default 5 s) and killed past it;
//!   a hung decider can never freeze the serve loop.
//! - **Bounded verdict** — stdout is read through
//!   `Read::take(MAX_VERDICT_BYTES + 1)`; a flooding `yes`-style
//!   child fills its pipe, blocks, and dies on the deadline
//!   instead of sinking memory.
//! - **Non-blocking stdin** — the request is written on a helper
//!   thread so a >64 KiB request can't deadlock the loop against
//!   a child that never reads.
//! - **Scrubbed env** — the child inherits only the
//!   `tamias-shell` allowlist (same scrub `tamias run` applies);
//!   `TAMIAS_CF_TOKEN`, `CLOUDFLARE_API_TOKEN`, and every other
//!   parent credential stay out.
//!
//! The exec driver keeps the binary dependency-light and sync —
//! no HTTP client, no async runtime, no daemon protocol. A PDP
//! that wants a persistent socket can put `socat`/`nc` in its
//! exec line; the wire stays the same.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use tamias_core::PdpConfig;

/// The most a PDP verdict may be — 64 KiB. A verdict is a small
/// JSON object; a child that floods stdout is broken, not
/// chatty, and must not sink memory.
const MAX_VERDICT_BYTES: u64 = 64 * 1024;

/// One consult's deadline when `[serve.pdp] timeout_ms` is unset.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Grace to drain the verdict pipe after the child exits — a
/// grandchild inheriting stdout could otherwise hold the pipe
/// open forever.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// `try_wait` poll interval inside the consult deadline.
const POLL: Duration = Duration::from_millis(5);

/// What serve asks the PDP — one materialization decision.
#[derive(Debug, Serialize)]
pub struct PdpRequest<'a> {
    /// The claimed actor (self-asserted, advisory — attestation is
    /// where real proof would ride).
    pub actor: &'a str,
    /// The requester's opaque identity claim, passed through
    /// verbatim. tamias never interprets it; the PDP validates (or
    /// ignores) it. Real signing is peregrin's side — a later ADR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attestation: Option<&'a str>,
    /// The op being decided (`get`/`put`/`rm`/`rotate`/`list`/
    /// `check`).
    pub op: &'a str,
    /// The env the op resolves under (pin-aware).
    pub env: &'a str,
    /// The resolved ref path (env pin stripped) — absent for the
    /// ref-less `list`/`check` consults.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub rref: Option<&'a str>,
}

/// What the PDP answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The op may run; optional reason/obligations ride along.
    Allow {
        /// PDP-supplied rationale.
        reason: Option<String>,
        /// Conditions on the allow (`allow-with-conditions`).
        obligations: Option<Vec<String>>,
    },
    /// The op is refused.
    Deny {
        /// PDP-supplied rationale.
        reason: Option<String>,
    },
    /// No verdict was obtained — spawn failed, non-zero exit,
    /// timeout, oversized/unparseable output. The string is
    /// diagnostics, never policy input.
    Unreachable(String),
}

/// The exec-driver PDP — one `sh -c` spawn per decision.
pub struct ExecPdp {
    cmd: String,
    allow_on_error: bool,
    timeout: Duration,
}

impl ExecPdp {
    /// From `[serve.pdp]` config.
    pub fn new(cfg: &PdpConfig) -> Self {
        ExecPdp {
            cmd: cfg.exec.clone(),
            allow_on_error: cfg.allow_on_error,
            timeout: cfg
                .timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_TIMEOUT),
        }
    }

    /// `[serve.pdp] allow_on_error`.
    pub fn allow_on_error(&self) -> bool {
        self.allow_on_error
    }

    /// Ask the PDP. Never panics, never hangs — every failure
    /// shape (spawn, timeout, flood, garbage) folds into
    /// [`Decision::Unreachable`].
    pub fn decide(&self, req: &PdpRequest) -> Decision {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&self.cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The PDP's stderr is not the audit wire — null it so a
            // chatty decider can't pollute serve's event stream.
            .stderr(Stdio::null());
        // Same scrub `tamias run` applies — the PDP decides policy,
        // it doesn't need the broker's credentials to do it.
        tamias_shell::scrub_env(&mut command, &[]);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return Decision::Unreachable(format!("pdp spawn failed: {e}")),
        };
        // Serializing this POD request cannot fail — the expect
        // documents the invariant, it doesn't guard a real path.
        #[allow(clippy::unwrap_used)]
        let line = serde_json::to_string(req).expect("pdp request serializes");
        // The request can exceed the pipe buffer (a 64 KiB
        // attestation serializes bigger) — write on a helper
        // thread so a child that never reads can't deadlock the
        // consult. Killing the child unblocks the writer via EPIPE.
        let mut stdin = child.stdin.take();
        std::thread::spawn(move || {
            if let Some(mut s) = stdin.take() {
                let _ = writeln!(s, "{line}");
            }
            // stdin drops here → the child sees EOF.
        });
        // Read the verdict bounded, also on a helper thread: a
        // grandchild inheriting stdout could hold the pipe open
        // past the child's exit and stall the drain.
        let mut stdout = child.stdout.take();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(out) = stdout.take() {
                let _ = out.take(MAX_VERDICT_BYTES + 1).read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        // wait_timeout semantics without a new dependency: poll
        // try_wait against the deadline, kill on expiry. A
        // flooding child that out-writes the bounded reader blocks
        // on its own pipe and dies here — memory stays bounded.
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Decision::Unreachable(format!(
                        "pdp timed out after {}ms",
                        self.timeout.as_millis()
                    ));
                }
                Ok(None) => std::thread::sleep(POLL),
                Err(e) => return Decision::Unreachable(format!("pdp wait failed: {e}")),
            }
        };
        // The child exited so its own stdout is closed — a short
        // grace covers the reader finishing; a leaked grandchild
        // holding the pipe open must not stall the consult.
        let out = match rx.recv_timeout(DRAIN_GRACE) {
            Ok(buf) => buf,
            Err(_) => {
                return Decision::Unreachable("pdp stdout never drained".into());
            }
        };
        if !status.success() {
            return Decision::Unreachable(format!("pdp exited {status}"));
        }
        if out.len() as u64 > MAX_VERDICT_BYTES {
            return Decision::Unreachable(format!("pdp verdict exceeds {MAX_VERDICT_BYTES} bytes"));
        }
        parse_verdict(&out)
    }
}

/// The verdict shape the PDP owes on stdout.
#[derive(Debug, serde::Deserialize)]
struct Verdict {
    allow: bool,
    reason: Option<String>,
    obligations: Option<Vec<String>>,
}

fn parse_verdict(stdout: &[u8]) -> Decision {
    match serde_json::from_slice::<Verdict>(stdout) {
        Ok(v) if v.allow => Decision::Allow {
            reason: v.reason,
            obligations: v.obligations,
        },
        Ok(v) => Decision::Deny { reason: v.reason },
        Err(e) => Decision::Unreachable(format!("pdp verdict unparseable: {e}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tamias_core::PdpConfig;

    fn pdp(exec: &str, allow_on_error: bool) -> ExecPdp {
        ExecPdp::new(&PdpConfig {
            exec: exec.into(),
            allow_on_error,
            timeout_ms: None,
        })
    }

    fn req() -> PdpRequest<'static> {
        PdpRequest {
            actor: "agent:test",
            attestation: Some("fake-attestation"),
            op: "get",
            env: "dev",
            rref: Some("a/b"),
        }
    }

    #[test]
    fn allow_deny_and_obligations_parse() {
        let d = pdp(
            "printf '%s' '{\"allow\":true,\"reason\":\"ok\",\"obligations\":[\"redact\"]}'",
            false,
        )
        .decide(&req());
        assert_eq!(
            d,
            Decision::Allow {
                reason: Some("ok".into()),
                obligations: Some(vec!["redact".into()])
            }
        );
        let d = pdp(
            "printf '%s' '{\"allow\":false,\"reason\":\"no prod\"}'",
            false,
        )
        .decide(&req());
        assert_eq!(
            d,
            Decision::Deny {
                reason: Some("no prod".into())
            }
        );
        // a bare allow carries no extras
        let d = pdp("printf '%s' '{\"allow\":true}'", false).decide(&req());
        assert_eq!(
            d,
            Decision::Allow {
                reason: None,
                obligations: None
            }
        );
    }

    #[test]
    fn every_failure_shape_is_unreachable() {
        // unspawnable command
        assert!(matches!(
            pdp("/nonexistent-pdp-binary-xyz", false).decide(&req()),
            Decision::Unreachable(_)
        ));
        // non-zero exit
        assert!(matches!(
            pdp("exit 3", false).decide(&req()),
            Decision::Unreachable(_)
        ));
        // garbage on stdout
        assert!(matches!(
            pdp("printf 'not json'", false).decide(&req()),
            Decision::Unreachable(_)
        ));
        // missing `allow` key
        assert!(matches!(
            pdp("printf '%s' '{\"reason\":\"hmm\"}'", false).decide(&req()),
            Decision::Unreachable(_)
        ));
        // a pdp that closes stdin without reading doesn't kill us
        assert_eq!(
            pdp("printf '%s' '{\"allow\":true}'", false).decide(&req()),
            Decision::Allow {
                reason: None,
                obligations: None
            }
        );
    }

    #[test]
    fn the_request_rides_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let captured = dir.path().join("req.json");
        let cmd = format!(
            "cat > {}; printf '%s' '{{\"allow\":true}}'",
            captured.display()
        );
        assert_eq!(
            pdp(&cmd, false).decide(&req()),
            Decision::Allow {
                reason: None,
                obligations: None
            }
        );
        let sent: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&captured).unwrap()).unwrap();
        assert_eq!(sent["actor"], "agent:test");
        assert_eq!(sent["attestation"], "fake-attestation");
        assert_eq!(sent["op"], "get");
        assert_eq!(sent["env"], "dev");
        assert_eq!(sent["ref"], "a/b");
    }

    #[test]
    fn refless_requests_omit_the_ref() {
        let dir = tempfile::tempdir().unwrap();
        let captured = dir.path().join("req.json");
        let cmd = format!(
            "cat > {}; printf '%s' '{{\"allow\":true}}'",
            captured.display()
        );
        let r = PdpRequest {
            rref: None,
            ..req()
        };
        assert!(matches!(
            pdp(&cmd, false).decide(&r),
            Decision::Allow { .. }
        ));
        let sent: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&captured).unwrap()).unwrap();
        assert!(sent.get("ref").is_none(), "{sent}");
    }

    #[test]
    fn a_hung_pdp_is_killed_at_the_deadline() {
        let p = ExecPdp::new(&PdpConfig {
            exec: "sleep 60".into(),
            allow_on_error: false,
            timeout_ms: Some(200),
        });
        let start = Instant::now();
        match p.decide(&req()) {
            Decision::Unreachable(why) => assert!(why.contains("timed out"), "{why}"),
            d => panic!("expected unreachable, got {d:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_flooding_pdp_stays_bounded_and_dies() {
        // `yes` writes forever — the bounded reader stops, the
        // child blocks on a full pipe, the deadline kills it.
        let p = ExecPdp::new(&PdpConfig {
            exec: "yes".into(),
            allow_on_error: false,
            timeout_ms: Some(500),
        });
        let start = Instant::now();
        assert!(matches!(p.decide(&req()), Decision::Unreachable(_)));
        assert!(start.elapsed() < Duration::from_secs(10));
        // an oversized-but-finite verdict is unreachable too
        let p = ExecPdp::new(&PdpConfig {
            exec: "head -c 100000 /dev/zero".into(),
            allow_on_error: false,
            timeout_ms: Some(5000),
        });
        match p.decide(&req()) {
            Decision::Unreachable(why) => assert!(why.contains("exceeds"), "{why}"),
            d => panic!("expected unreachable, got {d:?}"),
        }
    }

    #[test]
    fn the_pdp_child_env_is_scrubbed() {
        temp_env::with_var("TAMIAS_CF_TOKEN", Some("fake-parent-token"), || {
            // If the var leaked, printenv succeeds and the pdp
            // "denies"; scrubbed, it fails and the pdp allows.
            let d = pdp(
                "printenv TAMIAS_CF_TOKEN >/dev/null \
                 && printf '%s' '{\"allow\":false}' \
                 || printf '%s' '{\"allow\":true}'",
                false,
            )
            .decide(&req());
            assert_eq!(
                d,
                Decision::Allow {
                    reason: None,
                    obligations: None
                }
            );
        });
    }

    #[test]
    fn allow_on_error_flag_passthrough() {
        assert!(pdp("x", true).allow_on_error());
        assert!(!pdp("x", false).allow_on_error());
    }
}
