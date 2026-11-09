#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-source-git — the `git:` ref kind + source: a clone URL with an
//! optional `//subdir` and `#rev`. Planning pins the rev to a commit sha up
//! front (via `ls-remote`), so the content-addressed cache can be hit
//! *before* fetching.

use std::any::Any;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tamias_core::{copy_tree, output_with_timeout, Error};
use tamias_source::{expand_path, Fetched, Plan, RefKind, Source, Spec};

/// `git clone` moves the whole repo — large repos need minutes, but a
/// hung remote must not wedge the install forever.
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);

/// `ls-remote`/`rev-parse`/`checkout` are quick probes — a minute is
/// generous for a responsive remote and a local checkout.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

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
    let url = normalize_clone_url(repo);
    validate_clone_url(&url)?;
    if let Some(rev) = &rev {
        validate_rev(rev)?;
    }
    Ok(Box::new(GitSpec { url, subdir, rev }))
}

/// A clone URL git argv can safely carry: an allowlisted scheme, scp-style
/// `host:path`, or a local path — never a leading `-` (which git's option
/// parser would read as a flag like `--upload-pack`) and never whitespace
/// or control chars.
fn validate_clone_url(url: &str) -> Result<(), Error> {
    let bad = url.is_empty()
        || url.starts_with('-')
        || url.chars().any(|c| c.is_whitespace() || c.is_control());
    if bad {
        return Err(Error::Failed(format!("bad clone url '{url}'")));
    }
    let ok = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ssh" | "git" | "file"
        ) {
            return Err(Error::Failed(format!(
                "unsupported clone scheme '{scheme}:'"
            )));
        }
        // the repo path must exist — `https://host` alone names no repo
        rest.split_once('/')
            .is_some_and(|(_, path)| !path.is_empty())
    } else if is_scp_style(url) {
        // `host:path` — the `:` precedes the first `/`; argv-safe because
        // the string can't start with `-` and carries no whitespace.
        true
    } else {
        url.starts_with('/') || url.starts_with('.') || url.starts_with('~')
    };
    if ok {
        Ok(())
    } else {
        Err(Error::Failed(format!("bad clone url '{url}'")))
    }
}

/// A `#rev` pin that can't smuggle a git option: conservative charset
/// (alnum plus `._/+@-~^` — `~`/`^` are relative-rev syntax like
/// `HEAD~2`/`main^`, inert as argv since they can't lead with `-`),
/// no leading `-`, no `..`/`//`/trailing `.`/`/` (mirrors
/// `git check-ref-format`). `{`/`}` stay excluded so `^{…}` peel
/// syntax can't be injected into the `{want}^{commit}` we append.
fn validate_rev(rev: &str) -> Result<(), Error> {
    let ok = !rev.is_empty()
        && !rev.starts_with('-')
        && rev.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+' | '@' | '~' | '^')
        })
        && !rev.contains("..")
        && !rev.contains("//")
        && !rev.ends_with(['/', '.']);
    if ok {
        Ok(())
    } else {
        Err(Error::Failed(format!("bad rev '{rev}'")))
    }
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
/// `timeout` bounds the whole invocation — a hung remote is killed, not
/// waited on forever.
fn run_git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, Error> {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .args(args)
        // Auth must fail, never hang: suppress every interactive prompt
        // (terminal, askpass, ssh askpass). The rest of the env —
        // credential helpers, `GIT_SSH`, the agent — is inherited so
        // legitimate private repos still authenticate non-interactively.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(std::process::Stdio::null());
    let out = output_with_timeout(&mut cmd, timeout)
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

/// A sha `ls-remote`/`rev-parse` reported — hex only, so a hostile remote
/// can't smuggle a `-`-leading option into `checkout` argv.
fn is_sha_hex(sha: &str) -> bool {
    (sha.len() == 40 || sha.len() == 64)
        && sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `ls-remote` stdout → `(refname → sha)`; non-hex shas are dropped so a
/// malicious remote can't inject argv into `rev-parse`/`checkout`.
fn parse_ls_remote(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let (sha, refname) = line.split_once('\t')?;
            let sha = sha.trim();
            is_sha_hex(sha).then(|| (refname.trim().to_string(), sha.to_string()))
        })
        .collect()
}

