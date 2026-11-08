#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source-cf — the `cf:` arm: a **Cloudflare registry**. Resolves
//! `<name>@<version>` against the registry index (`<base>/index.json`),
//! then fetches the tarball over HTTP (delegating to `tamias-source-http`).
//!
//! The ref grammar is `cf:<account>/<bucket…>/<name>@<version>`, where the
//! address derives the base URL: a full `http(s)://…` passes through, a
//! first segment with no dot is a `workers.dev` subdomain, and later
//! segments are a path prefix.

use std::path::Path;

use tamias_core::{CfRef, Error, SourceRef};
use tamias_registry::Registry;
use tamias_registry_cf::HttpRegistry;
use tamias_source::{Fetched, Resolved, Source};
use tamias_source_http::HttpSource;

/// The `cf:` source — a Cloudflare registry.
#[derive(Clone, Copy, Debug, Default)]
pub struct CfSource;

impl CfSource {
    /// The source — stateless.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Source for CfSource {
    fn resolve(&self, reference: &SourceRef) -> Result<Resolved, Error> {
        let SourceRef::Cf(CfRef {
            base_url,
            name,
            version,
            ..
        }) = reference
        else {
            return Err(Error::Failed("CfSource only resolves cf: refs".to_string()));
        };
        let base = base_url.trim_end_matches('/').to_string();
        let entries = HttpRegistry::from_env(base.clone()).entries()?;
        let entry = entries
            .into_iter()
            .find(|e| &e.name == name && e.version.as_deref() == Some(version.as_str()))
            .ok_or_else(|| Error::Failed(format!("{name}@{version} not found in {base}")))?;
        Ok(Resolved::Http {
            url: format!("{base}/{}", entry.file),
            digest: None,
        })
    }

    fn fetch(&self, resolved: &Resolved, dest_dir: &Path) -> Result<Fetched, Error> {
        // A `cf:` resolves to an http tarball — the http source does the rest.
        HttpSource::new().fetch(resolved, dest_dir)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use tamias_core::parse_ref;

    /// A local registry serving `hello@0.1.0`; returns its base URL + the
    /// kept-alive root dir.
    fn serve_registry(port: &str) -> (String, tempfile::TempDir) {
        use tamias_registry_local::{serve_http, LocalRegistry};
        let reg = tempfile::tempdir().unwrap();
        let root = reg.path().to_path_buf();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("tamias.yaml"),
            "name: hello\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        let manifest = tamias_core::Manifest::load(src.path()).unwrap();
        LocalRegistry::new(&root)
            .publish(src.path(), &manifest)
            .unwrap();
        let addr = format!("127.0.0.1:{port}");
        let served = root.clone();
        std::thread::spawn(move || {
            let _ = serve_http(&addr, &served);
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        (format!("http://127.0.0.1:{port}"), reg)
    }

    #[test]
    fn resolve_and_fetch_through_the_registry() {
        let (base, _reg) = serve_registry("8901");
        let reference = parse_ref(&format!("cf:{base}/hello@0.1.0")).unwrap();
        let resolved = CfSource::new().resolve(&reference).unwrap();
        let Resolved::Http { url, .. } = &resolved else {
            panic!("expected http");
        };
        assert_eq!(url, &format!("{base}/hello/0.1.0.tgz"));

        let dest = tempfile::tempdir().unwrap();
        let fetched = CfSource::new().fetch(&resolved, dest.path()).unwrap();
        assert!(dest.path().join("a.txt").is_file());
        assert!(fetched.resolved.starts_with("sha256:"));
    }

    #[test]
    fn error_arms() {
        // wrong variant
        let fs = parse_ref("./x").unwrap();
        assert!(CfSource::new().resolve(&fs).is_err());
        // missing package (server up) → not found
        let (base, _reg) = serve_registry("8902");
        let missing = parse_ref(&format!("cf:{base}/ghost@9.9.9")).unwrap();
        assert!(CfSource::new().resolve(&missing).is_err());
        // dead registry → resolve error
        let dead = parse_ref("cf:http://127.0.0.1:1/ghost@1.0.0").unwrap();
        assert!(CfSource::new().resolve(&dead).is_err());
        // fetch of a non-http resolved → error
        let fsres = Resolved::Fs {
            path: std::path::PathBuf::from("/x"),
        };
        let dest = tempfile::tempdir().unwrap();
        assert!(CfSource::new().fetch(&fsres, dest.path()).is_err());
    }
}
