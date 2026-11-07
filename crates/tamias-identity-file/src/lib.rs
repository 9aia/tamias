//! tamias-identity-file — the age identity keyfile impl.
//!
//! One `AGE-SECRET-KEY-…` line in a plain file (`#` comments
//! allowed), mode `0600`, at `~/.config/tamias/identity.txt` —
//! exactly the behavior `tamias` shipped at v1, extracted behind the
//! [`IdentitySource`] port. It stays the default for headless and
//! CI: `TAMIAS_IDENTITY_FILE` (or `[vaults.*].identity` in config)
//! pins an explicit path and skips the keyring entirely.
//!
//! The keyfile is raw key material — protect it like a private
//! SSH key. The keyring impl (`tamias-identity-keyring`) is the
//! preferred source when a session secret store exists.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use age::x25519;
use tamias_identity::{IdentityError, IdentitySource};
use time::OffsetDateTime;

/// An age identity stored as a keyfile on disk.
pub struct FileIdentity {
    path: PathBuf,
}

impl FileIdentity {
    /// A keyfile at an explicit path — `TAMIAS_IDENTITY_FILE` or a
    /// `[vaults.*].identity` config value.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileIdentity { path: path.into() }
    }

    /// The default keyfile — `$TAMIAS_IDENTITY_FILE`, else
    /// `$XDG_CONFIG_HOME/tamias/identity.txt`, else
    /// `~/.config/tamias/identity.txt`.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("TAMIAS_IDENTITY_FILE") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        config_home().join("tamias/identity.txt")
    }

    /// Does the keyfile exist on disk?
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// The keyfile path this source wraps.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for FileIdentity {
    /// The [`Self::default_path`] keyfile.
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

impl IdentitySource for FileIdentity {
    /// Read + validate the keyfile: first non-empty, non-`#`
    /// comment line must be an `AGE-SECRET-KEY-…` string. Missing
    /// file → `Ok(None)`; present-but-unparseable → `Err(Parse)`
    /// (never silently treated as empty — that would risk
    /// clobbering a good key on `store`).
    fn load(&self) -> Result<Option<x25519::Identity>, IdentityError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(IdentityError::Io(format!(
                    "read identity {}: {e}",
                    self.path.display()
                )))
            }
        };
        // F3: the keyfile is raw key material — a group/world-
        // readable file is a stderr note, never a load failure
        // (ssh's strict-modes posture, non-fatal).
        warn_perms(&self.path);
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .ok_or_else(|| {
                IdentityError::Parse(format!("identity {} holds no key", self.path.display()))
            })?;
        x25519::Identity::from_str(line)
            .map(Some)
            .map_err(|e| IdentityError::Parse(format!("identity {}: {e}", self.path.display())))
    }

    /// Write the keyfile (mode `0600`, parents created), overwrite
    /// allowed — no-clobber semantics live in
    /// [`tamias_identity::ensure`]. The public recipient goes in a `#`
    /// comment line so the file stays rage-compatible.
    #[allow(clippy::unwrap_used)]
    fn store(&self, identity: &x25519::Identity) -> Result<(), IdentityError> {
        use age::secrecy::ExposeSecret;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let key = identity.to_string();
        let body = format!(
            "# tamias identity — created {}\n# public: {}\n{}\n",
            OffsetDateTime::now_utc(),
            identity.to_public(),
            key.expose_secret(),
        );
        // Create 0600 at open() — a write+chmod sequence leaves the
        // key material world/group-readable for a window (L10).
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        opts.open(&self.path)
            .and_then(|mut f| f.write_all(body.as_bytes()))?;
        // chmod'ing a file this process just created and owns
        // cannot realistically fail — an expect is honest here
        // where a `?` would imply a reachable error arm.
        set_private(&self.path).expect("chmod on a freshly written file");
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "file"
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// `~` for path defaults.
fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
}

