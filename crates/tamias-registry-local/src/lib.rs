#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-registry-local — a **directory** registry: published packages as
//! `.tgz` blobs plus an `index.json`. It can also be `serve`d over a tiny
//! HTTP surface (`GET /<key>`, `PUT /<key>`) so a remote client (and the
//! Cloudflare Worker, which speaks the same protocol) can publish/read.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::fs;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tamias_core::{
    dir_digest, is_valid_name, is_valid_version, pack_tgz_atomic, write_atomic, Error, Manifest,
};
use tamias_registry::{Registry, RegistryEntry};

/// A dir-backed registry.
#[derive(Clone, Debug)]
pub struct LocalRegistry {
    root: PathBuf,
    /// Upload cap enforced in [`Registry::publish`] — `Some` rejects a
    /// packed `.tgz` larger than the configured `limits.max_upload`.
    /// `None` (the default) leaves direct dir writes uncapped; the
    /// `registry serve` HTTP surface enforces its own cap regardless.
    max_upload: Option<u64>,
}

impl LocalRegistry {
    /// A registry under `root` (created lazily on first publish).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_upload: None,
        }
    }

    /// Cap publishes at `bytes` — the configured `max_upload`.
    #[must_use]
    pub fn with_max_upload(mut self, bytes: u64) -> Self {
        self.max_upload = Some(bytes);
        self
    }

    /// The root dir.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }
}

