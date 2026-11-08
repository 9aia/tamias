//! The ref grammar — *where* a package comes from.
//!
//! ```text
//! ./path  ../path  /abs/path  ~/path  nested/path    → fs
//! git:<url>[//<subdir>][#<ref>]                      → git
//! https://…/name.tgz | .tar.gz [#sha256:<hex>]        → http
//! ```
//!
//! Git specifics (the go-getter conventions): `//` splits the repo path
//! from the in-repo path — required because nested groups make bare
//! `a/b/c/d` ambiguous. Without `//`, a two-segment prefix is the repo
//! and the remainder is the in-repo path (shorthand only). `org/repo`
//! defaults to github.com; `#ref` is branch/tag/sha — the version pin.

use std::path::{Path, PathBuf};

/// A parsed ref — which source impl handles it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceRef {
    /// A local path — `./x`, `/x`, `~/x`, `nested/x`.
    Fs(PathBuf),
    /// `git:` — any clone URL with optional `//subdir` and `#rev`.
    Git(GitRef),
    /// `http(s)://….tgz|tar.gz` with an optional `#sha256:` pin.
    Http(HttpRef),
}

/// The `git:` arm of a [`SourceRef`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRef {
    /// Normalized clone URL (`org/repo` → `https://github.com/org/repo`).
    pub url: String,
    /// In-repo path after `//` (or the post-two-segment remainder).
    pub subdir: Option<String>,
    /// `#<branch|tag|sha>` — the version pin.
    pub rev: Option<String>,
}

/// The tarball arm of a [`SourceRef`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRef {
    /// URL minus the `#sha256:` fragment.
    pub url: String,
    /// Expected sha256 hex of the tarball bytes, when pinned.
    pub digest: Option<String>,
}

/// Ref-grammar failures.
#[derive(Debug, thiserror::Error)]
#[error("invalid ref: {0}")]
pub struct RefError(pub String);

/// Parse a ref into its [`SourceRef`] arm.
///
/// # Errors
/// Returns [`RefError`] when the input matches no arm of the grammar.
pub fn parse_ref(input: &str) -> Result<SourceRef, RefError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(RefError("empty ref".to_string()));
    }
    if let Some(rest) = input.strip_prefix("git:") {
        return parse_git(rest).map(SourceRef::Git);
    }
    if input.starts_with("https://") || input.starts_with("http://") {
        return parse_http(input).map(SourceRef::Http);
    }
    if is_path_ref(input) {
        return Ok(SourceRef::Fs(PathBuf::from(input)));
    }
    Err(RefError(format!(
        "unrecognized ref '{input}' — expected ./path, /path, git:<url>[//subdir][#rev], or an http(s) .tgz/.tar.gz URL"
    )))
}

/// Path-shaped refs — `./x`, `../x`, `~/x`, `/x`, plus bare paths with a
/// separator (`libraries/ci-gate`). A bare name never is.
fn is_path_ref(input: &str) -> bool {
    input == "."
        || input == ".."
        || input == "~"
        || input.starts_with("./")
        || input.starts_with("../")
        || input.starts_with("~/")
        || input.starts_with('/')
        || input.contains('/')
}

/// `git:<rest>` — `#rev` off the tail, `//subdir` out of the middle, then
/// clone-URL normalization.
fn parse_git(rest: &str) -> Result<GitRef, RefError> {
    if rest.is_empty() {
        return Err(RefError("git: ref is empty".to_string()));
    }
    let (head, rev) = match rest.split_once('#') {
        Some((head, rev)) => {
            if rev.is_empty() {
                return Err(RefError("empty ref after '#'".to_string()));
            }
            if rev.contains('#') {
                return Err(RefError(format!("malformed ref '#{rev}'")));
            }
            (head, Some(rev.to_string()))
        }
        None => (rest, None),
    };
    if head.is_empty() {
        return Err(RefError("git: ref missing the repo".to_string()));
    }
    let (repo, subdir) = split_subdir(head)?;
    let url = normalize_clone_url(repo);
    Ok(GitRef { url, subdir, rev })
}