/// The warning text when the keyfile is more permissive than
/// `0600` — `None` when it's strict enough or missing (F3).
/// Returned (not printed) so tests can assert on it.
#[cfg(unix)]
fn perm_note(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::symlink_metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & !0o600 != 0).then(|| {
        format!(
            "identity file {} is group/world-accessible (mode {mode:o}) — \
             run `chmod 600 {}`",
            path.display(),
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn perm_note(_path: &Path) -> Option<String> {
    None
}

/// F3 stderr note — a loose keyfile permission warns, never fails.
fn warn_perms(path: &Path) {
    if let Some(note) = perm_note(path) {
        eprintln!("tamias: {note}");
    }
}

/// `chmod 0600` — the keyfile holds raw key material.
#[cfg(unix)]
fn set_private(path: &Path) -> Result<(), IdentityError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<(), IdentityError> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn store_then_load_roundtrips_0600() {
        let dir = tempfile::tempdir().unwrap();
        let src = FileIdentity::new(dir.path().join("identity.txt"));
        assert!(src.load().unwrap().is_none());
        let (id, created) = tamias_identity::ensure(&src).unwrap();
        assert!(created);
        // idempotent — second ensure loads, never clobbers
        let (again, created) = tamias_identity::ensure(&src).unwrap();
        assert!(!created);
        assert_eq!(id.to_public(), again.to_public());
        // the file documents its own public key, rage-compatible
        let text = std::fs::read_to_string(src.path()).unwrap();
        assert!(text.contains(&id.to_public().to_string()));
        assert!(text.contains("AGE-SECRET-KEY-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(src.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_loose_keyfile_warns_but_still_loads() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = FileIdentity::new(dir.path().join("id.txt"));
        let (_id, created) = tamias_identity::ensure(&src).unwrap();
        assert!(created);
        // F3: 0644 warns on stderr but the load still succeeds
        std::fs::set_permissions(src.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let note = perm_note(src.path()).unwrap();
        assert!(note.contains("644"), "{note}");
        assert!(src.load().unwrap().is_some());
        // strict and missing paths are quiet
        std::fs::set_permissions(src.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(perm_note(src.path()).is_none());
        assert!(perm_note(&dir.path().join("nope")).is_none());
    }

    #[test]
    fn load_skips_comment_and_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let src = FileIdentity::new(dir.path().join("id.txt"));
        use age::secrecy::ExposeSecret;
        let id = x25519::Identity::generate();
        std::fs::write(
            src.path(),
            format!("# comment first\n\n{}\n", id.to_string().expose_secret()),
        )
        .unwrap();
        assert_eq!(src.load().unwrap().unwrap().to_public(), id.to_public());
    }

    #[test]
    fn load_rejects_garbage_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        // missing → Ok(None), the fall-through signal
        let src = FileIdentity::new(dir.path().join("nope.txt"));
        assert!(src.load().unwrap().is_none());
        // comments only → Parse, never a silent None
        std::fs::write(src.path(), "# only comments\n\n").unwrap();
        assert!(matches!(src.load(), Err(IdentityError::Parse(_))));
        // a non-key line → Parse
        std::fs::write(src.path(), "not-a-key\n").unwrap();
        assert!(matches!(src.load(), Err(IdentityError::Parse(_))));
    }

    #[test]
    fn store_surfaces_write_failures() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let src = FileIdentity::new(blocker.join("sub/id.txt"));
        let id = x25519::Identity::generate();
        assert!(src.store(&id).is_err());
        // missing parents get created on demand
        let src = FileIdentity::new(dir.path().join("a/b/identity.txt"));
        src.store(&id).unwrap();
        assert!(src.exists());
    }

    #[test]
    fn store_surfaces_parent_dir_failures() {
        // the keyfile's parent exists as a *file* → create_dir_all
        // fails → the `?` inside store, not a panic
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let f = FileIdentity::new(blocker.join("id.txt"));
        let id = x25519::Identity::generate();
        assert!(matches!(f.store(&id), Err(IdentityError::Io(_))));
        // a parentless path ("/") skips create_dir_all entirely —
        // the write then fails on its own
        let root = FileIdentity::new("/");
        assert!(matches!(root.store(&id), Err(IdentityError::Io(_))));
    }

    #[test]
    fn set_private_on_missing_file_is_an_error() {
        assert!(set_private(Path::new("/nonexistent/x/y")).is_err());
    }

    #[test]
    fn the_source_names_and_defaults_itself() {
        let f = FileIdentity::new("/tmp/tamias-id-x");
        assert_eq!(f.kind(), "file");
        assert!(f.describe().contains("tamias-id-x"));
        assert!(!f.exists());
        assert_eq!(f.path(), Path::new("/tmp/tamias-id-x"));
        // Default wraps default_path — same path, no surprises
        temp_env::with_var("TAMIAS_IDENTITY_FILE", Some("/tmp/tamias-id-y"), || {
            assert_eq!(
                FileIdentity::default().path(),
                Path::new("/tmp/tamias-id-y")
            );
        });
    }

    #[test]
    fn load_surfaces_non_notfound_io_errors() {
        // a directory where the keyfile should be → read fails with
        // something other than NotFound → the Io arm, not Ok(None)
        let dir = tempfile::tempdir().unwrap();
        let f = FileIdentity::new(dir.path());
        assert!(f.exists());
        assert!(matches!(f.load(), Err(IdentityError::Io(_))));
    }

    #[test]
    fn default_path_follows_env_xdg_then_home() {
        // TAMIAS_IDENTITY_FILE wins outright
        temp_env::with_var(
            "TAMIAS_IDENTITY_FILE",
            Some("/tmp/tamias-id-override"),
            || {
                assert_eq!(
                    FileIdentity::default_path(),
                    PathBuf::from("/tmp/tamias-id-override")
                );
            },
        );
        // empty override is ignored; XDG drives the rest
        temp_env::with_vars(
            [
                ("TAMIAS_IDENTITY_FILE", Some("")),
                ("XDG_CONFIG_HOME", Some("/tmp/xdg-c")),
                ("HOME", Some("/tmp/fakehome")),
            ],
            || {
                assert_eq!(
                    FileIdentity::default_path(),
                    PathBuf::from("/tmp/xdg-c/tamias/identity.txt")
                );
            },
        );
        // no XDG → HOME-based fallback
        temp_env::with_vars(
            [
                ("TAMIAS_IDENTITY_FILE", None::<&str>),
                ("XDG_CONFIG_HOME", None::<&str>),
                ("HOME", Some("/tmp/fakehome")),
            ],
            || {
                assert_eq!(
                    FileIdentity::default_path(),
                    PathBuf::from("/tmp/fakehome/.config/tamias/identity.txt")
                );
            },
        );
        // no HOME either → "."
        temp_env::with_vars(
            [
                ("TAMIAS_IDENTITY_FILE", None::<&str>),
                ("XDG_CONFIG_HOME", None::<&str>),
                ("HOME", None::<&str>),
            ],
            || {
                assert_eq!(
                    FileIdentity::default_path(),
                    PathBuf::from("./.config/tamias/identity.txt")
                );
            },
        );
    }
}
