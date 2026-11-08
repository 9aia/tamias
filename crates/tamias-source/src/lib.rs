#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source — the source port: turn a **ref** into bytes.
//!
//! `resolve` turns a ref into a [`Resolved`] fetch spec (git pins `#rev`
//! to a commit sha here, via `ls-remote`); `fetch` materializes the payload
//! into a caller-owned dir. The fetch→unpack split is what makes the
//! content-addressed cache work — repeat installs and air-gapped nodes
//! reuse cache entries instead of re-fetching.

use std::path::{Path, PathBuf};

use tamias_core::Error;
pub use tamias_core::{parse_ref, CfRef, GitRef, HttpRef, SourceRef};

/// What [`Source::resolve`] produces — the concrete fetch spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// Local dir or tarball file to copy/unpack.
    Fs {
        /// `~`-expanded path.
        path: PathBuf,
    },
    /// A git clone — `sha` is filled when resolve pinned it via `ls-remote`.
    Git {
        /// Normalized clone URL.
        url: String,
        /// In-repo path, if any.
        subdir: Option<String>,
        /// The requested ref (kept for fetch when `sha` is None).
        rev: Option<String>,
        /// Resolved commit sha — the pin recorded in the store.
        sha: Option<String>,
    },
    /// A tarball GET.
    Http {
        /// Fetch URL.
        url: String,
        /// Expected tarball sha256 hex, when the ref pinned it.
        digest: Option<String>,
    },
}

impl Resolved {
    /// `(content_key, resolved_id)` — both knowable *before* the fetch or
    /// neither: `git-<sha>` for resolved/pinned git refs, `sha256-<hex>`
    /// for digest-pinned tarballs.
    #[must_use]
    pub fn prefetch_identity(&self) -> Option<(String, String)> {
        match self {
            Self::Git { sha: Some(sha), .. } => Some((format!("git-{sha}"), sha.clone())),
            Self::Http {
                digest: Some(digest),
                ..
            } => Some((format!("sha256-{digest}"), format!("sha256:{digest}"))),
            _ => None,
        }
    }
}

/// What [`Source::fetch`] produced — an unpacked payload plus the identity
/// the store records.
#[derive(Clone, Debug)]
pub struct Fetched {
    /// Dir holding the unpacked payload.
    pub dir: PathBuf,
    /// Resolved identifier recorded in the store — commit sha or
    /// `sha256:<hex>` content digest.
    pub resolved: String,
    /// Content-addressed cache key (`git-<sha>` / `sha256-<hex>`).
    pub content_key: String,
}

/// The source port. One impl per transport.
pub trait Source: Send + Sync {
    /// Turn a parsed ref into a fetch spec.
    ///
    /// # Errors
    /// Transport/validation failures.
    fn resolve(&self, reference: &SourceRef) -> Result<Resolved, Error>;

    /// Materialize the payload into `dest_dir` (created as needed).
    ///
    /// # Errors
    /// Transport/validation failures.
    fn fetch(&self, resolved: &Resolved, dest_dir: &Path) -> Result<Fetched, Error>;
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn prefetch_identity_arms() {
        let fs = Resolved::Fs {
            path: std::path::PathBuf::from("/x"),
        };
        assert!(fs.prefetch_identity().is_none());
        let git = Resolved::Git {
            url: "u".into(),
            subdir: None,
            rev: None,
            sha: Some("abc".into()),
        };
        assert_eq!(git.prefetch_identity().unwrap().0, "git-abc");
        let http = Resolved::Http {
            url: "u".into(),
            digest: Some("deadbeef".into()),
        };
        assert_eq!(http.prefetch_identity().unwrap().0, "sha256-deadbeef");
    }
}
