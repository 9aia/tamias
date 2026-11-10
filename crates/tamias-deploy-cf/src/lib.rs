#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-deploy-cf — materializes the Cloudflare registry: the Worker
//! (`index.js`) + `wrangler.toml` (R2 binding), and the secret step. Runs
//! `wrangler deploy` for the operator; requires their CF account.
//!
//! The configured `limits.max_upload` is baked into `wrangler.toml`'s
//! `[vars] MAX_UPLOAD_BYTES` at deploy time — the Worker enforces it
//! server-side (the client preflight is cosmetic; this is authoritative).

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::Path;

use tamias_core::Error;
use tamias_deploy::Deploy;

/// The registry Worker source (bundled from `tamias-registry-cf/worker`).
pub const WORKER_JS: &str = include_str!("../worker/index.js");
/// The wrangler config (R2 binding) — `{{MAX_UPLOAD_BYTES}}` is the
/// deploy-time placeholder [`CfDeploy::deploy`] substitutes.
pub const WRANGLER_TOML: &str = include_str!("../worker/wrangler.toml");

/// Placeholder in [`WRANGLER_TOML`] replaced with the byte cap.
const MAX_UPLOAD_PLACEHOLDER: &str = "{{MAX_UPLOAD_BYTES}}";

/// Built-in upload cap when no limit is configured — mirrors
/// `tamias-config`'s `DEFAULT_MAX_UPLOAD` and the Worker's own fallback
/// (512 MiB).
const DEFAULT_MAX_UPLOAD: u64 = 512 * 1024 * 1024;

/// The Cloudflare registry deployer.
#[derive(Clone, Copy, Debug, Default)]
pub struct CfDeploy {
    /// Upload cap baked into the Worker — `None` → [`DEFAULT_MAX_UPLOAD`].
    max_upload: Option<u64>,
}

impl CfDeploy {
    /// A deployer baking `bytes` into the Worker's `MAX_UPLOAD_BYTES`.
    #[must_use]
    pub fn with_max_upload(mut self, bytes: u64) -> Self {
        self.max_upload = Some(bytes);
        self
    }
}

impl Deploy for CfDeploy {
    fn deploy(&self, out: &Path) -> Result<Vec<String>, Error> {
        std::fs::create_dir_all(out).map_err(Error::io(format!("create {}", out.display())))?;
        std::fs::write(out.join("index.js"), WORKER_JS).map_err(Error::io("write index.js"))?;
        let max_upload = self.max_upload.unwrap_or(DEFAULT_MAX_UPLOAD);
        if !WRANGLER_TOML.contains(MAX_UPLOAD_PLACEHOLDER) {
            return Err(Error::Failed(
                "wrangler.toml template lost its MAX_UPLOAD_BYTES placeholder".to_string(),
            ));
        }
        let wrangler = WRANGLER_TOML.replace(MAX_UPLOAD_PLACEHOLDER, &max_upload.to_string());
        std::fs::write(out.join("wrangler.toml"), wrangler)
            .map_err(Error::io("write wrangler.toml"))?;
        Ok(vec![
            format!("wrote registry worker → {}", out.display()),
            format!("upload cap: {max_upload} bytes (MAX_UPLOAD_BYTES)"),
            format!("next: (cd {} && wrangler deploy)", out.display()),
            "next: wrangler secret put REGISTRY_TOKEN   # the bearer token".to_string(),
        ])
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn deploy_writes_worker_and_config() {
        let out = tempfile::tempdir().unwrap();
        let actions = CfDeploy::default().deploy(out.path()).unwrap();
        assert!(out.path().join("index.js").is_file());
        assert!(out.path().join("wrangler.toml").is_file());
        assert!(actions.iter().any(|a| a.contains("wrangler deploy")));
        // Default: the built-in cap is baked, no placeholder leaks.
        let toml = std::fs::read_to_string(out.path().join("wrangler.toml")).unwrap();
        assert!(toml.contains(&format!("MAX_UPLOAD_BYTES = \"{}\"", DEFAULT_MAX_UPLOAD)));
        assert!(!toml.contains(MAX_UPLOAD_PLACEHOLDER));
    }

    #[test]
    fn deploy_bakes_configured_upload_limit() {
        let out = tempfile::tempdir().unwrap();
        CfDeploy::default()
            .with_max_upload(1024)
            .deploy(out.path())
            .unwrap();
        let toml = std::fs::read_to_string(out.path().join("wrangler.toml")).unwrap();
        assert!(toml.contains("MAX_UPLOAD_BYTES = \"1024\""), "{toml}");
    }

    #[test]
    fn worker_reads_upload_limit_and_rejects_over_limit() {
        // No JS harness here — pin the invariant in source, same style
        // as worker_auth_fails_closed: the worker reads the baked var
        // (with a fallback), checks Content-Length AND the buffered
        // body, and returns 413 before touching R2.
        assert!(WORKER_JS.contains("env.MAX_UPLOAD_BYTES"), "{WORKER_JS}");
        assert!(WORKER_JS.contains("DEFAULT_MAX_UPLOAD"));
        let put = WORKER_JS.find("request.method === \"PUT\"").unwrap();
        let guard = WORKER_JS[put..].find("status: 413").unwrap();
        let write = WORKER_JS[put..].find("env.REGISTRY.put(").unwrap();
        assert!(guard < write, "413 check must precede the R2 put");
        // Both the declared length and the actual body size are checked.
        let arm = &WORKER_JS[put..put + 1200];
        assert!(arm.contains("content-length"), "{arm}");
        assert!(arm.contains("byteLength"), "{arm}");
    }

    #[test]
    fn worker_auth_fails_closed() {
        // A missing REGISTRY_TOKEN secret must reject every request —
        // the old `env.REGISTRY_TOKEN && …` check failed OPEN, leaving
        // the blob store (index.json included) world-writable. There's
        // no JS harness here, so pin the invariant in source: the
        // missing-secret branch returns an error status and sits before
        // any REGISTRY access.
        let missing = WORKER_JS.find("!env.REGISTRY_TOKEN").unwrap();
        let guarded = WORKER_JS
            .find("env.REGISTRY.get(")
            .or_else(|| WORKER_JS.find("env.REGISTRY.put("))
            .unwrap();
        assert!(missing < guarded, "auth check must precede blob access");
        let branch = &WORKER_JS[missing..missing + 200];
        assert!(branch.contains("status: 500") || branch.contains("status: 401"));
    }
}
