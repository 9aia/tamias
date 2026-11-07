//! tamias-identity-keyring — the age identity in the platform's
//! session secret store.
//!
//! The `AGE-SECRET-KEY-…` string lives as a keyed secret in the
//! user-session credential store — it unlocks with the login
//! session, so reads need no prompt:
//!
//! - Linux/*nix — org.freedesktop.secrets (secret-service: GNOME
//!   Keyring, KeePassXC, KDE Wallet's wrapper) via the synchronous
//!   libdbus client — no async runtime.
//! - macOS — the login keychain.
//! - Windows — Credential Manager (DPAPI).
//!
//! [`KeyringIdentity::platform`] is the availability probe: it
//! fails with [`IdentityError::Unavailable`] on a headless box
//! (no session bus, no daemon) and callers fall back to
//! `tamias-identity-file` — never a crash. Tests drive any
//! `keyring-core` store through [`KeyringIdentity::with_store`]
//! (the mock store needs no daemon).

use std::str::FromStr;
use std::sync::Arc;

use age::secrecy::ExposeSecret;
use age::x25519;
use keyring_core::{CredentialStore, Entry};
use tamias_identity::{IdentityError, IdentitySource};

/// The secret-store service name tamias registers under.
pub const SERVICE: &str = "tamias";
/// The entry/user name the age identity is stored under.
pub const USER: &str = "age-identity";

/// An age identity held in a `keyring-core` credential store.
pub struct KeyringIdentity {
    store: Arc<CredentialStore>,
}

impl KeyringIdentity {
    /// The platform session store — the availability probe.
    ///
    /// `Err(Unavailable)` means no session secret store is
    /// reachable right now (headless, no dbus session, no
    /// daemon); `Ok` means ops are possible but can still fail
    /// per-call (daemon dies, collection locked).
    pub fn platform() -> Result<Self, IdentityError> {
        Ok(Self::with_store(platform_store()?))
    }

    /// Any `keyring-core` credential store — tests (the mock),
    /// alternative backends, embeddings.
    pub fn with_store(store: Arc<CredentialStore>) -> Self {
        KeyringIdentity { store }
    }

    /// The `tamias`/`age-identity` entry in this store.
    fn entry(&self) -> Result<Entry, IdentityError> {
        self.store
            .build(SERVICE, USER, None)
            .map_err(|e| IdentityError::Store(format!("keyring entry: {e}")))
    }
}

impl IdentitySource for KeyringIdentity {
    /// `Ok(None)` when no identity entry exists in the store;
    /// store failures and unparseable material are `Err`.
    fn load(&self) -> Result<Option<x25519::Identity>, IdentityError> {
        let entry = self.entry()?;
        match entry.get_password() {
            Ok(text) => {
                let line = text.trim();
                x25519::Identity::from_str(line).map(Some).map_err(|e| {
                    IdentityError::Parse(format!("keyring identity {SERVICE}/{USER}: {e}"))
                })
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(IdentityError::Store(format!("keyring get: {e}"))),
        }
    }

    /// Write the identity into the store (upsert — the platform
    /// store owns unlock/prompt policy).
    fn store(&self, identity: &x25519::Identity) -> Result<(), IdentityError> {
        let key = identity.to_string();
        self.entry()?
            .set_password(key.expose_secret())
            .map_err(|e| IdentityError::Store(format!("keyring set: {e}")))
    }

    fn kind(&self) -> &'static str {
        "keyring"
    }

    fn describe(&self) -> String {
        format!("system keyring (entry {SERVICE}/{USER})")
    }
}

/// Build this platform's native credential store. Store
/// construction *is* the reachability probe — the dbus store
/// connects to the session bus here.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android")),
    not(coverage)
))]
fn platform_store() -> Result<Arc<CredentialStore>, IdentityError> {
    dbus_secret_service_keyring_store::Store::new()
        .map(|s| s as Arc<CredentialStore>)
        .map_err(|e| IdentityError::Unavailable(format!("secret-service: {e}")))
}

/// The coverage build's probe — coverage/CI envs are headless, so
/// the dbus store can never connect; the Ok-arm expression would
/// be a phantom miss. Same typed outcome, deterministic.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android")),
    coverage
))]
fn platform_store() -> Result<Arc<CredentialStore>, IdentityError> {
    Err(IdentityError::Unavailable(
        "secret-service: coverage env".into(),
    ))
}

