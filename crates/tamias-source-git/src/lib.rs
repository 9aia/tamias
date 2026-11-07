//! tamias-source-git — the `git:` arm of the ref grammar: any clone URL,
//! `//subdir`, and `#<branch|tag|sha>` pins. `resolve` pins the ref to a
//! commit sha via `ls-remote` — the resolved sha is the content key
//! (`git-<sha>`), so a warm cache makes `install` offline-free. Git work
//! shells out to the `git` binary.

use std::path::Path;
use std::process::Command;

use tamias_core::{copy_tree, expand_path, Error, SourceRef};
use tamias_source::{Fetched, GitRef, Resolved, Source};

/// [`Source`] over git remotes.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitSource;

impl GitSource {
    /// The git source.
    #[must_use]
    pub fn new() -> Self {
        Self
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

impl Source for GitSource {
    fn resolve(&self, reference: &SourceRef) -> Result<Resolved, Error> {
        let SourceRef::Git(GitRef { url, subdir, rev }) = reference else {
            return Err(Error::Failed(
                "GitSource only resolves git refs".to_string(),
            ));
        };
        let url = expand_url(url)?;
        let sha = match rev.as_deref() {
            Some(rev) if is_full_sha(rev) => Some(rev.to_string()),
            Some(rev) if is_hex_prefix(rev) => None,
            other => Some(resolve_remote(&url, other)?),
        };
        Ok(Resolved::Git {
            url,
            subdir: subdir.clone(),
            rev: rev.clone(),
            sha,
        })
    }

    fn fetch(&self, resolved: &Resolved, dest_dir: &Path) -> Result<Fetched, Error> {
        let Resolved::Git {
            url,
            subdir,
            rev,
            sha,
        } = resolved
        else {
            return Err(Error::Failed(
                "GitSource only fetches git specs".to_string(),
            ));
        };
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
