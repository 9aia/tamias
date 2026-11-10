#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source — the **ref** + **source** ports.
//!
//! The grammar is a port: each [`RefKind`] claims a shape and parses it into
//! an opaque [`Spec`]. The transport is a port: each [`Source`] turns *its
//! own* spec into a [`Plan`] and fetches it. Neither this crate nor the
//! engine knows a concrete transport — kinds live in the
//! `tamias-source-<kind>` crates (`-fs`, `-git`, `-http`, `-cf`).

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tamias_core::Error;

/// A parsed ref — a typed spec owned by its [`RefKind`].
pub type Spec = Box<dyn Any + Send + Sync>;

/// A ref-grammar arm.
pub trait RefKind: Send + Sync {
    /// Whether this kind handles `input`.
    fn claims(&self, input: &str) -> bool;

    /// Parse `input` into a spec.
    ///
    /// # Errors
    /// Fails when `input` is claimed but malformed.
    fn parse(&self, input: &str) -> Result<Spec, Error>;
}

/// The ref registry — parse any ref with the registered kinds.
#[derive(Default)]
pub struct Refs {
    kinds: Vec<Arc<dyn RefKind>>,
}

impl Refs {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a kind. Kinds are tried in order; register the fallback
    /// (paths) last.
    #[must_use]
    pub fn with(mut self, kind: Arc<dyn RefKind>) -> Self {
        self.kinds.push(kind);
        self
    }

    /// Parse `input` with the first registered kind that claims it.
    ///
    /// # Errors
    /// Fails on an empty ref, no claiming kind, or a malformed ref.
    pub fn parse(&self, input: &str) -> Result<Spec, Error> {
        let input = input.trim();
        if input.is_empty() {
            return Err(Error::Failed("empty ref".to_string()));
        }
        let kind = self
            .kinds
            .iter()
            .find(|kind| kind.claims(input))
            .ok_or_else(|| Error::Failed(format!("unrecognized ref '{input}'")))?;
        kind.parse(input)
    }
}

/// A resolved fetch plan.
pub trait Plan: Any + Send + Sync {
    /// `(content_key, resolved_id)` — both knowable *before* the fetch or
    /// neither: `git-<sha>` for a pinned git plan, `sha256-<hex>` for a
    /// digest-pinned tarball.
    fn prefetch_identity(&self) -> Option<(String, String)> {
        None
    }
}

impl dyn Plan {
    /// Downcast to the owning source's concrete plan type.
    #[must_use]
    pub fn downcast_ref<T: Plan>(&self) -> Option<&T> {
        (self as &dyn Any).downcast_ref::<T>()
    }
}

/// A fetch plan paired with the [`Source`] that owns it.
pub struct Planned {
    source: Arc<dyn Source>,
    plan: Box<dyn Plan>,
}

impl Planned {
    /// Pair a source with its plan.
    #[must_use]
    pub fn new(source: Arc<dyn Source>, plan: Box<dyn Plan>) -> Self {
        Self { source, plan }
    }

    /// See [`Plan::prefetch_identity`].
    #[must_use]
    pub fn prefetch_identity(&self) -> Option<(String, String)> {
        self.plan.prefetch_identity()
    }

    /// Materialize the plan into `dest_dir`.
    ///
    /// # Errors
    /// Transport/validation failures.
    pub fn fetch(&self, dest_dir: &Path) -> Result<Fetched, Error> {
        self.source.fetch(&*self.plan, dest_dir)
    }
}

/// The source port. One impl per transport.
pub trait Source: Send + Sync {
    /// Whether this source owns `spec`.
    fn accepts(&self, spec: &dyn Any) -> bool;

    /// Turn its own spec into a fetch plan.
    ///
    /// # Errors
    /// Transport/validation failures.
    fn plan(&self, spec: &dyn Any) -> Result<Box<dyn Plan>, Error>;

    /// Materialize `plan` into `dest_dir`.
    ///
    /// # Errors
    /// Transport/validation failures.
    fn fetch(&self, plan: &dyn Plan, dest_dir: &Path) -> Result<Fetched, Error>;
}

/// The source registry.
#[derive(Default)]
pub struct Sources {
    sources: Vec<Arc<dyn Source>>,
}

impl Sources {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a source.
    #[must_use]
    pub fn with(mut self, source: Arc<dyn Source>) -> Self {
        self.sources.push(source);
        self
    }

