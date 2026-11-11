#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-registry-cf — a registry over an HTTP blob store: the Cloudflare
//! Worker (`tamias-deploy-cf` ships `worker/index.js`) backed by R2, or any host speaking the same
//! protocol (`GET /<key>`, `PUT /<key>`). The **client** maintains the
//! `index.json` (read-modify-write); the server is a dumb, token-guarded
//! blob store. Auth is a bearer token — from `token_env` / cuora, never
//! inline.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use tamias_core::{dir_digest, is_valid_name, is_valid_version, pack_tgz, Error, Manifest};
use tamias_registry::{apply_retention, PublishOutcome, Registry, RegistryEntry};

/// Largest `.tgz` blob a publish will PUT by default — mirrors
/// `tamias-config`'s `DEFAULT_MAX_UPLOAD` (512 MiB);
/// [`HttpRegistry::with_max_upload`] overrides with the configured
/// `limits.max_upload`. Bigger payloads can't be installed anyway.
const DEFAULT_MAX_UPLOAD: u64 = 512 * 1024 * 1024;

/// Largest response body a blob GET buffers — mirrors the fetch-side
/// `DEFAULT_MAX_DOWNLOAD` in `tamias-source-http` (512 MiB). The client
/// only GETs `index.json` today (capped tighter by `MAX_INDEX`); this
/// guards any future blob read.
const DEFAULT_MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

/// `index.json` is a small catalog — cap its GET well below the blob
/// limit so a hostile/buggy store can't OOM the client.
const MAX_INDEX: u64 = 16 * 1024 * 1024;

/// A registry over an http(s) blob store.
pub struct HttpRegistry {
    base: String,
    agent: ureq::Agent,
    token: Option<String>,
    /// Upload cap checked against the packed tarball before the PUT —
    /// the configured `limits.max_upload` when the caller threads it
    /// through. The server stays authoritative regardless.
    max_upload: u64,
    /// Index retention (P-024) — `Some(keep)` prunes the published
    /// package's older index entries beyond the `keep` most recent
    /// semver versions on every publish (client-side: this client owns
    /// the index read-modify-write; the Worker is a dumb blob store).
    /// `None` (the default) keeps every version. Payload blobs are
    /// never pruned.
    retention: Option<u64>,
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
                    .timeout_connect(Some(std::time::Duration::from_secs(15)))
                    .timeout_global(Some(std::time::Duration::from_secs(300)))
                    .build(),
            ),
            token: None,
            max_upload: DEFAULT_MAX_UPLOAD,
            retention: None,
        }
    }

    /// A bearer token for auth (from `token_env`/cuora).
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Cap publishes at `bytes` — the configured `limits.max_upload`.
    /// Best-effort client preflight; the server enforces its own limit.
    #[must_use]
    pub fn with_max_upload(mut self, bytes: u64) -> Self {
        self.max_upload = bytes;
        self
    }

    /// Retain only the `keep` most recent semver versions per package
    /// in the index (P-024) — `None` disables pruning. The resolved
    /// `retention.keep` from config (or `publish --keep`).
    #[must_use]
    pub fn with_retention(mut self, keep: Option<u64>) -> Self {
        self.retention = keep;
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

    /// `GET <key>` → the body, or `None` when the store has no such key
    /// (HTTP 404). Every other failure — transport error, 5xx, auth
    /// rejection, unreadable body — is an `Err`. Callers must never
    /// treat a failed read as "the key is absent": for `index.json` that
    /// would let a transient outage masquerade as an empty registry (and
    /// a publish could then overwrite the whole catalog).
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        let url = format!("{}/{key}", self.base);
        let mut req = self.agent.get(&url);
        if let Some(token) = &self.token {
            req = req.header("authorization", &format!("Bearer {token}"));
        }
        let mut resp = req
            .call()
            .map_err(|e| Error::Failed(format!("GET {url}: {e}")))?;
        let status = resp.status();
        if status == ureq::http::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(Error::Failed(format!("GET {url}: HTTP {status}")));
        }
        // Bound the body — `index.json` is small, blobs are tarballs.
        let limit = if key == "index.json" {
            MAX_INDEX
        } else {
            DEFAULT_MAX_DOWNLOAD
        };
        resp.body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map(Some)
            .map_err(|e| Error::Failed(format!("read {url}: {e}")))
    }

    /// `PUT <key>` — the body is streamed (`AsSendBody`), so a `.tgz`
    /// publish never buffers the whole tarball in memory.
    fn put(&self, key: &str, body: impl ureq::AsSendBody) -> Result<(), Error> {
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
    // The version is validated before this is called, but keep the temp
    // filename single-component regardless — records/manifests can be
    // hand-edited.
    let safe: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!(
        "tamias-pub-{}-{nanos}-{seq}-{safe}.tgz",
        std::process::id()
    ))
}

