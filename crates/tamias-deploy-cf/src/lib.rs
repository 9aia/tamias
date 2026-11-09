#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-deploy-cf — materializes the Cloudflare registry: the Worker
//! (`index.js`) + `wrangler.toml` (R2 binding), and the secret step. Runs
//! `wrangler deploy` for the operator; requires their CF account.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::Path;

use tamias_core::Error;
use tamias_deploy::Deploy;

/// The registry Worker source (bundled from `tamias-registry-cf/worker`).
pub const WORKER_JS: &str = include_str!("../worker/index.js");
/// The wrangler config (R2 binding).
pub const WRANGLER_TOML: &str = include_str!("../worker/wrangler.toml");

/// The Cloudflare registry deployer.
#[derive(Clone, Copy, Debug, Default)]
pub struct CfDeploy;

impl Deploy for CfDeploy {
    fn deploy(&self, out: &Path) -> Result<Vec<String>, Error> {
        std::fs::create_dir_all(out).map_err(Error::io(format!("create {}", out.display())))?;
        std::fs::write(out.join("index.js"), WORKER_JS).map_err(Error::io("write index.js"))?;
        std::fs::write(out.join("wrangler.toml"), WRANGLER_TOML)
            .map_err(Error::io("write wrangler.toml"))?;
        Ok(vec![
            format!("wrote registry worker → {}", out.display()),
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
        let actions = CfDeploy.deploy(out.path()).unwrap();
        assert!(out.path().join("index.js").is_file());
        assert!(out.path().join("wrangler.toml").is_file());
        assert!(actions.iter().any(|a| a.contains("wrangler deploy")));
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
