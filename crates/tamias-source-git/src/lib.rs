#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source-git — the `git:` ref kind + source: a clone URL with an
//! optional `//subdir` and `#rev`. Planning pins the rev to a commit sha up
//! front (via `ls-remote`), so the content-addressed cache can be hit
//! *before* fetching.

use std::any::Any;
use std::path::Path;
use std::process::Command;

use tamias_core::{copy_tree, Error};
use tamias_source::{expand_path, Fetched, Plan, RefKind, Source, Spec};

/// The `git:` ref kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitRefKind;

/// A parsed git ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSpec {
    /// Normalized clone URL (`org/repo` → `https://github.com/org/repo`).
    pub url: String,
    /// In-repo path after `//` (or the post-two-segment remainder).
    pub subdir: Option<String>,
    /// `#<branch|tag|sha>` — the version pin.
    pub rev: Option<String>,
}

impl RefKind for GitRefKind {
    fn claims(&self, input: &str) -> bool {
        input.starts_with("git:")
    }

    fn parse(&self, input: &str) -> Result<Spec, Error> {
        parse_git(input.strip_prefix("git:").unwrap_or_default())
    }
}

/// `git:<rest>` — `#rev` off the tail, `//subdir` out of the middle, then
/// clone-URL normalization.
fn parse_git(rest: &str) -> Result<Spec, Error> {
    if rest.is_empty() {
        return Err(Error::Failed("git: ref is empty".to_string()));
    }
    let (head, rev) = match rest.split_once('#') {
        Some((head, rev)) => {
            if rev.is_empty() {
                return Err(Error::Failed("empty ref after '#'".to_string()));
            }
            if rev.contains('#') {
                return Err(Error::Failed(format!("malformed ref '#{rev}'")));
            }
            (head, Some(rev.to_string()))
        }
        None => (rest, None),
    };
    if head.is_empty() {
        return Err(Error::Failed("git: ref missing the repo".to_string()));
    }
    let (repo, subdir) = split_subdir(head)?;
    Ok(Box::new(GitSpec {
        url: normalize_clone_url(repo),
        subdir,
        rev,
    }))
}

/// Pull the `//subdir` out of `head`; fall back to the two-segment rule.
fn split_subdir(head: &str) -> Result<(&str, Option<String>), Error> {
    let scan = head.find("://").map_or(0, |i| i + 3);
    if let Some(i) = head[scan..].find("//") {
        let repo = &head[..scan + i];
        let subdir = head[scan + i + 2..].trim_end_matches('/');
        if subdir.is_empty() || subdir == "." {
            return Ok((repo, None));
        }
        if subdir.starts_with('/') || subdir.split('/').any(|c| c == "..") {
            return Err(Error::Failed(format!("bad in-repo path '//{subdir}'")));
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
        return Err(Error::Failed(format!("bad in-repo path '/{subdir}'")));
    }
    Ok((&head[..repo_end], Some(subdir.to_string())))
}

/// scp-style ssh — `git@host:path`, `host:path`. The `:` lands before the
/// first `/` (a `://` was already ruled out by the caller).
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

/// Run `git` in `cwd`, returning stdout (or the stderr on failure).
fn run_git(cwd: &Path, args: &[&str]) -> Result<String, Error> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| Error::Failed(format!("git {}: {e}", args.join(" "))))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 40 lowercase-hex chars — already fully pinned, no `ls-remote`.
fn is_full_sha(rev: &str) -> bool {
    rev.len() == 40
        && rev
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// A hex prefix (7..40) resolves at fetch — the clone carries the object.
fn is_hex_prefix(rev: &str) -> bool {
    rev.len() >= 7
        && rev.len() < 40
        && rev
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `~`/`/`-leading clone paths expand like fs refs.
fn expand_url(url: &str) -> Result<String, Error> {
    if !(url.starts_with('~') || url.starts_with('/')) {
        return Ok(url.to_string());
    }
    expand_path(Path::new(url))
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Failed(format!("non-UTF-8 clone path '{url}'")))
}

/// `ls-remote` stdout → `(refname → sha)`.
fn parse_ls_remote(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let (sha, refname) = line.split_once('\t')?;
            Some((refname.trim().to_string(), sha.trim().to_string()))
        })
        .collect()
}

fn resolve_remote(url: &str, rev: Option<&str>) -> Result<String, Error> {
    let out = run_git(Path::new("."), &["ls-remote", url])?;
    let refs = parse_ls_remote(&out);
    let pick = |name: &str| refs.iter().find(|(r, _)| r == name).map(|(_, s)| s.clone());
    match rev {
        Some(rev) => {
            for cand in [
                format!("refs/tags/{rev}^{{}}"),
                format!("refs/tags/{rev}"),
                format!("refs/heads/{rev}"),
            ] {
                if let Some(sha) = pick(&cand) {
                    return Ok(sha);
                }
            }
            Err(Error::Failed(format!("ref '{rev}' not found on '{url}'")))
        }
        None => pick("HEAD").ok_or_else(|| Error::Failed(format!("no HEAD on '{url}'"))),
    }
}

/// The git source.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitSource;

impl GitSource {
    /// The git source — stateless.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

struct GitPlan {
    url: String,
    subdir: Option<String>,
    rev: Option<String>,
    sha: Option<String>,
}

impl Plan for GitPlan {
    fn prefetch_identity(&self) -> Option<(String, String)> {
        self.sha
            .as_ref()
            .map(|sha| (format!("git-{sha}"), sha.clone()))
    }
}

impl Source for GitSource {
    fn accepts(&self, spec: &dyn Any) -> bool {
        spec.is::<GitSpec>()
    }

