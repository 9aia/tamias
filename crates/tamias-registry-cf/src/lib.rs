//! tamias-registry-cf — a registry over an HTTP blob store: the Cloudflare
//! Worker (`worker/index.js`) backed by R2, or any host speaking the same
//! protocol (`GET /<key>`, `PUT /<key>`). The **client** maintains the
//! `index.json` (read-modify-write); the server is a dumb, token-guarded
//! blob store. Auth is a bearer token — from `token_env` / arca, never
//! inline.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::Path;

use tamias_core::{dir_digest, pack_tgz, Error, Manifest};
use tamias_registry::{Registry, RegistryEntry};

/// A registry over an http(s) blob store.
pub struct HttpRegistry {
    base: String,
    agent: ureq::Agent,
    token: Option<String>,
}

impl HttpRegistry {
    /// A registry at `base` (trailing `/` trimmed).
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            agent: ureq::Agent::new_with_config(
                ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .build(),
            ),
            token: None,
        }
    }

    /// A bearer token for auth (from `token_env`/arca).
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Build a registry resolved from env: `TAMIAS_REGISTRY_TOKEN` is the
    /// bearer token.
    #[must_use]
    pub fn from_env(base: impl Into<String>) -> Self {
        let mut registry = Self::new(base);
        if let Ok(token) = std::env::var("TAMIAS_REGISTRY_TOKEN") {
            registry = registry.with_token(token);
        }
        registry
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, Error> {
        let url = format!("{}/{key}", self.base);
        let mut req = self.agent.get(&url);
        if let Some(token) = &self.token {
            req = req.header("authorization", &format!("Bearer {token}"));
        }
        let mut resp = req
            .call()
            .map_err(|e| Error::Failed(format!("GET {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Failed(format!("GET {url}: HTTP {}", resp.status())));
        }
        resp.body_mut()
            .with_config()
            .read_to_vec()
            .map_err(|e| Error::Failed(format!("read {url}: {e}")))
    }

    fn put(&self, key: &str, body: &[u8]) -> Result<(), Error> {
        let url = format!("{}/{key}", self.base);
        let mut req = self.agent.put(&url);
        if let Some(token) = &self.token {
            req = req.header("authorization", &format!("Bearer {token}"));
        }
        let resp = req
            .send(body)
            .map_err(|e| Error::Failed(format!("PUT {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Failed(format!("PUT {url}: HTTP {}", resp.status())));
        }
        Ok(())
    }
}

impl Registry for HttpRegistry {
    fn entries(&self) -> Result<Vec<RegistryEntry>, Error> {
        match self.get("index.json") {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| Error::Failed(format!("parse index: {e}"))),
            Err(_) => Ok(Vec::new()),
        }
    }

    fn publish(&self, payload_root: &Path, manifest: &Manifest) -> Result<RegistryEntry, Error> {
        let scoped = match &manifest.scope {
            Some(scope) => format!("@{scope}/{}", manifest.name),
            None => manifest.name.clone(),
        };
        let version = manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_string());
        let rel = format!("{scoped}/{version}.tgz");
        let tmp =
            std::env::temp_dir().join(format!("tamias-pub-{}-{}.tgz", std::process::id(), version));
        pack_tgz(payload_root, &tmp)?;
        let bytes = std::fs::read(&tmp).map_err(Error::io(format!("read {}", tmp.display())))?;
        let _ = std::fs::remove_file(&tmp);
        self.put(&rel, &bytes)?;

        let entry = RegistryEntry {
            name: manifest.name.clone(),
            scope: manifest.scope.clone(),
            version: manifest.version.clone(),
            kind: manifest.kind.clone(),
            digest: dir_digest(payload_root)?,
            file: rel,
        };
        let mut entries = self.entries()?;
        entries.retain(|e| {
            !(e.name == entry.name && e.scope == entry.scope && e.version == entry.version)
        });
        entries.push(entry.clone());
        entries
            .sort_by(|a, b| (&a.scope, &a.name, &a.version).cmp(&(&b.scope, &b.name, &b.version)));
        let index = serde_json::to_vec_pretty(&entries)
            .map_err(|e| Error::Failed(format!("serialize index: {e}")))?;
        self.put("index.json", &index)?;
        Ok(entry)
    }
}