impl Registry for LocalRegistry {
    fn entries(&self) -> Result<Vec<RegistryEntry>, Error> {
        let path = self.index_path();
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let text =
            fs::read_to_string(&path).map_err(Error::io(format!("read {}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))
    }

    fn publish(&self, payload_root: &Path, manifest: &Manifest) -> Result<RegistryEntry, Error> {
        // The manifest may not have been validated upstream — every field
        // embedded in the blob path must be a single safe component.
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
        let out = self.root.join(&rel);
        let parent = out.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(Error::io(format!("create {}", parent.display())))?;
        // Blob lands atomically — a crash mid-pack leaves a stray
        // `*.tmp`, never a truncated `.tgz` at the canonical path.
        pack_tgz_atomic(payload_root, &out)?;
        // The size probe runs post-pack/pre-index: an over-limit blob is
        // removed before it can be served or referenced.
        if let Some(max) = self.max_upload {
            let size = fs::metadata(&out)
                .map_err(Error::io(format!("stat {}", out.display())))?
                .len();
            if size > max {
                let _ = fs::remove_file(&out);
                return Err(Error::Failed(format!(
                    "payload tarball {} is {size} bytes — exceeds the {max}-byte upload limit",
                    out.display()
                )));
            }
        }
        let entry = RegistryEntry {
            name: manifest.name.clone(),
            scope: manifest.scope.clone(),
            version: manifest.version.clone(),
            kind: manifest.kind.clone(),
            digest: dir_digest(payload_root)?,
            file: rel,
        };
        // The index read-modify-write is serialized across processes —
        // concurrent publishes can't lose each other's entries.
        fs::create_dir_all(&self.root)
            .map_err(Error::io(format!("create {}", self.root.display())))?;
        let _lock = IndexLock::acquire(&self.root)?;
        let mut entries = self.entries()?;
        entries.retain(|e| {
            !(e.name == entry.name && e.scope == entry.scope && e.version == entry.version)
        });
        entries.push(entry.clone());
        entries
            .sort_by(|a, b| (&a.scope, &a.name, &a.version).cmp(&(&b.scope, &b.name, &b.version)));
        let text = serde_json::to_string_pretty(&entries)
            .map_err(|e| Error::Failed(format!("serialize index: {e}")))?;
        write_atomic(&self.index_path(), text.as_bytes())?;
        Ok(entry)
    }
}

/// `index.lock` — an atomic-create (`create_new`) lockfile serializing
/// the registry index read-modify-write. A lock older than
/// [`STALE_AFTER`] is treated as abandoned (holder crashed) and stolen;
/// `create_new` still guarantees a single winner of the steal race.
/// Residual: a holder legitimately keeping the lock past
/// [`STALE_AFTER`] (a multi-minute index update) could have its lock
/// stolen — index updates here are sub-second, so this is theoretical.
struct IndexLock {
    path: PathBuf,
}

/// A lockfile untouched for this long is considered stale.
const STALE_AFTER: Duration = Duration::from_secs(30);
/// How long `acquire` waits for a live holder before giving up.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

impl IndexLock {
    /// Take `root/index.lock`, waiting out live holders.
    ///
    /// # Errors
    /// Fails on I/O errors or when the lock stays held past
    /// [`LOCK_TIMEOUT`].
    fn acquire(root: &Path) -> Result<Self, Error> {
        let path = root.join("index.lock");
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(Error::Failed(format!(
                            "registry index locked ({})",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(Error::io(format!("lock {}", path.display()))(e)),
            }
        }
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Whether `path`'s mtime is older than [`STALE_AFTER`] — or unreadable,
/// in which case removing it is harmless (the create race decides).
fn lock_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().is_ok_and(|age| age > STALE_AFTER))
        .unwrap_or(true)
}

/// Default PUT cap — mirrors `tamias-config`'s `DEFAULT_MAX_UPLOAD`
/// (512 MiB); bigger blobs can't be installed anyway.
const MAX_PUT: u64 = 512 * 1024 * 1024;

/// Per-read timeout on a connection — an idle client can't stall the
/// single-threaded accept loop (each `read` gets this long).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Env var that turns on bearer auth for the dev server — mirrors the
/// client's `TAMIAS_REGISTRY_TOKEN`. Unset = open (dev default).
const SERVE_TOKEN_ENV: &str = "TAMIAS_REGISTRY_SERVE_TOKEN";

/// Serve `root` over a tiny blocking HTTP surface until the process stops.
/// `GET /<key>` reads a blob; `PUT /<key>` writes one. The registry root is
/// the blob namespace (paths sanitized — no traversal).
///
/// **Dev surface, not a real registry.** Anyone who can reach the port can
/// read and overwrite every blob — including `index.json` (the `PUT /`
/// sanitize maps the root to it, and `HttpRegistry` publishes through this
/// protocol, so it stays allowed). Bind loopback, or set
/// `TAMIAS_REGISTRY_SERVE_TOKEN` to require `Authorization: Bearer <token>`.
///
/// # Errors
/// Propagates the bind failure; per-request errors are logged, not fatal.
pub fn serve_http(addr: &str, root: &Path) -> Result<(), Error> {
    serve_http_with_limits(addr, root, MAX_PUT)
}

/// [`serve_http`] with an explicit upload cap — the configured
/// `limits.max_upload` (flag > config > this default).
///
/// # Errors
/// Propagates the bind failure; per-request errors are logged, not fatal.
pub fn serve_http_with_limits(addr: &str, root: &Path, max_upload: u64) -> Result<(), Error> {
    let token = std::env::var(SERVE_TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty());
    serve_http_token(addr, root, token.as_deref(), max_upload)
}

/// [`serve_http_with_limits`] with an explicit bearer token — `Some`
/// requires `Authorization: Bearer <token>` on every request. Split out
/// so tests don't race on the process env.
fn serve_http_token(
    addr: &str,
    root: &Path,
    token: Option<&str>,
    max_upload: u64,
) -> Result<(), Error> {
    let listener = std::net::TcpListener::bind(addr).map_err(Error::io(format!("bind {addr}")))?;
    fs::create_dir_all(root).map_err(Error::io(format!("create {}", root.display())))?;
    eprintln!(
        "tamias registry serving {} at http://{addr}/ — dev server: \
         unauthenticated blob store (set {SERVE_TOKEN_ENV} for bearer auth)",
        root.display()
    );
    // `--addr 0.0.0.0` (or any non-loopback bind) exposes the writable
    // store to the network — say so loudly.
    let exposed = addr
        .to_socket_addrs()
        .map(|mut addrs| addrs.any(|a| !a.ip().is_loopback()))
        .unwrap_or(false);
    if exposed {
        eprintln!(
            "warning: registry is bound to a non-loopback address ({addr}) — \
             anyone who can reach it can read/overwrite all blobs"
        );
    }
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(e) = handle(&mut stream, root, token, max_upload) {
                    eprintln!("tamias registry: {e}");
                }
            }
            Err(e) => eprintln!("tamias registry: accept: {e}"),
        }
    }
    Ok(())
}

fn handle(
    stream: &mut std::net::TcpStream,
    root: &Path,
    token: Option<&str>,
    max_upload: u64,
) -> Result<(), Error> {
    use std::io::{BufRead, BufReader, Read};

    // Bound every read on this connection — headers AND body — so an
    // idle or slow-loris client can't park the accept loop.
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(Error::io("set read timeout"))?;
    let clone = stream.try_clone().map_err(Error::io("clone stream"))?;
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(Error::io("read request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0u64;
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(Error::io("read header"))?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        // Match the header name case-insensitively but keep the VALUE
        // verbatim — bearer tokens are case-sensitive.
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("authorization:") {
            authorization = line["authorization:".len()..].trim().to_string();
        }
    }
    // Optional bearer auth — `Some(token)` means the operator opted in.
    // `get(..)` slicing: a hostile non-ASCII header can't panic the loop.
    if let Some(token) = token {
        let ok = authorization
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer "))
            && authorization.get(7..).is_some_and(|t| t == token);
        if !ok {
            return respond(stream, 401, b"unauthorized");
        }
    }
    let Some(key) = sanitize_key(&raw_path) else {
        return respond(stream, 400, b"bad path");
    };
    match method.as_str() {
        "GET" => match fs::read(root.join(&key)) {
            Ok(bytes) => respond(stream, 200, &bytes),
            Err(_) => respond(stream, 404, b"not found"),
        },
        "PUT" => {
            // Reject the declared length BEFORE allocating — a huge
            // Content-Length must not become a huge vec.
            if content_length > max_upload {
                return respond(stream, 413, b"payload too large");
            }
            let mut body = vec![0u8; content_length as usize];
            reader
                .read_exact(&mut body)
                .map_err(Error::io("read body"))?;
            let path = root.join(&key);
            let parent = path.parent().unwrap_or(Path::new("."));
            fs::create_dir_all(parent).map_err(Error::io("create blob dir"))?;
            fs::write(&path, &body).map_err(Error::io(format!("write {}", path.display())))?;
            respond(stream, 200, b"ok")
        }
        _ => respond(stream, 405, b"method not allowed"),
    }
}

/// A request path → a safe relative key (no traversal; `/` → `index.json`).
fn sanitize_key(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.split('?').next().unwrap_or(raw).trim_start_matches('/');
    let mut rel = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            std::path::Component::Normal(p) => rel.push(p),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    if rel.as_os_str().is_empty() {
        rel.push("index.json");
    }
    Some(rel)
}

fn respond(stream: &mut std::net::TcpStream, code: u16, body: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .map_err(Error::io("write head"))?;
    stream.write_all(body).map_err(Error::io("write body"))?;
    stream.flush().map_err(Error::io("flush"))?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn publish_and_search() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("tamias.yaml"),
            "name: ci-gate\nversion: 0.2.0\n",
        )
        .unwrap();
        std::fs::write(src.path().join("main.yaml"), "x").unwrap();
        let reg_dir = tempfile::tempdir().unwrap();
        let registry = LocalRegistry::new(reg_dir.path());
        let manifest = Manifest::load(src.path()).unwrap();
        let entry = registry.publish(src.path(), &manifest).unwrap();
        assert_eq!(entry.label(), "ci-gate@0.2.0");
        assert!(reg_dir.path().join("ci-gate/0.2.0.tgz").is_file());
        assert_eq!(registry.search("gate").unwrap().len(), 1);
    }

