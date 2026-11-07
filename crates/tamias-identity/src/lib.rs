//! tamias-identity — the identity port: *where the age identity
//! comes from*.
//!
//! The age identity is the master key every vault impl decrypts
//! with (ADR-0001: all stores hold opaque age ciphertext). How that
//! key material is held is a port — same convention as
//! `tamias-vault`/`tamias-config`/`tamias-deploy`:
//!
//! - `tamias-identity-file` — the `~/.config/tamias/identity.txt`
//!   keyfile (mode `0600`); the headless/CI path, overridable via
//!   `TAMIAS_IDENTITY_FILE` or `[vaults.*].identity`.
//! - `tamias-identity-keyring` — the platform session secret store
//!   (secret-service / Keychain / Credential Manager) via the
//!   keyring-core `CredentialStore` model.
//!
//! Resolution order (in `tamias-ops`): an explicit file override
//! wins; else the session keyring is preferred; else the default
//! keyfile. A later arm of this port — a passphrase-wrapped
//! identity file for portable identities — is a new impl, not a
//! new port.

use age::x25519;

/// A source of (and sink for) the agent's age identity.
///
/// Implementations hold raw `AGE-SECRET-KEY-…` material however
/// their backend prefers; the port speaks parsed
/// [`x25519::Identity`]s so vault impls never parse text. All
/// methods are sync — no async runtime in the binary.
pub trait IdentitySource: Send + Sync {
    /// Load the stored identity. `Ok(None)` means *this source
    /// holds nothing* — distinct from an error, so callers can
    /// fall through to the next source. Unreadable or corrupt
    /// material is `Err`, never silently `None` (that would risk
    /// clobbering a good identity on `store`).
    fn load(&self) -> Result<Option<x25519::Identity>, IdentityError>;

    /// Persist `identity`, overwriting any existing entry. Callers
    /// wanting no-clobber semantics check [`load`](Self::load)
    /// first (see [`ensure`]).
    fn store(&self, identity: &x25519::Identity) -> Result<(), IdentityError>;

    /// Short backend name for messages — `"file"`, `"keyring"`.
    fn kind(&self) -> &'static str;

    /// Human-facing locator — a keyfile path or a store entry name.
    /// Never contains key material.
    fn describe(&self) -> String;
}

/// Load-or-generate against one source: `Ok((identity, created))`.
/// The `init`/bootstrap path — generates a fresh identity iff the
/// source holds none, then stores it. A corrupt entry is an error,
/// not an overwrite.
pub fn ensure(source: &dyn IdentitySource) -> Result<(x25519::Identity, bool), IdentityError> {
    match source.load()? {
        Some(id) => Ok((id, false)),
        None => {
            let id = x25519::Identity::generate();
            source.store(&id)?;
            Ok((id, true))
        }
    }
}

/// Identity port errors. Impls map backend failures onto these so
/// callers can match without knowing the store.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// Filesystem I/O failure.
    #[error("identity i/o: {0}")]
    Io(String),
    /// Stored material isn't a usable age identity.
    #[error("identity parse: {0}")]
    Parse(String),
    /// The backend exists but the read/write failed (daemon down,
    /// access denied, session locked).
    #[error("identity store: {0}")]
    Store(String),
    /// The backend itself doesn't exist on this host/session —
    /// e.g. no reachable session keyring on a headless box.
    #[error("identity backend unavailable: {0}")]
    Unavailable(String),
}

impl From<std::io::Error> for IdentityError {
    fn from(e: std::io::Error) -> Self {
        IdentityError::Io(e.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An in-memory `IdentitySource` exercising the port contract.
    struct Mem {
        cell: Mutex<Option<x25519::Identity>>,
        fail: Mutex<Option<IdentityError>>,
    }

    impl Mem {
        fn new() -> Self {
            Mem {
                cell: Mutex::new(None),
                fail: Mutex::new(None),
            }
        }

        fn failing(e: IdentityError) -> Self {
            Mem {
                cell: Mutex::new(None),
                fail: Mutex::new(Some(e)),
            }
        }
    }

    impl IdentitySource for Mem {
        fn load(&self) -> Result<Option<x25519::Identity>, IdentityError> {
            let cell = self.cell.lock().unwrap().clone();
            Ok(cell)
        }

        fn store(&self, identity: &x25519::Identity) -> Result<(), IdentityError> {
            if let Some(e) = self.fail.lock().unwrap().take() {
                return Err(e);
            }
            *self.cell.lock().unwrap() = Some(identity.clone());
            Ok(())
        }

        fn kind(&self) -> &'static str {
            "mem"
        }

        fn describe(&self) -> String {
            "in-memory test source".into()
        }
    }

    #[test]
    fn ensure_generates_once_then_loads() {
        let mem = Mem::new();
        let (a, created) = ensure(&mem).unwrap();
        assert!(created);
        let (b, created) = ensure(&mem).unwrap();
        assert!(!created);
        assert_eq!(a.to_public(), b.to_public());
    }

    #[test]
    fn ensure_propagates_store_failures() {
        let mem = Mem::failing(IdentityError::Store("backend dead".into()));
        // `.err()` is infallible here — a failing store must not
        // yield an identity, so no dead panic arm.
        let e = ensure(&mem).err().unwrap();
        assert!(e.to_string().contains("backend dead"), "{e}");
    }

    #[test]
    fn mem_names_itself_like_any_source() {
        let mem = Mem::new();
        assert_eq!(mem.kind(), "mem");
        assert_eq!(mem.describe(), "in-memory test source");
    }

    #[test]
    fn io_error_maps_to_the_io_variant() {
        let e = IdentityError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(
            matches!(e, IdentityError::Io(ref s) if s.contains("gone")),
            "{e}"
        );
    }
}
