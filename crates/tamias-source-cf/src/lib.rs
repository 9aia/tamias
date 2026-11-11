#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source-cf — the `cf:` ref kind + source: a **Cloudflare registry**.
//! `<name>@<version>` resolves against the registry index (`<base>/index.json`),
//! then the tarball is fetched over HTTP (delegating to `tamias-source-http`).
//!
//! The grammar is `cf:<account>/<bucket…>/<name>@<version>`, where the address
//! derives the base URL: a full `http(s)://…` passes through, a first segment
//! with no dot is a `workers.dev` subdomain, and later segments are a path
//! prefix.

use std::any::Any;
use std::path::Path;

use tamias_core::{dir_digest, payload_root, Error};
use tamias_registry::Registry;
use tamias_registry_cf::HttpRegistry;
use tamias_source::{Fetched, Plan, RefKind, Source, Spec};
use tamias_source_http::{HttpPlan, HttpSource};

/// The `cf:` ref kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct CfRefKind;

/// A parsed `cf:` ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CfSpec {
    /// Registry base URL, derived from the address (`cf:`) or taken
    /// verbatim from the named registry's `url` (registry refs).
    pub base_url: String,
    /// The `<account>/<bucket…>` address as written, or the registry
    /// label — provenance only, never fetched.
    pub address: String,
    /// Bare package name.
    pub name: String,
    /// `@scope/` namespace when the package is scoped — `None` for
    /// `cf:` refs (the `cf:` grammar can't express scopes) and for
    /// unscoped registry-ref names.
    pub scope: Option<String>,
    /// Declared version.
    pub version: String,
    /// Env var **name** holding the bearer token — set by
    /// registry-scoped refs (`<registry>:<name>@<version>`) from the
    /// registry's `token_env`; `None` → `TAMIAS_REGISTRY_TOKEN` only
    /// (the `cf:` behavior). The token *value* is resolved at plan
    /// time, never stored in the spec.
    pub token_env: Option<String>,
}

impl RefKind for CfRefKind {
    fn claims(&self, input: &str) -> bool {
        input.starts_with("cf:")
    }

    fn parse(&self, input: &str) -> Result<Spec, Error> {
        parse_cf(input.strip_prefix("cf:").unwrap_or_default())
    }
}

/// `cf:<account>/<bucket…>/<name>@<version>` → a [`CfSpec`].
fn parse_cf(body: &str) -> Result<Spec, Error> {
    let bad = || {
        Error::Failed(format!(
            "bad cf ref '{body}' — expected cf:<account>/<bucket…>/<name>@<version>"
        ))
    };
    let (addr, version) = body.rsplit_once('@').ok_or_else(bad)?;
    let (address, name) = addr.rsplit_once('/').ok_or_else(bad)?;
    if address.is_empty() || name.is_empty() || version.is_empty() {
        return Err(bad());
    }
    Ok(Box::new(CfSpec {
        base_url: cf_base_url(address),
        address: address.to_string(),
        name: name.to_string(),
        scope: None,
        version: version.to_string(),
        token_env: None,
    }))
}

/// `<account>/<bucket…>` → a registry base URL. A full `http(s)://…` address
/// passes through; a first segment with no dot is a `workers.dev` subdomain;
/// later segments are a path prefix.
fn cf_base_url(address: &str) -> String {
    if address.starts_with("http://") || address.starts_with("https://") {
        return address.trim_end_matches('/').to_string();
    }
    let mut segments = address.split('/');
    let host = segments.next().unwrap_or_default();
    let host = if host.contains('.') {
        host.to_string()
    } else {
        format!("{host}.workers.dev")
    };
    let prefix: Vec<&str> = segments.collect();
    if prefix.is_empty() {
        format!("https://{host}")
    } else {
        format!("https://{host}/{}", prefix.join("/"))
    }
}

/// The `cf:` source — a Cloudflare registry.
#[derive(Clone, Copy, Debug)]
pub struct CfSource {
    /// Fetch cap handed to the delegated [`HttpSource`] — the configured
    /// `limits.max_download` when the caller threads it through.
    max_download: u64,
}

