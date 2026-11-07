//! tamias-vault — the readable-store port.
//!
//! A [`Vault`] is a store of [`SecretRecord`]s keyed by
//! `(env, ref-path)`. Every impl stores opaque ciphertext — stores
//! never see plaintext (ADR-0001); impls are `tamias-vault-age`
//! (local file, the default), `tamias-vault-cf` (self-hosted
//! Cloudflare Worker + D1), and `tamias-vault-cached` (read-through
//! encrypted local cache in front of any backend, P-007). The
//! trait is sync by design — no async runtime in the binary; an
//! HTTP-backed impl uses a blocking client.
//!
//! The contract below is conformance-tested: every impl runs the
//! `tamias-testkit` suite and must agree on semantics —
//!
//! - `put` — upsert. New ref → `version = 1`, `created_at = now`,
//!   `rotated_at = None`. Existing ref → value replaced, `version`
//!   bumps, `created_at` preserved, `rotated_at` untouched.
//! - `get` — `Ok(None)` when the ref isn't stored in that env.
//! - `rm` — `Ok(true)` removed, `Ok(false)` it wasn't there.
//! - `list` — the refs stored in that env, sorted.
//! - `rotate` — deliberate rotation: `Err(NotFound)` on a missing
//!   ref, else new value + `version` bump + `rotated_at = now`.
//! - envs are isolated keyspaces — same ref path in `dev` and
//!   `prod` are independent records.

use tamias_core::{Env, Ref, Secret};
use time::OffsetDateTime;

/// A stored secret plus its bookkeeping.
#[derive(Debug, Clone)]
pub struct SecretRecord {
    /// The value — redacting [`Secret`]; `.expose()` is the only
    /// way out, at edges only.
    pub value: Secret,
    /// Write generation: `1` on first put, +1 per put/rotate.
    pub version: u64,
    /// When the record was first stored.
    pub created_at: OffsetDateTime,
    /// Last deliberate rotation (`rotate`), if ever.
    pub rotated_at: Option<OffsetDateTime>,
}

/// A readable secret store.
pub trait Vault: Send + Sync {
    /// Fetch the record for `rref` in `env`, or `None`.
    fn get(&self, env: &Env, rref: &Ref) -> Result<Option<SecretRecord>, VaultError>;

    /// Upsert `value` at `rref` in `env`; returns the stored record.
    fn put(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError>;

    /// Remove `rref` from `env`; `true` when something was removed.
    fn rm(&self, env: &Env, rref: &Ref) -> Result<bool, VaultError>;

    /// All refs stored in `env`, sorted.
    fn list(&self, env: &Env) -> Result<Vec<Ref>, VaultError>;

    /// Rotate `rref` in `env` to a new value — `version` bump +
    /// `rotated_at`. [`VaultError::NotFound`] on a missing ref.
    fn rotate(&self, env: &Env, rref: &Ref, value: Secret) -> Result<SecretRecord, VaultError>;
}

/// Vault port errors. Impls map their own failure modes onto these
/// variants so callers can match without knowing the backend.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// The ref isn't stored in that env (rotate of a missing ref).
    #[error("no secret at tamias://{rref} in env {env}")]
    NotFound {
        /// The bare ref path.
        rref: String,
        /// The env name.
        env: String,
    },
    /// Filesystem / network I/O failure.
    #[error("vault i/o: {0}")]
    Io(String),
    /// Encryption/decryption failure (bad identity, corrupt file).
    #[error("vault crypto: {0}")]
    Crypto(String),
    /// Misconfigured backend (bad path, missing identity file).
    #[error("vault config: {0}")]
    Config(String),
    /// Anything else the backend wants to say.
    #[error("vault backend: {0}")]
    Backend(String),
}

impl From<std::io::Error> for VaultError {
    fn from(e: std::io::Error) -> Self {
        VaultError::Io(e.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn io_error_maps_to_the_io_variant() {
        let e = VaultError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(
            matches!(e, VaultError::Io(ref s) if s.contains("gone")),
            "{e}"
        );
    }
}