fn resolve_remote(url: &str, rev: Option<&str>) -> Result<String, Error> {
    let out = run_git(Path::new("."), &["ls-remote", "--", url], GIT_TIMEOUT)?;
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
        // GitSpec fields are public — re-validate at the argv boundary.
        validate_clone_url(url)?;
        if let Some(rev) = rev {
            validate_rev(rev)?;
        }
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
        // `--` ends option parsing: `url` can't become a `--upload-pack`-style
        // flag even if validation were bypassed.
        run_git(
            dest_dir,
            &["clone", "--quiet", "--", url, &clone_str],
            CLONE_TIMEOUT,
        )?;
        let want = sha.as_deref().or(rev.as_deref()).unwrap_or("HEAD");
        let want_commit = format!("{want}^{{commit}}");
        // rev-parse has no `--` form; `want` is charset-validated at parse
        // and the resolved sha is re-checked hex before checkout.
        let sha = run_git(
            &clone,
            &["rev-parse", "--verify", &want_commit],
            GIT_TIMEOUT,
        )?
        .trim()
        .to_string();
        if !is_sha_hex(&sha) {
            return Err(Error::Failed(format!(
                "unexpected rev-parse output '{sha}'"
            )));
        }
        run_git(&clone, &["checkout", "--quiet", &sha], GIT_TIMEOUT)?;
        let payload = subdir
            .as_ref()
            .map_or_else(|| clone.clone(), |s| clone.join(s));
        // `symlink_metadata` — a `subdir` that's a link isn't the payload
        // (copy_tree would walk whatever it points at).
        let payload_is_dir = std::fs::symlink_metadata(&payload)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if !payload_is_dir {
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
        let sha = run_git(&repo.path().join("."), &["rev-parse", "HEAD"], GIT_TIMEOUT).unwrap();
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

    #[test]
    fn hostile_refs_never_reach_git_argv() {
        let refs = Refs::new().with(Arc::new(GitRefKind));
        // each of these would land in `git clone`/`rev-parse` argv as an
        // option (`--upload-pack`, `-b`, …) without validation
        for bad in [
            "git:--upload-pack=touch /tmp/pwned:/x", // scp-shaped + whitespace
            "git:--upload-pack=id:file:///tmp/x",    // scp-shaped, leading '-'
            "git:-x:y",                              // scp-shaped, leading '-'
            "git:https://ok#--upload-pack=x",        // rev as an option
            "git:https://ok#-b",                     // rev leading '-'
            "git:https://ok#a b",                    // rev whitespace
            "git:https://ok#a..b",                   // rev `..`
            "git:https://ok#a//b",                   // rev `//`
            "git:https://ok#v1^{}",                  // rev rev-syntax escape
            "git:foo",                               // bare → no repo path
            "git:foo://bar/baz",                     // scheme not allowlisted
            "git:https://host/",                     // empty repo path
            "git:host:path with space",              // scp + whitespace
        ] {
            assert!(refs.parse(bad).is_err(), "{bad} should be rejected");
        }

        // GitSpec fields are public — plan() must re-validate at the argv
        // boundary even for a hand-built spec
        let source = GitSource::new();
        let spec = Box::new(GitSpec {
            url: "--upload-pack=id".to_string(),
            subdir: None,
            rev: None,
        });
        assert!(source.plan(&*spec).is_err());
        let spec = Box::new(GitSpec {
            url: "https://ok/repo".to_string(),
            subdir: None,
            rev: Some("--upload-pack=x".to_string()),
        });
        assert!(source.plan(&*spec).is_err());

        // the shapes we DO accept still parse
        for good in [
            "git:org/repo",
            "git:git@github.com:org/repo#v1.2.3",
            "git:ssh://git@host:2222/org/repo",
            "git:file:///tmp/repo#main",
            "git:https://host/org/repo//sub/dir#abc1234",
            "git:https://host/org/repo#HEAD~2", // relative rev
            "git:https://host/org/repo#main^",  // relative rev
        ] {
            assert!(refs.parse(good).is_ok(), "{good} should parse");
        }
    }

    #[cfg(unix)]
    #[test]
    fn fetch_skips_symlinked_payload_entries() {
        let refs = Refs::new().with(Arc::new(GitRefKind));
        let sources = Sources::new().with(Arc::new(GitSource::new()));
        // an attacker-controlled repo: `loot -> /` would exfiltrate the
        // whole fs through the old walk; `a -> .` would loop it forever
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("tamias.yaml"), "name: g\nversion: 1.0.0\n").unwrap();
        std::fs::write(repo.path().join("a.txt"), "a").unwrap();
        std::os::unix::fs::symlink("/", repo.path().join("loot")).unwrap();
        std::os::unix::fs::symlink(".", repo.path().join("self")).unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "i"]);
        let url = format!("git:file://{}#main", repo.path().display());
        let spec = refs.parse(&url).unwrap();
        let planned = sources.plan(&*spec).unwrap();
        let dest = tempfile::tempdir().unwrap();
        // completes — links never enter the fetched payload
        let fetched = planned.fetch(dest.path()).unwrap();
        assert!(fetched.content_key.starts_with("git-"));
        assert!(dest.path().join("a.txt").is_file());
        assert!(!dest.path().join("loot").exists());
        assert!(!dest.path().join("self").exists());
        assert!(!dest.path().join("etc").exists());
    }
}