impl CfSource {
    /// The source.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_download: tamias_source_http::DEFAULT_MAX_DOWNLOAD,
        }
    }

    /// Cap fetched tarballs at `bytes` — the configured `max_download`.
    #[must_use]
    pub fn with_max_download(mut self, bytes: u64) -> Self {
        self.max_download = bytes;
        self
    }
}

impl Default for CfSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the bearer token — `token_env` names the env var holding it
/// (registry-scoped refs carry the named registry's `token_env`; `cf:`
/// refs pass `None`). An unset/empty named var warns and falls back to
/// `TAMIAS_REGISTRY_TOKEN`, preserving the `cf:` behavior. Only env var
/// *names* live in specs — values resolve here, at plan time.
fn resolve_token(token_env: Option<&str>) -> Option<String> {
    token_env
        .and_then(|var| match std::env::var(var) {
            Ok(value) if !value.is_empty() => Some(value),
            _ => {
                eprintln!("tamias: warning — ${var} is unset; no token");
                None
            }
        })
        .or_else(|| {
            std::env::var("TAMIAS_REGISTRY_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
        })
}

/// The index client for `base`, authenticated with `token` when set.
fn http_client(base: &str, token: Option<String>) -> HttpRegistry {
    let mut registry = HttpRegistry::new(base);
    if let Some(token) = token {
        registry = registry.with_token(token);
    }
    registry
}

struct CfPlan {
    url: String,
    /// The index's recorded `dir_digest` of the published payload root —
    /// the *expected* unpacked-payload digest. The tarball itself carries
    /// no pin, so this is the only provenance check between "the registry
    /// said hello@1.0.0 lives at this URL" and "these bytes are it".
    digest: String,
    /// The index entry's `ssh-keygen -Y` signature (`publish --sign`),
    /// when present — handed to the install pipeline via
    /// [`Fetched::signature`](tamias_source::Fetched::signature).
    signature: Option<String>,
    /// The bearer token resolved at plan time — the tarball fetch is a
    /// plain HTTP GET, so auth must ride the plan (token-guarded
    /// registries 401 the blob without it).
    token: Option<String>,
}

impl Plan for CfPlan {}

impl Source for CfSource {
    fn accepts(&self, spec: &dyn Any) -> bool {
        spec.is::<CfSpec>()
    }

    fn plan(&self, spec: &dyn Any) -> Result<Box<dyn Plan>, Error> {
        let CfSpec {
            base_url,
            name,
            scope,
            version,
            token_env,
            ..
        } = spec
            .downcast_ref::<CfSpec>()
            .ok_or_else(|| Error::Failed("CfSource only plans cf refs".to_string()))?;
        let base = base_url.trim_end_matches('/').to_string();
        let token = resolve_token(token_env.as_deref());
        let entries = http_client(&base, token.clone()).entries()?;
        let entry = entries
            .into_iter()
            .find(|e| {
                &e.name == name
                    && e.scope.as_deref() == scope.as_deref()
                    && e.version.as_deref() == Some(version.as_str())
            })
            .ok_or_else(|| Error::Failed(format!("{name}@{version} not found in {base}")))?;
        Ok(Box::new(CfPlan {
            url: format!("{base}/{}", entry.file),
            digest: entry.digest,
            signature: entry.signature,
            token,
        }))
    }

    fn fetch(&self, plan: &dyn Plan, dest_dir: &Path) -> Result<Fetched, Error> {
        // A `cf:` plan is an http tarball — the http source does the rest.
        let CfPlan {
            url,
            digest,
            signature,
            token,
        } = plan
            .downcast_ref::<CfPlan>()
            .ok_or_else(|| Error::Failed("CfSource only fetches cf plans".to_string()))?;
        let mut fetched = HttpSource::new()
            .with_max_download(self.max_download)
            .fetch(
                &HttpPlan::new(url.clone(), None).with_token(token.clone()),
                dest_dir,
            )?;
        // Verify the unpacked payload against the digest the registry
        // index recorded at publish — a swapped or truncated tarball
        // fails here instead of installing under the right name@version.
        let actual = dir_digest(&payload_root(&fetched.dir)?)?;
        if actual != *digest {
            return Err(Error::Failed(format!(
                "payload digest mismatch for {url}: {actual} ≠ index {digest}"
            )));
        }
        // The index's signature rides along — the install pipeline
        // verifies it against allowed_signers before the store commit.
        fetched.signature = signature.clone();
        Ok(fetched)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::Arc;

    use super::*;
    use tamias_source::{Refs, Sources};

    struct ForeignPlan;
    impl Plan for ForeignPlan {}

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
            .publish(src.path(), &manifest, None)
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
    fn grammar_plan_fetch_and_errors() {
        let refs = Refs::new().with(Arc::new(CfRefKind));
        let sources = Sources::new().with(Arc::new(CfSource::new()));

        // grammar + base-URL derivation
        let spec = refs.parse("cf:9aia/bucket/hello@0.1.0").unwrap();
        let CfSpec {
            base_url,
            address,
            name,
            version,
            ..
        } = *spec.downcast::<CfSpec>().unwrap();
        assert_eq!(base_url, "https://9aia.workers.dev/bucket");
        assert_eq!(address, "9aia/bucket");
        assert_eq!(name, "hello");
        assert_eq!(version, "0.1.0");
        let spec = refs.parse("cf:reg.example.com/hi@1.2.3").unwrap();
        assert_eq!(
            spec.downcast::<CfSpec>().unwrap().base_url,
            "https://reg.example.com"
        );
        assert!(refs.parse("cf:").is_err());
        assert!(refs.parse("cf:name@1.0.0").is_err());
        assert!(refs.parse("cf:a/b/name").is_err());
        assert!(refs.parse("cf:a/b/@1.0.0").is_err());
        assert!(refs.parse("cf:a/b/name@").is_err());
        assert!(CfRefKind.claims("cf:x/y@1"));
        assert!(!CfRefKind.claims("./x"));

        // live registry: plan + fetch
        let (base, _reg) = serve_registry("8911");
        let spec = refs.parse(&format!("cf:{base}/hello@0.1.0")).unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let fetched = planned.fetch(dest.path()).unwrap();
        assert!(fetched.resolved.starts_with("sha256:"));
        assert!(dest.path().join("a.txt").is_file());

        // missing version → plan error
        let spec = refs.parse(&format!("cf:{base}/ghost@9.9.9")).unwrap();
        assert!(sources.plan(&*spec).is_err());
        // dead registry → plan error
        let spec = refs.parse("cf:http://127.0.0.1:1/ghost@1.0.0").unwrap();
        assert!(sources.plan(&*spec).is_err());

        // foreign spec / plan
        let source = CfSource::new();
        assert!(!source.accepts(&1u8));
        assert!(source.plan(&1u8).is_err());
        assert!(source.fetch(&ForeignPlan, dest.path()).is_err());
    }

    #[test]
    fn fetch_rejects_payload_digest_mismatch() {
        let refs = Refs::new().with(Arc::new(CfRefKind));
        let sources = Sources::new().with(Arc::new(CfSource::new()));
        let (base, reg) = serve_registry("8912");
        let spec = refs.parse(&format!("cf:{base}/hello@0.1.0")).unwrap();
        let planned = sources.plan(&*spec).unwrap();
        // happy path first — the index digest verifies the unpacked payload
        let d1 = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d1.path()).is_ok());
        // swap the served tarball for a different payload under the same
        // name@version → the index digest must reject it
        let evil = tempfile::tempdir().unwrap();
        std::fs::write(
            evil.path().join("tamias.yaml"),
            "name: hello\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(evil.path().join("a.txt"), "evil").unwrap();
        tamias_core::pack_tgz_atomic(evil.path(), &reg.path().join("hello/0.1.0.tgz")).unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let err = planned.fetch(d2.path()).unwrap_err();
        assert!(err.to_string().contains("digest mismatch"), "{err}");
    }
}
