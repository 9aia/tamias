#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-registry-cf — a registry over an HTTP blob store: the Cloudflare
//! Worker (`worker/index.js`) backed by R2, or any host speaking the same
//! protocol (`GET /<key>`, `PUT /<key>`). The **client** maintains the
//! `index.json` (read-modify-write); the server is a dumb, token-guarded
//! blob store. Auth is a bearer token — from `token_env` / cuora, never
//! inline.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

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

    /// A bearer token for auth (from `token_env`/cuora).
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

/// A per-call-unique temp path for the transient publish tarball, so
/// concurrent publishes in one process don't clobber each other's file.
fn publish_tmp_path(version: &str) -> std::path::PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!(
        "tamias-pub-{}-{nanos}-{seq}-{version}.tgz",
        std::process::id()
    ))
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
        let tmp = publish_tmp_path(&version);
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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn entries_without_server_is_empty_and_env_token() {
        let registry = HttpRegistry::new("http://127.0.0.1:1");
        assert!(registry.entries().unwrap().is_empty());
        assert!(HttpRegistry::from_env("http://127.0.0.1:1")
            .with_token("t")
            .entries()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn publish_without_server_errors() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        let registry = HttpRegistry::new("http://127.0.0.1:1");
        assert!(registry.publish(src.path(), &manifest).is_err());
    }

    #[test]
    fn publish_and_read_roundtrip() {
        use tamias_registry_local::serve_http;
        let reg = tempfile::tempdir().unwrap();
        let root = reg.path().to_path_buf();
        std::thread::spawn(move || {
            let _ = serve_http("127.0.0.1:8793", &root);
        });
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        let registry = HttpRegistry::new("http://127.0.0.1:8793");
        for _ in 0..100 {
            if std::net::TcpStream::connect("127.0.0.1:8793").is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(
            registry.publish(src.path(), &manifest).unwrap().label(),
            "x@1.0.0"
        );
        assert_eq!(registry.entries().unwrap().len(), 1);
    }

    fn serve_500() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = s
                    .write_all(b"HTTP/1.1 500 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = s.flush();
            }
        });
        port
    }

    #[test]
    fn server_error_status_paths() {
        let base = format!("http://127.0.0.1:{}", serve_500());
        let registry = HttpRegistry::new(&base);
        assert!(registry.entries().unwrap().is_empty());
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        assert!(registry.publish(src.path(), &manifest).is_err());
    }

    #[test]
    fn scoped_publish_put_token_and_overwrite() {
        use tamias_registry_local::serve_http;
        let reg = tempfile::tempdir().unwrap();
        let root = reg.path().to_path_buf();
        std::thread::spawn(move || {
            let _ = serve_http("127.0.0.1:8796", &root);
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect("127.0.0.1:8796").is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("tamias.yaml"),
            "name: s\nscope: acme\nversion: 1.0.0\n",
        )
        .unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        let registry = HttpRegistry::new("http://127.0.0.1:8796").with_token("tok");
        assert_eq!(
            registry.publish(src.path(), &manifest).unwrap().label(),
            "@acme/s@1.0.0"
        );
        // re-publish → the existing index entry is replaced
        assert_eq!(
            registry.publish(src.path(), &manifest).unwrap().label(),
            "@acme/s@1.0.0"
        );
        assert_eq!(registry.entries().unwrap().len(), 1);
    }

    #[test]
    fn from_env_token_header_is_sent() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]");
            let _ = s.flush();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        std::env::set_var("TAMIAS_REGISTRY_TOKEN", "sekret");
        let registry = HttpRegistry::from_env(format!("http://127.0.0.1:{port}"));
        let _ = registry.entries().unwrap();
        std::env::remove_var("TAMIAS_REGISTRY_TOKEN");
        let req = h.join().unwrap().to_lowercase();
        assert!(req.contains("authorization: bearer sekret"), "{req}");
    }

    #[test]
    fn put_non_success_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..2 {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                if buf[..n].starts_with(b"PUT") {
                    let _ = s.write_all(
                        b"HTTP/1.1 500 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else {
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
                    );
                }
                let _ = s.flush();
            }
        });
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        let registry = HttpRegistry::new(format!("http://127.0.0.1:{port}"));
        assert!(registry.publish(src.path(), &manifest).is_err());
    }
}