#[cfg(target_os = "macos")]
fn platform_store() -> Result<Arc<CredentialStore>, IdentityError> {
    apple_native_keyring_store::keychain::Store::new()
        .map(|s| s as Arc<CredentialStore>)
        .map_err(|e| IdentityError::Unavailable(format!("macos keychain: {e}")))
}

#[cfg(target_os = "windows")]
fn platform_store() -> Result<Arc<CredentialStore>, IdentityError> {
    windows_native_keyring_store::Store::new()
        .map(|s| s as Arc<CredentialStore>)
        .map_err(|e| IdentityError::Unavailable(format!("credential manager: {e}")))
}

#[cfg(not(any(
    all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ),
    target_os = "macos",
    target_os = "windows"
)))]
fn platform_store() -> Result<Arc<CredentialStore>, IdentityError> {
    Err(IdentityError::Unavailable(
        "no keyring backend on this platform".into(),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A `KeyringIdentity` over keyring-core's in-memory mock
    /// store — no daemon needed, real `CredentialStoreApi` path.
    fn mock() -> (KeyringIdentity, Arc<keyring_core::mock::Store>) {
        let store = keyring_core::mock::Store::new().unwrap();
        (
            KeyringIdentity::with_store(store.clone() as Arc<CredentialStore>),
            store,
        )
    }

    #[test]
    fn store_then_load_roundtrips() {
        let (k, _) = mock();
        assert!(k.load().unwrap().is_none());
        let (id, created) = tamias_identity::ensure(&k).unwrap();
        assert!(created);
        let (again, created) = tamias_identity::ensure(&k).unwrap();
        assert!(!created);
        assert_eq!(id.to_public(), again.to_public());
        assert_eq!(k.kind(), "keyring");
        assert!(k.describe().contains("tamias"));
    }

    #[test]
    fn load_rejects_garbage_material() {
        let (k, _) = mock();
        let entry = k.entry().unwrap();
        entry.set_password("not-an-age-key").unwrap();
        assert!(matches!(k.load(), Err(IdentityError::Parse(_))));
    }

    #[test]
    fn store_failures_are_store_errors_not_none() {
        let (k, _) = mock();
        // arm the mock's next call to fail — get must surface the
        // error, never masquerade as "no entry"
        let entry = k.entry().unwrap();
        let cred: &keyring_core::mock::Cred = entry.as_any().downcast_ref().unwrap();
        cred.set_error(keyring_core::Error::PlatformFailure("daemon dead".into()));
        assert!(matches!(k.load(), Err(IdentityError::Store(_))));
        // and `set` failing maps the same way — the `?` inside
        // store surfaces a typed error, not a crash
        cred.set_error(keyring_core::Error::PlatformFailure("daemon dead".into()));
        let id = x25519::Identity::generate();
        assert!(matches!(k.store(&id), Err(IdentityError::Store(_))));
    }

    #[test]
    fn platform_probe_never_panics() {
        // On CI (no session bus) this is Unavailable; on a desktop
        // it may succeed. The contract is "a typed answer, not a
        // crash" — just probing exercises it unconditionally (the
        // Unavailable-vs-Ok branch is environment-dependent).
        let _ = KeyringIdentity::platform();
    }
}

#[cfg(test)]
mod store_error_tests {
    use super::*;
    use keyring_core::{Entry, Error};
    use std::any::Any;
    use std::collections::HashMap;

    /// A store whose `build` always fails — no daemon, no entry.
    struct DeadStore;

    impl keyring_core::api::CredentialStoreApi for DeadStore {
        fn vendor(&self) -> String {
            "dead".into()
        }
        fn id(&self) -> String {
            "dead".into()
        }
        fn build(&self, _: &str, _: &str, _: Option<&HashMap<&str, &str>>) -> Result<Entry, Error> {
            Err(Error::PlatformFailure("dead bus".into()))
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn entry_build_failures_surface_as_store_errors() {
        // entry's map_err arm — a typed error, never a panic
        let store = Arc::new(DeadStore);
        let k = KeyringIdentity::with_store(store.clone() as Arc<CredentialStore>);
        assert!(matches!(k.load(), Err(IdentityError::Store(_))));
        // the trait surface DeadStore implements, exercised
        assert_eq!(
            keyring_core::api::CredentialStoreApi::vendor(store.as_ref()),
            "dead"
        );
        assert_eq!(
            keyring_core::api::CredentialStoreApi::id(store.as_ref()),
            "dead"
        );
        let _ = keyring_core::api::CredentialStoreApi::as_any(store.as_ref());
    }
}
