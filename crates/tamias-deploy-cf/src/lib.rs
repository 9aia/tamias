//! tamias-deploy-cf — materializes the Cloudflare registry: the Worker
//! (`index.js`) + `wrangler.toml` (R2 binding), and the secret step. Runs
//! `wrangler deploy` for the operator; requires their CF account.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::Path;

use tamias_core::Error;
use tamias_deploy::Deploy;

/// The registry Worker source (bundled from `tamias-registry-cf/worker`).
pub const WORKER_JS: &str = include_str!("../../tamias-registry-cf/worker/index.js");
/// The wrangler config (R2 binding).
pub const WRANGLER_TOML: &str = include_str!("../../tamias-registry-cf/worker/wrangler.toml");

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
