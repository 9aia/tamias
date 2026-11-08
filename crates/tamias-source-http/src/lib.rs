#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source-http — the tarball ref kind + source:
//! `http(s)://….tgz|.tar.gz` GETs, verified against the optional `#sha256:`
//! pin, unpacked into the fetch dir. The tarball's sha256 is the content key
//! (`sha256-<hex>`), so a pinned ref + warm cache never touches the network —
//! the air-gapped path.

use std::any::Any;
use std::fs;
use std::path::Path;

use tamias_core::{sha256_hex, untar_gz, Error};
use tamias_source::{Fetched, Plan, RefKind, Source, Spec};

/// Largest tarball a fetch buffers — 512 MiB.
const MAX_TARBALL: u64 = 512 * 1024 * 1024;

/// The http ref kind — a tarball URL with an optional digest pin.
#[derive(Clone, Copy, Debug, Default)]
pub struct HttpRefKind;

/// A parsed http ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpSpec {
    /// URL minus the `#sha256:` fragment.
    pub url: String,
    /// Expected tarball sha256 hex, when pinned.
    pub digest: Option<String>,
}

impl RefKind for HttpRefKind {
    fn claims(&self, input: &str) -> bool {
        input.starts_with("https://") || input.starts_with("http://")
    }

    /// URLs are case-sensitive — the extension check is deliberately exact.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn parse(&self, input: &str) -> Result<Spec, Error> {
        let (url, fragment) = match input.split_once('#') {
            Some((url, fragment)) => (url, Some(fragment)),
            None => (input, None),
        };
        let digest = match fragment {
            None => None,
            Some(fragment) => {
                let hex = fragment
                    .strip_prefix("sha256:")
                    .or_else(|| fragment.strip_prefix("sha256="))
                    .ok_or_else(|| {
                        Error::Failed(format!(
                            "unsupported fragment '#{fragment}' — expected '#sha256:<hex>'"
                        ))
                    })?;
                if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(Error::Failed(
                        "sha256 digest must be 64 hex chars".to_string(),
                    ));
                }
                Some(hex.to_ascii_lowercase())
            }
        };
        if !(url.ends_with(".tgz") || url.ends_with(".tar.gz")) {
            return Err(Error::Failed(format!(
                "http refs must point at a .tgz/.tar.gz tarball: {url}"
            )));
        }
        Ok(Box::new(HttpSpec {
            url: url.to_string(),
            digest,
        }))
    }
}

/// The http source.
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

/// An http fetch plan — reusable by other sources (e.g. `cf:`).
pub struct HttpPlan {
    url: String,
    digest: Option<String>,
}

impl HttpPlan {
    /// A plan for `url`, optionally digest-pinned.
    #[must_use]
    pub fn new(url: impl Into<String>, digest: Option<String>) -> Self {
        Self {
            url: url.into(),
            digest,
        }
    }
}

impl Plan for HttpPlan {
    fn prefetch_identity(&self) -> Option<(String, String)> {
        self.digest
            .as_ref()
            .map(|d| (format!("sha256-{d}"), format!("sha256:{d}")))
    }
}

impl Source for HttpSource {
    fn accepts(&self, spec: &dyn Any) -> bool {
        spec.is::<HttpSpec>()
    }

    fn plan(&self, spec: &dyn Any) -> Result<Box<dyn Plan>, Error> {
        let HttpSpec { url, digest } = spec
            .downcast_ref::<HttpSpec>()
            .ok_or_else(|| Error::Failed("HttpSource only plans http refs".to_string()))?;
        Ok(Box::new(HttpPlan {
            url: url.clone(),
            digest: digest.clone(),
        }))
    }

    fn fetch(&self, plan: &dyn Plan, dest_dir: &Path) -> Result<Fetched, Error> {
        let HttpPlan { url, digest } = plan
            .downcast_ref::<HttpPlan>()
            .ok_or_else(|| Error::Failed("HttpSource only fetches http plans".to_string()))?;
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
        if let Some(want) = digest.as_deref().filter(|want| hex.as_str() != *want) {
            return Err(Error::Failed(format!(
                "digest mismatch for {url}: sha256:{hex} ≠ pinned sha256:{want}"
            )));
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;
    use tamias_core::pack_tgz;
    use tamias_source::{Refs, Sources};

    struct ForeignPlan;
    impl Plan for ForeignPlan {}

    /// Serve `body` once with `status` on a random port.
    fn serve(body: Vec<u8>, status: &str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let status = status.to_string();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
                let _ = s.flush();
            }
        });
        port
    }

    fn tarball() -> Vec<u8> {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let out = tempfile::tempdir().unwrap();
        let tgz = out.path().join("x.tgz");
        pack_tgz(src.path(), &tgz).unwrap();
        std::fs::read(&tgz).unwrap()
    }

    #[test]
    fn parse_plan_fetch_and_errors() {
        let refs = Refs::new().with(Arc::new(HttpRefKind));
        let sources = Sources::new().with(Arc::new(HttpSource::new()));
        let bytes = tarball();
        let hex = sha256_hex(&bytes);

        // unpinned happy path
        let port = serve(bytes.clone(), "200 OK");
        let spec = refs
            .parse(&format!("http://127.0.0.1:{port}/x.tgz"))
            .unwrap();
        let planned = sources.plan(&*spec).unwrap();
        assert!(planned.prefetch_identity().is_none());
        let dest = tempfile::tempdir().unwrap();
        assert!(planned.fetch(dest.path()).is_ok());
        assert!(dest.path().join("a.txt").is_file());

        // digest-pinned → prefetch identity + verified
        let port = serve(bytes.clone(), "200 OK");
        let spec = refs
            .parse(&format!("http://127.0.0.1:{port}/x.tgz#sha256:{hex}"))
            .unwrap();
        let planned = sources.plan(&*spec).unwrap();
        assert_eq!(
            planned.prefetch_identity().unwrap().0,
            format!("sha256-{hex}")
        );
        let d2 = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d2.path()).is_ok());

        // grammar errors + claims
        assert!(refs.parse("http://x/y.zip").is_err());
        assert!(refs.parse("http://x/y.tgz#nope:1").is_err());
        assert!(refs
            .parse(&format!("http://x/y.tgz#sha256:{}", "z".repeat(64)))
            .is_err());
        assert!(HttpRefKind.claims("https://x/y.tgz"));
        assert!(!HttpRefKind.claims("git:x"));

        // HTTP error status
        let port = serve(Vec::new(), "500 X");
        let spec = refs
            .parse(&format!("http://127.0.0.1:{port}/x.tgz"))
            .unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let d3 = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d3.path()).is_err());

        // digest mismatch
        let port = serve(bytes.clone(), "200 OK");
        let spec = refs
            .parse(&format!(
                "http://127.0.0.1:{port}/x.tgz#sha256:{}",
                "a".repeat(64)
            ))
            .unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let d4 = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d4.path()).is_err());

        // dead server
        let spec = refs.parse("http://127.0.0.1:1/x.tgz").unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let d5 = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d5.path()).is_err());

        // foreign spec / plan
        let source = HttpSource::new();
        assert!(!source.accepts(&1u8));
        assert!(source.plan(&1u8).is_err());
        assert!(source.fetch(&ForeignPlan, d5.path()).is_err());
    }
}
