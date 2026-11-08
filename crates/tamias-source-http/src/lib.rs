#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-source-http — the tarball arm of the ref grammar:
//! `http(s)://….tgz|.tar.gz` GETs, verified against the optional
//! `#sha256:` pin, unpacked into the fetch dir. The tarball's sha256 is
//! the content key (`sha256-<hex>`), so a pinned ref + warm cache never
//! touches the network — the air-gapped path.

use std::fs;
use std::path::Path;

use tamias_core::{sha256_hex, untar_gz, Error, SourceRef};
use tamias_source::{Fetched, Resolved, Source};

/// Largest tarball a fetch buffers — 512 MiB.
const MAX_TARBALL: u64 = 512 * 1024 * 1024;

/// [`Source`] over http(s) tarballs.
pub struct HttpSource {
    agent: ureq::Agent,
}

impl HttpSource {
    /// An http source over a default ureq agent.
    #[must_use]
    pub fn new() -> Self {
        Self::with_agent(ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build(),
        ))
    }

    /// An http source over a custom agent (proxies, timeouts, fakes).
    #[must_use]
    pub fn with_agent(agent: ureq::Agent) -> Self {
        Self { agent }
    }
}

impl Default for HttpSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for HttpSource {
    fn resolve(&self, reference: &SourceRef) -> Result<Resolved, Error> {
        let SourceRef::Http(http) = reference else {
            return Err(Error::Failed(
                "HttpSource only resolves http refs".to_string(),
            ));
        };
        Ok(Resolved::Http {
            url: http.url.clone(),
            digest: http.digest.clone(),
        })
    }

    fn fetch(&self, resolved: &Resolved, dest_dir: &Path) -> Result<Fetched, Error> {
        let Resolved::Http { url, digest } = resolved else {
            return Err(Error::Failed(
                "HttpSource only fetches http specs".to_string(),
            ));
        };
        fs::create_dir_all(dest_dir)
            .map_err(Error::io(format!("create {}", dest_dir.display())))?;
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| Error::Failed(format!("GET {url}: {e}")))?;
        if !response.status().is_success() {
            return Err(Error::Failed(format!(
                "GET {url}: HTTP {}",
                response.status()
            )));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_TARBALL)
            .read_to_vec()
            .map_err(|e| Error::Failed(format!("read {url}: {e}")))?;
        let hex = sha256_hex(&bytes);
        if let Some(want) = digest {
            if hex != *want {
                return Err(Error::Failed(format!(
                    "digest mismatch for {url}: sha256:{hex} ≠ pinned sha256:{want}"
                )));
            }
        }
        untar_gz(bytes.as_slice(), dest_dir)?;
        Ok(Fetched {
            dir: dest_dir.to_path_buf(),
            resolved: format!("sha256:{hex}"),
            content_key: format!("sha256-{hex}"),
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;
    use tamias_core::pack_tgz;

    /// Serve `bytes` once on a random port; return the port.
    fn serve_once(bytes: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&bytes);
                let _ = stream.flush();
            }
        });
        port
    }

    #[test]
    fn fetch_and_verify_a_tgz() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.tgz");
        pack_tgz(src.path(), &out).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let port = serve_once(bytes);
        let reference = tamias_core::parse_ref(&format!("http://127.0.0.1:{port}/x.tgz")).unwrap();
        let source = HttpSource::new();
        let resolved = source.resolve(&reference).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let fetched = source.fetch(&resolved, dest.path()).unwrap();
        assert!(dest.path().join("tamias.yaml").exists());
        assert!(fetched.resolved.starts_with("sha256:"));
        assert!(fetched.content_key.starts_with("sha256-"));
    }

    #[test]
    fn resolves_only_http() {
        let reference = tamias_core::parse_ref("./x").unwrap();
        assert!(HttpSource::new().resolve(&reference).is_err());
    }

    #[test]
    fn fetch_http_error() {
        let reference = tamias_core::parse_ref("http://127.0.0.1:1/x.tgz").unwrap();
        let resolved = HttpSource::new().resolve(&reference).unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(HttpSource::new().fetch(&resolved, dest.path()).is_err());
    }

    #[test]
    fn digest_mismatch_errors() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tamias.yaml"), "name: x\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.tgz");
        pack_tgz(src.path(), &out).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let port = serve_once(bytes);
        let hex = "0".repeat(64);
        let reference =
            tamias_core::parse_ref(&format!("http://127.0.0.1:{port}/x.tgz#sha256:{hex}")).unwrap();
        let resolved = HttpSource::new().resolve(&reference).unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(HttpSource::new().fetch(&resolved, dest.path()).is_err());
    }

    #[test]
    fn http_error_status_errors() {
        let port = serve_status(500, b"boom");
        let reference = tamias_core::parse_ref(&format!("http://127.0.0.1:{port}/x.tgz")).unwrap();
        let resolved = HttpSource::new().resolve(&reference).unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(HttpSource::new().fetch(&resolved, dest.path()).is_err());
    }

    fn serve_status(code: u16, body: &[u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_vec();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        port
    }

    #[test]
    fn with_agent_and_wrong_kind() {
        let _ = HttpSource::with_agent(ureq::Agent::new_with_config(
            ureq::Agent::config_builder().build(),
        ));
        let fs = tamias_core::parse_ref("./x").unwrap();
        assert!(HttpSource::new().resolve(&fs).is_err());
        let resolved = tamias_source::Resolved::Fs {
            path: std::path::PathBuf::from("/x"),
        };
        let dest = tempfile::tempdir().unwrap();
        assert!(HttpSource::new().fetch(&resolved, dest.path()).is_err());
    }

    #[test]
    fn default_impl() {
        let _ = HttpSource::default();
    }

    #[test]
    fn fetch_untar_and_body_error_arms() {
        // a 200 with a garbage body → untar_gz fails
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = listener.accept() {
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\njunk",
                );
                let _ = s.flush();
            }
        });
        let r = tamias_core::parse_ref(&format!("http://127.0.0.1:{port}/x.tgz")).unwrap();
        let resolved = HttpSource::new().resolve(&r).unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(HttpSource::new().fetch(&resolved, dest.path()).is_err());
    }
}