    fn plan(&self, spec: &dyn Any) -> Result<Box<dyn Plan>, Error> {
        let GitSpec { url, subdir, rev } = spec
            .downcast_ref::<GitSpec>()
            .ok_or_else(|| Error::Failed("GitSource only plans git refs".to_string()))?;
        let url = expand_url(url)?;
        let sha = match rev.as_deref() {
            Some(rev) if is_full_sha(rev) => Some(rev.to_string()),
            Some(rev) if is_hex_prefix(rev) => None,
            other => Some(resolve_remote(&url, other)?),
        };
        Ok(Box::new(GitPlan {
            url,
            subdir: subdir.clone(),
            rev: rev.clone(),
            sha,
        }))
    }

    fn fetch(&self, plan: &dyn Plan, dest_dir: &Path) -> Result<Fetched, Error> {
        let GitPlan {
            url,
            subdir,
            rev,
            sha,
        } = plan
            .downcast_ref::<GitPlan>()
            .ok_or_else(|| Error::Failed("GitSource only fetches git plans".to_string()))?;
        std::fs::create_dir_all(dest_dir)
            .map_err(Error::io(format!("create {}", dest_dir.display())))?;
        let clone = dest_dir.join(".clone");
        let clone_str = clone.to_string_lossy().to_string();
        run_git(dest_dir, &["clone", "--quiet", url, &clone_str])?;
        let want = sha.as_deref().or(rev.as_deref()).unwrap_or("HEAD");
        let want_commit = format!("{want}^{{commit}}");
        let sha = run_git(&clone, &["rev-parse", "--verify", &want_commit])?
            .trim()
            .to_string();
        run_git(&clone, &["checkout", "--quiet", &sha])?;
        let payload = subdir
            .as_ref()
            .map_or_else(|| clone.clone(), |s| clone.join(s));
        if !payload.is_dir() {
            return Err(Error::Failed(format!(
                "subdir '{}' not in {url}@{sha}",
                subdir.as_deref().unwrap_or_default()
            )));
        }
        copy_tree(&payload, dest_dir)?;
        std::fs::remove_dir_all(&clone).map_err(Error::io(format!("clear {}", clone.display())))?;
        Ok(Fetched {
            dir: dest_dir.to_path_buf(),
            resolved: sha.clone(),
            content_key: format!("git-{sha}"),
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::process::Command;
    use std::sync::Arc;

    use super::*;
    use tamias_source::{Refs, Sources};

    struct ForeignPlan;
    impl Plan for ForeignPlan {}

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn grammar_plan_fetch_and_errors() {
        let refs = Refs::new().with(Arc::new(GitRefKind));
        let sources = Sources::new().with(Arc::new(GitSource::new()));

        // grammar
        let spec = refs
            .parse("git:9aia/peregrin//libraries/ci-gate#v0.2.0")
            .unwrap();
        let GitSpec { url, subdir, rev } = *spec.downcast::<GitSpec>().unwrap();
        assert_eq!(url, "https://github.com/9aia/peregrin");
        assert_eq!(subdir.as_deref(), Some("libraries/ci-gate"));
        assert_eq!(rev.as_deref(), Some("v0.2.0"));
        assert_eq!(
            *refs
                .parse("git:org/repo")
                .unwrap()
                .downcast::<GitSpec>()
                .unwrap(),
            GitSpec {
                url: "https://github.com/org/repo".into(),
                subdir: None,
                rev: None
            }
        );
        assert!(refs.parse("git:").is_err());
        assert!(refs.parse("git:x/y#").is_err());
        assert!(refs.parse("git:x/y#a#b").is_err());
        assert!(refs.parse("git:x/y//a/../b").is_err());
        assert!(GitRefKind.claims("git:x"));
        assert!(!GitRefKind.claims("./x"));

        // a real repo → plan pins the sha; fetch clones
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("tamias.yaml"), "name: g\nversion: 1.0.0\n").unwrap();
        std::fs::write(repo.path().join("a.txt"), "a").unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "i"]);
        let url = format!("git:file://{}#main", repo.path().display());
        let spec = refs.parse(&url).unwrap();
        let planned = sources.plan(&*spec).unwrap();
        assert!(planned.prefetch_identity().unwrap().0.starts_with("git-"));
        let dest = tempfile::tempdir().unwrap();
        let fetched = planned.fetch(dest.path()).unwrap();
        assert!(fetched.content_key.starts_with("git-"));
        assert!(dest.path().join("a.txt").is_file());

        // a full-sha rev needs no ls-remote
        let sha = run_git(&repo.path().join("."), &["rev-parse", "HEAD"]).unwrap();
        let spec = refs
            .parse(&format!(
                "git:file://{}#{}",
                repo.path().display(),
                sha.trim()
            ))
            .unwrap();
        assert!(sources.plan(&*spec).is_ok());

        // errors: unknown rev / bad subdir / dead remote
        let spec = refs
            .parse(&format!("git:file://{}#nosuchrev", repo.path().display()))
            .unwrap();
        assert!(sources.plan(&*spec).is_err());
        let spec = refs
            .parse(&format!("git:file://{}//nope#main", repo.path().display()))
            .unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let d2 = tempfile::tempdir().unwrap();
        assert!(planned.fetch(d2.path()).is_err());
        let spec = refs.parse("git:file:///nonexistent-xyz-999#main").unwrap();
        assert!(sources.plan(&*spec).is_err());

        // foreign spec / plan
        let source = GitSource::new();
        assert!(!source.accepts(&1u8));
        assert!(source.plan(&1u8).is_err());
        assert!(source.fetch(&ForeignPlan, d2.path()).is_err());
    }
}