    #[test]
    fn entries_publish_twice_and_label() {
        let reg_dir = tempfile::tempdir().unwrap();
        let registry = LocalRegistry::new(reg_dir.path());
        assert!(registry.entries().unwrap().is_empty());
        assert!(registry.search("x").unwrap().is_empty());
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: ci\nversion: 0.2.0\n").unwrap();
        std::fs::write(src.path().join("main.yaml"), "x").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        assert_eq!(
            registry.publish(src.path(), &manifest).unwrap().label(),
            "ci@0.2.0"
        );
        let _ = registry.publish(src.path(), &manifest).unwrap();
        assert_eq!(registry.entries().unwrap().len(), 1);
        assert!(registry.root().is_dir());
    }

    fn raw(addr: &str, req: &[u8]) -> String {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.write_all(req).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).to_string()
    }

    /// A loopback addr nothing else is using — bind :0, read the port,
    /// release. Tests run in parallel threads; shared fixed ports made
    /// the second binder fail and its client see `ConnectionRefused`.
    fn free_addr() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        addr
    }

    /// Wait until the spawned server accepts — a fixed sleep raced the
    /// bind under nextest load.
    fn wait_ready(addr: &str) {
        for _ in 0..100 {
            if std::net::TcpStream::connect(addr).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("server on {addr} never came up");
    }

    #[test]
    fn serve_http_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path().to_path_buf();
        let addr = free_addr();
        std::thread::spawn({
            let addr = addr.clone();
            move || {
                let _ = serve_http(&addr, &rp);
            }
        });
        wait_ready(&addr);
        let put = raw(
            &addr,
            b"PUT /index.json HTTP/1.1\r\nContent-Length: 3\r\n\r\n[1]",
        );
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        let get = raw(&addr, b"GET /index.json HTTP/1.1\r\n\r\n");
        assert!(get.contains("[1]"), "{get}");
        let nf = raw(&addr, b"GET /nope HTTP/1.1\r\n\r\n");
        assert!(nf.contains("404"), "{nf}");
        let bad = raw(&addr, b"GET /../x HTTP/1.1\r\n\r\n");
        assert!(bad.contains("400"), "{bad}");
    }

    #[test]
    fn serve_http_rejects_oversized_content_length() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path().to_path_buf();
        let addr = free_addr();
        std::thread::spawn({
            let addr = addr.clone();
            move || {
                let _ = serve_http_token(&addr, &rp, None, MAX_PUT);
            }
        });
        wait_ready(&addr);
        // Declared length over MAX_PUT → 413 before any allocation; the
        // connection closes without needing the (absent) body.
        let res = raw(
            &addr,
            format!(
                "PUT /big HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                MAX_PUT + 1
            )
            .as_bytes(),
        );
        assert!(res.contains("413"), "{res}");
        assert!(!root.path().join("big").exists());
    }

    #[test]
    fn serve_http_custom_upload_limit() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path().to_path_buf();
        let addr = free_addr();
        std::thread::spawn({
            let addr = addr.clone();
            move || {
                let _ = serve_http_token(&addr, &rp, None, 4);
            }
        });
        wait_ready(&addr);
        // Over the configured cap → 413 before the body is read.
        let big = raw(
            &addr,
            b"PUT /big HTTP/1.1\r\nContent-Length: 5\r\n\r\n12345",
        );
        assert!(big.contains("413"), "{big}");
        assert!(!root.path().join("big").exists());
        // At the cap → accepted.
        let ok = raw(
            &addr,
            b"PUT /small HTTP/1.1\r\nContent-Length: 4\r\n\r\n1234",
        );
        assert!(ok.contains("200"), "{ok}");
        assert_eq!(std::fs::read(root.path().join("small")).unwrap(), b"1234");
    }

    #[test]
    fn publish_enforces_max_upload() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("tamias.yaml"),
            "name: capped\nversion: 1.0.0\n",
        )
        .unwrap();
        std::fs::write(src.path().join("blob.bin"), vec![0u8; 4096]).unwrap();
        let reg_dir = tempfile::tempdir().unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        // A 1-byte cap rejects the packed .tgz and leaves no blob behind.
        let registry = LocalRegistry::new(reg_dir.path()).with_max_upload(1);
        let err = registry.publish(src.path(), &manifest).unwrap_err();
        assert!(err.to_string().contains("upload limit"), "{err}");
        assert!(!reg_dir.path().join("capped/1.0.0.tgz").exists());
        assert!(registry.entries().unwrap().is_empty());
        // Generous cap → publishes fine.
        let registry = LocalRegistry::new(reg_dir.path()).with_max_upload(1 << 20);
        assert!(registry.publish(src.path(), &manifest).is_ok());
        // Default (no cap) → also fine.
        let reg2 = tempfile::tempdir().unwrap();
        assert!(LocalRegistry::new(reg2.path())
            .publish(src.path(), &manifest)
            .is_ok());
    }

    #[test]
    fn serve_http_bearer_token() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path().to_path_buf();
        let addr = free_addr();
        std::thread::spawn({
            let addr = addr.clone();
            move || {
                let _ = serve_http_token(&addr, &rp, Some("sekret"), MAX_PUT);
            }
        });
        wait_ready(&addr);
        let noauth = raw(&addr, b"GET /index.json HTTP/1.1\r\n\r\n");
        assert!(noauth.contains("401"), "{noauth}");
        let wrong = raw(
            &addr,
            b"GET /index.json HTTP/1.1\r\nAuthorization: Bearer wrong\r\n\r\n",
        );
        assert!(wrong.contains("401"), "{wrong}");
        let put = raw(
            &addr,
            b"PUT /index.json HTTP/1.1\r\nAuthorization: Bearer sekret\r\nContent-Length: 2\r\n\r\n[]",
        );
        assert!(put.contains("200"), "{put}");
        let get = raw(
            &addr,
            b"GET /index.json HTTP/1.1\r\nauthorization: bearer sekret\r\n\r\n",
        );
        assert!(get.contains("200"), "{get}");
    }

    #[test]
    fn publish_rejects_unsafe_manifest_fields() {
        let reg_dir = tempfile::tempdir().unwrap();
        let registry = LocalRegistry::new(reg_dir.path());
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        for bad in ["../../evil", "a/b", "..", "", "-x"] {
            let manifest = Manifest {
                name: "x".into(),
                version: Some(bad.into()),
                ..Default::default()
            };
            assert!(registry.publish(src.path(), &manifest).is_err(), "{bad}");
        }
        // an unsafe/empty name must not escape via an absolute rel path
        for name in ["../x", "", "a/b"] {
            let manifest = Manifest {
                name: name.into(),
                version: Some("1.0.0".into()),
                ..Default::default()
            };
            assert!(registry.publish(src.path(), &manifest).is_err(), "{name}");
        }
        // nothing was published — no blob, no index
        assert!(registry.entries().unwrap().is_empty());
        assert!(!reg_dir.path().join("index.json").exists());
    }

    #[test]
    fn bad_index_errors() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("index.json"), "not json").unwrap();
        assert!(LocalRegistry::new(root.path()).entries().is_err());
    }

    #[test]
    fn publish_leaves_no_tmp_or_lock_litter() {
        let reg_dir = tempfile::tempdir().unwrap();
        let registry = LocalRegistry::new(reg_dir.path());
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        registry.publish(src.path(), &manifest).unwrap();
        assert!(!reg_dir.path().join("index.lock").exists());
        // no *.tmp siblings anywhere in the registry
        let mut stack = vec![reg_dir.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    assert!(
                        !p.file_name().unwrap().to_string_lossy().ends_with(".tmp"),
                        "tmp litter: {}",
                        p.display()
                    );
                }
            }
        }
    }

    #[test]
    fn concurrent_publishes_keep_all_entries() {
        let reg_dir = tempfile::tempdir().unwrap();
        let registry = LocalRegistry::new(reg_dir.path());
        let mut handles = Vec::new();
        for i in 0..4 {
            let reg = registry.clone();
            handles.push(std::thread::spawn(move || {
                let src = tempfile::tempdir().unwrap();
                std::fs::write(
                    src.path().join("tamias.yaml"),
                    format!("name: pkg{i}\nversion: 1.0.0\n"),
                )
                .unwrap();
                std::fs::write(src.path().join("a.txt"), "a").unwrap();
                let manifest = Manifest::load(src.path()).unwrap();
                reg.publish(src.path(), &manifest).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let entries = registry.entries().unwrap();
        assert_eq!(entries.len(), 4, "{entries:?}");
        assert!(!reg_dir.path().join("index.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn publish_and_read_io_errors() {
        use std::os::unix::fs::PermissionsExt;
        // publish into an unwritable root
        let ro = tempfile::tempdir().unwrap();
        let rodir = ro.path().join("d");
        std::fs::create_dir_all(&rodir).unwrap();
        std::fs::set_permissions(&rodir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let registry = LocalRegistry::new(rodir.join("reg"));
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\nversion: 1.0.0\n").unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        assert!(registry.publish(src.path(), &manifest).is_err());
        std::fs::set_permissions(&rodir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // an unreadable index.json → read error
        let reg = tempfile::tempdir().unwrap();
        std::fs::write(reg.path().join("index.json"), "[]").unwrap();
        std::fs::set_permissions(
            reg.path().join("index.json"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        assert!(LocalRegistry::new(reg.path()).entries().is_err());
        std::fs::set_permissions(
            reg.path().join("index.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }

    #[test]
    fn serve_http_method_and_query() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path().to_path_buf();
        let addr = free_addr();
        std::thread::spawn({
            let addr = addr.clone();
            move || {
                let _ = serve_http(&addr, &rp);
            }
        });
        wait_ready(&addr);
        let del = raw(&addr, b"DELETE /x HTTP/1.1\r\n\r\n");
        assert!(del.contains("405"), "{del}");
        let put = raw(&addr, b"PUT /k?v=1 HTTP/1.1\r\nContent-Length: 1\r\n\r\nz");
        assert!(put.contains("200"), "{put}");
        let get = raw(&addr, b"GET /k?v=1 HTTP/1.1\r\n\r\n");
        assert!(get.contains('z'), "{get}");
    }

    #[test]
    fn search_scoped_entry() {
        let reg = tempfile::tempdir().unwrap();
        let registry = LocalRegistry::new(reg.path());
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("tamias.yaml"),
            "name: s\nscope: acme\nversion: 1.0.0\n",
        )
        .unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        registry.publish(src.path(), &manifest).unwrap();
        assert_eq!(registry.search("acme").unwrap().len(), 1);
        assert_eq!(registry.search("s").unwrap().len(), 1);
    }

    #[test]
    fn sanitize_default_curdir_arms() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path().to_path_buf();
        let addr = free_addr();
        std::thread::spawn({
            let addr = addr.clone();
            move || {
                let _ = serve_http(&addr, &rp);
            }
        });
        wait_ready(&addr);
        // empty path → index.json (absent → 404)
        let root_req = raw(&addr, b"GET / HTTP/1.1\r\n\r\n");
        assert!(root_req.contains("404"), "{root_req}");
        // a "./" component is skipped
        let cur = raw(&addr, b"PUT /./k HTTP/1.1\r\nContent-Length: 1\r\n\r\nz");
        assert!(cur.contains("200"), "{cur}");
        let got = raw(&addr, b"GET /./k HTTP/1.1\r\n\r\n");
        assert!(got.contains('z'), "{got}");
    }

    #[test]
    fn respond_unknown_code_uses_error_reason() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            respond(&mut s, 999, b"x").unwrap();
        });
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        let mut buf = Vec::new();
        use std::io::Read as _;
        let _ = c.read_to_end(&mut buf);
        h.join().unwrap();
        let text = String::from_utf8_lossy(&buf).to_string();
        assert!(text.contains("999 Error"), "{text}");
    }

    #[test]
    fn serve_io_error_arms() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path().to_path_buf();
        let addr = free_addr();
        std::thread::spawn({
            let addr = addr.clone();
            move || {
                let _ = serve_http(&addr, &rp);
            }
        });
        wait_ready(&addr);
        use std::io::Write;
        // short body vs Content-Length → read_exact fails
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        let _ = s.write_all(b"PUT /k HTTP/1.1\r\nContent-Length: 10\r\n\r\nab");
        drop(s);
        // connect + immediate close → server write → broken pipe
        let _ = std::net::TcpStream::connect(&addr);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[cfg(unix)]
    #[test]
    fn serve_http_io_errors() {
        use std::os::unix::fs::PermissionsExt;
        // bind failure → error
        assert!(serve_http("not-an-addr", Path::new("/tmp")).is_err());
        // unwritable root dir → create_dir_all(root) fails
        let ro = tempfile::tempdir().unwrap();
        let rodir = ro.path().join("d");
        std::fs::create_dir_all(&rodir).unwrap();
        std::fs::set_permissions(&rodir, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(serve_http(&free_addr(), &rodir.join("reg")).is_err());
        std::fs::set_permissions(&rodir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