/// Pull the `//subdir` out of `head`; fall back to the two-segment rule.
fn split_subdir(head: &str) -> Result<(&str, Option<String>), RefError> {
    let scan = head.find("://").map_or(0, |i| i + 3);
    if let Some(i) = head[scan..].find("//") {
        let repo = &head[..scan + i];
        let subdir = head[scan + i + 2..].trim_end_matches('/');
        if subdir.is_empty() || subdir == "." {
            return Ok((repo, None));
        }
        if subdir.starts_with('/') || subdir.split('/').any(|c| c == "..") {
            return Err(RefError(format!("bad in-repo path '//{subdir}'")));
        }
        return Ok((repo, Some(subdir.to_string())));
    }
    if head.contains("://")
        || is_scp_style(head)
        || head.starts_with('/')
        || head.starts_with('.')
        || head.starts_with('~')
    {
        return Ok((head, None));
    }
    let Some(first_slash) = head.find('/') else {
        return Ok((head, None));
    };
    let Some(second_slash) = head[first_slash + 1..].find('/') else {
        return Ok((head, None));
    };
    let repo_end = first_slash + 1 + second_slash;
    let subdir = head[repo_end + 1..].trim_end_matches('/');
    if subdir.is_empty() || subdir == "." {
        return Ok((&head[..repo_end], None));
    }
    if subdir.split('/').any(|c| c == "..") {
        return Err(RefError(format!("bad in-repo path '/{subdir}'")));
    }
    Ok((&head[..repo_end], Some(subdir.to_string())))
}

/// scp-style ssh — `git@host:path`, `host:path`. The `:` lands before
/// the first `/` (a `://` was already ruled out by the caller).
fn is_scp_style(head: &str) -> bool {
    head.split('/')
        .next()
        .is_some_and(|first| first.contains(':'))
}

/// Bare shorthand → a clone URL: `org/repo` → github.com,
/// `host/org/repo` → `https://host/…`; URLs and scp pass through.
fn normalize_clone_url(repo: &str) -> String {
    if repo.contains("://")
        || is_scp_style(repo)
        || repo.starts_with('/')
        || repo.starts_with('.')
        || repo.starts_with('~')
    {
        return repo.to_string();
    }
    let mut segments = repo.split('/');
    let first = segments.next().unwrap_or(repo);
    let two_segments = segments.clone().count() == 1;
    if two_segments && !first.contains('.') {
        format!("https://github.com/{repo}")
    } else {
        format!("https://{repo}")
    }
}

