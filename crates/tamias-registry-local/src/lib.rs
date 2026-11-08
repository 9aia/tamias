#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-registry-local — a **directory** registry: published packages as
//! `.tgz` blobs plus an `index.json`. It can also be `serve`d over a tiny
//! HTTP surface (`GET /<key>`, `PUT /<key>`) so a remote client (and the
//! Cloudflare Worker, which speaks the same protocol) can publish/read.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::fs;
use std::path::{Path, PathBuf};

use tamias_core::{dir_digest, pack_tgz, Error, Manifest};
use tamias_registry::{Registry, RegistryEntry};

/// A dir-backed registry.
#[derive(Clone, Debug)]
pub struct LocalRegistry {
    root: PathBuf,
}

impl LocalRegistry {
    /// A registry under `root` (created lazily on first publish).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
        let scoped = match &manifest.scope {
            Some(scope) => format!("@{scope}/{}", manifest.name),
            None => manifest.name.clone(),
        };
        let version = manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_string());
        let rel = format!("{scoped}/{version}.tgz");
        let out = self.root.join(&rel);
        let parent = out.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(Error::io(format!("create {}", parent.display())))?;
        pack_tgz(payload_root, &out)?;
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
        fs::create_dir_all(&self.root)
            .map_err(Error::io(format!("create {}", self.root.display())))?;
        let text = serde_json::to_string_pretty(&entries)
            .map_err(|e| Error::Failed(format!("serialize index: {e}")))?;
        fs::write(self.index_path(), text)
            .map_err(Error::io(format!("write {}", self.index_path().display())))?;
        Ok(entry)
    }
}

/// Serve `root` over a tiny blocking HTTP surface until the process stops.
/// `GET /<key>` reads a blob; `PUT /<key>` writes one. The registry root is
/// the blob namespace (paths sanitized — no traversal).
///
/// # Errors
/// Propagates the bind failure; per-request errors are logged, not fatal.
pub fn serve_http(addr: &str, root: &Path) -> Result<(), Error> {
    let listener = std::net::TcpListener::bind(addr).map_err(Error::io(format!("bind {addr}")))?;
    fs::create_dir_all(root).map_err(Error::io(format!("create {}", root.display())))?;
    eprintln!(
        "tamias registry serving {} at http://{addr}/",
        root.display()
    );
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(e) = handle(&mut stream, root) {
                    eprintln!("tamias registry: {e}");
                }
            }
            Err(e) => eprintln!("tamias registry: accept: {e}"),
        }
    }
    Ok(())
}

fn handle(stream: &mut std::net::TcpStream, root: &Path) -> Result<(), Error> {
    use std::io::{BufRead, BufReader, Read};

    let clone = stream.try_clone().map_err(Error::io("clone stream"))?;
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(Error::io("read request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(Error::io("read header"))?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
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
            let mut body = vec![0u8; content_length];
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
        404 => "Not Found",
        405 => "Method Not Allowed",
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

    #[test]
    fn serve_http_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path().to_path_buf();
        std::thread::spawn(move || {
            let _ = serve_http("127.0.0.1:8791", &rp);
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        let put = raw(
            "127.0.0.1:8791",
            b"PUT /index.json HTTP/1.1\r\nContent-Length: 3\r\n\r\n[1]",
        );
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        let get = raw("127.0.0.1:8791", b"GET /index.json HTTP/1.1\r\n\r\n");
        assert!(get.contains("[1]"), "{get}");
        let nf = raw("127.0.0.1:8791", b"GET /nope HTTP/1.1\r\n\r\n");
        assert!(nf.contains("404"), "{nf}");
        let bad = raw("127.0.0.1:8791", b"GET /../x HTTP/1.1\r\n\r\n");
        assert!(bad.contains("400"), "{bad}");
    }

    #[test]
    fn bad_index_errors() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("index.json"), "not json").unwrap();
        assert!(LocalRegistry::new(root.path()).entries().is_err());
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
        std::thread::spawn(move || {
            let _ = serve_http("127.0.0.1:8794", &rp);
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        let del = raw("127.0.0.1:8794", b"DELETE /x HTTP/1.1\r\n\r\n");
        assert!(del.contains("405"), "{del}");
        let put = raw(
            "127.0.0.1:8794",
            b"PUT /k?v=1 HTTP/1.1\r\nContent-Length: 1\r\n\r\nz",
        );
        assert!(put.contains("200"), "{put}");
        let get = raw("127.0.0.1:8794", b"GET /k?v=1 HTTP/1.1\r\n\r\n");
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
        std::thread::spawn(move || {
            let _ = serve_http("127.0.0.1:8795", &rp);
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect("127.0.0.1:8795").is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // empty path → index.json (absent → 404)
        let root_req = raw("127.0.0.1:8795", b"GET / HTTP/1.1\r\n\r\n");
        assert!(root_req.contains("404"), "{root_req}");
        // a "./" component is skipped
        let cur = raw(
            "127.0.0.1:8795",
            b"PUT /./k HTTP/1.1\r\nContent-Length: 1\r\n\r\nz",
        );
        assert!(cur.contains("200"), "{cur}");
        let got = raw("127.0.0.1:8795", b"GET /./k HTTP/1.1\r\n\r\n");
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
        std::thread::spawn(move || {
            let _ = serve_http("127.0.0.1:8797", &rp);
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect("127.0.0.1:8797").is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        use std::io::Write;
        // short body vs Content-Length → read_exact fails
        let mut s = std::net::TcpStream::connect("127.0.0.1:8797").unwrap();
        let _ = s.write_all(b"PUT /k HTTP/1.1\r\nContent-Length: 10\r\n\r\nab");
        drop(s);
        // connect + immediate close → server write → broken pipe
        let _ = std::net::TcpStream::connect("127.0.0.1:8797");
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
        assert!(serve_http("127.0.0.1:8798", &rodir.join("reg")).is_err());
        std::fs::set_permissions(&rodir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