    /// Plan `spec` with the first source that accepts it.
    ///
    /// # Errors
    /// Fails when no source accepts the spec, or planning fails.
    pub fn plan(&self, spec: &dyn Any) -> Result<Planned, Error> {
        let source = self
            .sources
            .iter()
            .find(|source| source.accepts(spec))
            .ok_or_else(|| Error::Failed("no source handles this ref".to_string()))?;
        let plan = source.plan(spec)?;
        Ok(Planned::new(Arc::clone(source), plan))
    }
}

/// What a fetch produced — an unpacked payload plus the identity the store
/// records.
#[derive(Clone, Debug)]
pub struct Fetched {
    /// Dir holding the unpacked payload.
    pub dir: PathBuf,
    /// Resolved identifier recorded in the store — a commit sha or
    /// `sha256:<hex>` content digest.
    pub resolved: String,
    /// Content-addressed cache key (`git-<sha>` / `sha256-<hex>`).
    pub content_key: String,
}

/// `~` / `~/…` expansion (`$HOME`, else `$USERPROFILE`, else `/`).
#[must_use]
pub fn expand_path(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Serializes tests that mutate process env/cwd — `cargo test`
    /// runs tests on threads in one process, so mutations must be
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

    /// A toy kind claiming `toy:`, parsing the tail into a `String` spec.
    struct ToyKind;
    impl RefKind for ToyKind {
        fn claims(&self, input: &str) -> bool {
            input.starts_with("toy:")
        }
        fn parse(&self, input: &str) -> Result<Spec, Error> {
            Ok(Box::new(input.trim_start_matches("toy:").to_string()))
        }
    }

    struct ToyPlan(String);
    impl Plan for ToyPlan {
        fn prefetch_identity(&self) -> Option<(String, String)> {
            Some((format!("k-{}", self.0), self.0.clone()))
        }
    }

    struct ToySource;
    impl Source for ToySource {
        fn accepts(&self, spec: &dyn Any) -> bool {
            spec.downcast_ref::<String>().is_some()
        }
        fn plan(&self, spec: &dyn Any) -> Result<Box<dyn Plan>, Error> {
            let name = spec
                .downcast_ref::<String>()
                .ok_or_else(|| Error::Failed("not a toy spec".to_string()))?;
            Ok(Box::new(ToyPlan(name.clone())))
        }
        fn fetch(&self, plan: &dyn Plan, dest_dir: &Path) -> Result<Fetched, Error> {
            let (content_key, resolved) = plan.prefetch_identity().unwrap_or_default();
            std::fs::create_dir_all(dest_dir).map_err(Error::io("create dest"))?;
            Ok(Fetched {
                dir: dest_dir.to_path_buf(),
                resolved,
                content_key,
            })
        }
    }

    #[test]
    fn refs_and_sources_registries() {
        let refs = Refs::new().with(Arc::new(ToyKind));
        let spec = refs.parse("toy:hello").unwrap();
        assert!(refs.parse("  ").is_err());
        assert!(refs.parse("nope").is_err());

        let toy = Arc::new(ToySource);
        let sources = Sources::new().with(toy.clone());
        let planned = sources.plan(&*spec).unwrap();
        assert_eq!(planned.prefetch_identity().unwrap().0, "k-hello");
        let dest = tempfile::tempdir().unwrap();
        let fetched = planned.fetch(dest.path()).unwrap();
        assert_eq!(fetched.content_key, "k-hello");
        assert!(dest.path().is_dir());

        // no source claims it
        assert!(Sources::new().plan(&*spec).is_err());
        // a source rejects a foreign spec
        assert!(!toy.accepts(&42u32));
        assert!(toy.plan(&42u32).is_err());
    }

    #[test]
    fn expand_path_arms() {
        let _g = EnvGuard::new();
        std::env::set_var("HOME", "/home/z");
        assert_eq!(expand_path(Path::new("~/a")), PathBuf::from("/home/z/a"));
        assert_eq!(expand_path(Path::new("~")), PathBuf::from("/home/z"));
        assert_eq!(expand_path(Path::new("/abs")), PathBuf::from("/abs"));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let p = PathBuf::from(std::ffi::OsStr::from_bytes(b"~\xff"));
            assert_eq!(expand_path(&p), p);
        }
    }
}