/// `http(s)://….tgz|tar.gz[#sha256:<hex>|#sha256=<hex>]`.
/// URLs are case-sensitive — the extension check is deliberately exact.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn parse_http(input: &str) -> Result<HttpRef, RefError> {
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
                    RefError(format!(
                        "unsupported fragment '#{fragment}' — expected '#sha256:<hex>'"
                    ))
                })?;
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(RefError("sha256 digest must be 64 hex chars".to_string()));
            }
            Some(hex.to_ascii_lowercase())
        }
    };
    if !(url.ends_with(".tgz") || url.ends_with(".tar.gz")) {
        return Err(RefError(format!(
            "http refs must point at a .tgz/.tar.gz tarball: {url}"
        )));
    }
    Ok(HttpRef {
        url: url.to_string(),
        digest,
    })
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

    #[test]
    fn fs_arms() {
        assert_eq!(
            parse_ref("./x").unwrap(),
            SourceRef::Fs(PathBuf::from("./x"))
        );
        assert!(matches!(
            parse_ref("libraries/ci-gate").unwrap(),
            SourceRef::Fs(_)
        ));
        assert!(matches!(parse_ref("~/x").unwrap(), SourceRef::Fs(_)));
    }

    #[test]
    fn git_shorthand_and_subdir_and_rev() {
        let SourceRef::Git(g) = parse_ref("git:9aia/peregrin//libraries/ci-gate#v0.2.0").unwrap()
        else {
            panic!("expected git");
        };
        assert_eq!(g.url, "https://github.com/9aia/peregrin");
        assert_eq!(g.subdir.as_deref(), Some("libraries/ci-gate"));
        assert_eq!(g.rev.as_deref(), Some("v0.2.0"));
        // two-segment shorthand, no //
        let SourceRef::Git(g) = parse_ref("git:9aia/peregrin").unwrap() else {
            panic!("expected git");
        };
        assert_eq!(g.url, "https://github.com/9aia/peregrin");
        assert_eq!(g.subdir, None);
    }

    #[test]
    fn http_with_digest() {
        let hex = "a".repeat(64);
        let SourceRef::Http(h) = parse_ref(&format!("https://x/y.tgz#sha256:{hex}")).unwrap()
        else {
            panic!("expected http");
        };
        assert_eq!(h.url, "https://x/y.tgz");
        assert_eq!(h.digest.as_deref(), Some(hex.as_str()));
        assert!(parse_ref("https://x/y.zip").is_err());
    }

    #[test]
    fn rejects() {
        assert!(parse_ref("").is_err());
        assert!(parse_ref("ci-gate").is_err()); // bare name
        assert!(parse_ref("git:").is_err());
        assert!(parse_ref("git:x/y#").is_err());
    }

    #[test]
    fn expand_tilde() {
        std::env::set_var("HOME", "/home/z");
        assert_eq!(
            expand_path(Path::new("~/a/b")),
            PathBuf::from("/home/z/a/b")
        );
        assert_eq!(expand_path(Path::new("/abs")), PathBuf::from("/abs"));
    }

    #[test]
    fn scp_style_and_more_errors() {
        let SourceRef::Git(g) = parse_ref("git:git@github.com:9aia/x.git").unwrap() else {
            panic!("expected git");
        };
        assert!(g.url.contains("git@github.com"));
        assert!(parse_ref("git:x/y#a#b").is_err());
        assert!(parse_ref("http://x/y.tgz#nope:1").is_err());
        let hex = "z".repeat(64);
        assert!(parse_ref(&format!("http://x/y.tgz#sha256:{hex}")).is_err());
    }

    #[test]
    fn shorthand_and_expand_arms() {
        let SourceRef::Git(g) = parse_ref("git:9aia/peregrin/libraries/ci-gate").unwrap() else {
            panic!("expected git");
        };
        assert_eq!(g.subdir.as_deref(), Some("libraries/ci-gate"));
        assert!(matches!(
            parse_ref("http://x/y.tar.gz").unwrap(),
            SourceRef::Http(_)
        ));
        std::env::set_var("HOME", "/h");
        assert_eq!(expand_path(Path::new("~")), PathBuf::from("/h"));
        assert_eq!(expand_path(Path::new("plain")), PathBuf::from("plain"));
        assert!(parse_ref("git:9aia/peregrin//").is_ok());
    }

    #[test]
    fn parse_more_errors() {
        assert!(parse_ref("git:").is_err());
        assert!(parse_ref("git:#rev").is_err());
        assert!(parse_ref("git:x#a#b").is_err());
        assert!(parse_ref("https://x/y.zip").is_err());
        assert!(parse_ref("https://x/y.tgz#sha256:short").is_err());
        assert!(parse_ref("git:x/y//a/../b").is_err());
    }

    #[test]
    fn git_no_slash_trailing_and_host_forms() {
        assert!(parse_ref("git:justrepo").is_ok());
        assert!(parse_ref("git:org/repo/").is_ok());
        assert!(parse_ref("git:org/repo/a/../b").is_err());
        assert!(parse_ref("git:github.com/org/repo").is_ok());
    }

    #[test]
    fn expand_path_non_utf8_is_verbatim() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let p = PathBuf::from(std::ffi::OsStr::from_bytes(b"~\xff"));
            assert_eq!(expand_path(&p), p);
        }
    }
}