impl Registry for HttpRegistry {
    fn entries(&self) -> Result<Vec<RegistryEntry>, Error> {
        // A cleartext index fetch is unauthenticated and modifiable in
        // transit — the payload digests it records are only as
        // trustworthy as this GET (signatures mitigate only when
        // `allowed_signers` is configured). Warn per call: installs
        // aren't hot loops.
        if self.base.starts_with("http://") {
            eprintln!(
                "warning: fetching registry index {}/index.json over plain http — \
                 the index is unauthenticated and modifiable in transit",
                self.base
            );
        }
        // Only a genuine 404 (no index yet — first publish) yields an
        // empty list; any other read failure propagates so `publish`
        // never PUTs an index built from a failed read.
        match self.get("index.json")? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| Error::Failed(format!("parse index: {e}"))),
            None => Ok(Vec::new()),
        }
    }

    fn publish(
        &self,
        payload_root: &Path,
        manifest: &Manifest,
        signature: Option<&str>,
    ) -> Result<PublishOutcome, Error> {
        // The manifest may not have been validated upstream — every field
        // embedded in the blob key/URL must be a single safe component.
        if !is_valid_name(&manifest.name) {
            return Err(Error::Failed(format!(
                "unsafe package name '{}'",
                manifest.name
            )));
        }
        if let Some(scope) = &manifest.scope {
            if !is_valid_name(scope) {
                return Err(Error::Failed(format!("unsafe scope '{scope}'")));
            }
        }
        let scoped = match &manifest.scope {
            Some(scope) => format!("@{scope}/{}", manifest.name),
            None => manifest.name.clone(),
        };
        let version = manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_string());
        if !is_valid_version(&version) {
            return Err(Error::Failed(format!(
                "unsafe version '{version}' — expected [A-Za-z0-9][A-Za-z0-9._+-]* with no '..'"
            )));
        }
        let rel = format!("{scoped}/{version}.tgz");
        let tmp = publish_tmp_path(&version);
        pack_tgz(payload_root, &tmp)?;
        // Stream the tarball — `File`'s AsSendBody sets Content-Length
        // from metadata, so the whole .tgz never lands in memory. Reject
        // oversized payloads before the PUT: the configured
        // `limits.max_upload` client-side, the server's own cap
        // authoritatively.
        let file =
            std::fs::File::open(&tmp).map_err(Error::io(format!("open {}", tmp.display())))?;
        let size = file
            .metadata()
            .map_err(Error::io(format!("stat {}", tmp.display())))?
            .len();
        if size > self.max_upload {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Failed(format!(
                "payload tarball {} is {size} bytes — exceeds the {}-byte upload limit",
                tmp.display(),
                self.max_upload
            )));
        }
        self.put(&rel, file)?;
        let _ = std::fs::remove_file(&tmp);

        let entry = RegistryEntry {
            name: manifest.name.clone(),
            scope: manifest.scope.clone(),
            version: manifest.version.clone(),
            kind: manifest.kind.clone(),
            digest: dir_digest(payload_root)?,
            signature: signature.map(str::to_string),
            publisher: None,
            file: rel,
        };
        let mut entries = self.entries()?;
        entries.retain(|e| {
            !(e.name == entry.name && e.scope == entry.scope && e.version == entry.version)
        });
        entries.push(entry.clone());
        entries
            .sort_by(|a, b| (&a.scope, &a.name, &a.version).cmp(&(&b.scope, &b.name, &b.version)));
        // Retention (P-024) — index-entry prune only; pruned `.tgz`
        // blobs stay in the store.
        let pruned = self
            .retention
            .map(|keep| {
                apply_retention(
                    &mut entries,
                    usize::try_from(keep).unwrap_or(usize::MAX),
                    &entry,
                )
            })
            .unwrap_or_default();
        let index = serde_json::to_vec_pretty(&entries)
            .map_err(|e| Error::Failed(format!("serialize index: {e}")))?;
        self.put("index.json", &index)?;
        Ok(PublishOutcome { entry, pruned })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Serializes tests that mutate process env — `cargo test` runs
    /// tests on threads in one process, so mutations must be
    /// serialized *and* restored even on panic.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard for [`ENV`]: restores the process environment and
    /// working directory on drop — panic-safe. Poison is ignored: an
    /// unwinding sibling already restored everything via its `Drop`.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        cwd: std::path::PathBuf,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
            let env = std::env::vars_os().collect();
            let cwd = std::env::current_dir().unwrap();
            Self {
                _lock: lock,
                env,
                cwd,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, _) in std::env::vars_os() {
                if !self.env.iter().any(|(sk, _)| *sk == k) {
                    std::env::remove_var(&k);
                }
            }
            for (k, v) in &self.env {
                std::env::set_var(k, v);
            }
            let _ = std::env::set_current_dir(&self.cwd);
        }
    }

    #[test]
    fn entries_without_server_errors_and_env_token() {
        // A dead registry is a transport failure, not an empty index.
        let registry = HttpRegistry::new("http://127.0.0.1:1");
        assert!(registry.entries().is_err());
        assert!(HttpRegistry::from_env("http://127.0.0.1:1")
            .with_token("t")
            .entries()
            .is_err());
    }

    #[test]
    fn publish_without_server_errors() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        let registry = HttpRegistry::new("http://127.0.0.1:1");
        assert!(registry.publish(src.path(), &manifest, None).is_err());
    }

    #[test]
    fn publish_rejects_unsafe_manifest_fields() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let registry = HttpRegistry::new("http://127.0.0.1:1");
        for bad in ["../../evil", "a/b", "..", "", "-x"] {
            let manifest = Manifest {
                name: "x".into(),
                version: Some(bad.into()),
                ..Default::default()
            };
            assert!(
                registry.publish(src.path(), &manifest, None).is_err(),
                "{bad}"
            );
        }
        let manifest = Manifest {
            name: "../x".into(),
            version: Some("1.0.0".into()),
            ..Default::default()
        };
        assert!(registry.publish(src.path(), &manifest, None).is_err());
    }

    #[test]
    fn publish_enforces_max_upload_before_put() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        // A 1-byte cap fails on the packed tarball before any network
        // I/O — the dead-server URL proves no PUT was attempted.
        let registry = HttpRegistry::new("http://127.0.0.1:1").with_max_upload(1);
        let err = registry.publish(src.path(), &manifest, None).unwrap_err();
        assert!(err.to_string().contains("upload limit"), "{err}");
    }

    #[test]
    fn publish_tmp_path_stays_single_component() {
        let tmp = publish_tmp_path("../../evil");
        assert_eq!(
            tmp.components().count(),
            std::env::temp_dir().components().count() + 1
        );
        let name = tmp.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.contains("tamias-pub-"), "{name}");
        assert!(!name.contains('/'), "{name}");
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
        // Fresh registry: index.json is absent → 404 → legitimately empty.
        assert!(registry.entries().unwrap().is_empty());
        assert_eq!(
            registry
                .publish(src.path(), &manifest, None)
                .unwrap()
                .label(),
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
        // A 500 on GET index.json is a failure, not an empty registry.
        assert!(registry.entries().is_err());
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        assert!(registry.publish(src.path(), &manifest, None).is_err());
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
            registry
                .publish(src.path(), &manifest, None)
                .unwrap()
                .label(),
            "@acme/s@1.0.0"
        );
        // re-publish → the existing index entry is replaced
        assert_eq!(
            registry
                .publish(src.path(), &manifest, None)
                .unwrap()
                .label(),
            "@acme/s@1.0.0"
        );
        assert_eq!(registry.entries().unwrap().len(), 1);
    }

    #[test]
    fn from_env_token_header_is_sent() {
        let _g = EnvGuard::new();
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
        assert!(registry.publish(src.path(), &manifest, None).is_err());
    }

    /// A store that accepts PUTs but fails GET index.json with a 500 —
    /// the index-wipe scenario: publish must fail and must NOT PUT a
    /// rebuilt index on top of the failed read.
    #[test]
    fn publish_failed_index_read_never_writes_index() {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_srv = seen.clone();
        std::thread::spawn(move || {
            while let Ok((mut s, _)) = listener.accept() {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let header_end = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break buf.len();
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let first = head.lines().next().unwrap_or_default().to_string();
                // Drain the request body so the response isn't cut short.
                let content_length: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse().ok())
                    })
                    .unwrap_or(0);
                let mut left = content_length.saturating_sub(buf.len() - header_end);
                while left > 0 {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    left = left.saturating_sub(n);
                }
                seen_srv.lock().unwrap().push(first.clone());
                let body = if first.starts_with("GET") {
                    b"HTTP/1.1 500 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
                };
                let _ = s.write_all(body);
                let _ = s.flush();
            }
        });

        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        let registry = HttpRegistry::new(format!("http://127.0.0.1:{port}"));
        assert!(registry.publish(src.path(), &manifest, None).is_err());
        let reqs = seen.lock().unwrap();
        assert!(
            reqs.iter().any(|r| r.starts_with("GET /index.json")),
            "{reqs:?}"
        );
        assert!(
            !reqs.iter().any(|r| r.starts_with("PUT /index.json")),
            "failed index read must not be overwritten: {reqs:?}"
        );
    }
}
