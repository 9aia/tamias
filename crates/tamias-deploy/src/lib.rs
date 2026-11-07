//! tamias-deploy — the write-only push port.
//!
//! Deploy targets are **sinks, not sources**. Some stores accept a
//! value and never give it back — Cloudflare's account Secrets
//! Store is the motivating case: `POST`/`PATCH` a value in and the
//! API literally cannot return it. A [`Deploy`] therefore has
//! `push`/`remove` and deliberately no `get`/`list` — anything
//! readable belongs behind the [`tamias_vault::Vault`] port instead.
//!
//! The value pushed is **plaintext** (decrypted at the edge by the
//! caller): the sink is the thing Workers/CI read the secret from
//! directly — the age-ciphertext boundary lives at tamias's own
//! vaults, not at external secret stores that must serve values to
//! runtimes that can't hold an tamias identity.
//!
//! Naming: impls map `(env, ref)` onto their sink's name scheme;
//! [`PushReport::name`] reports what the value was written as.
//!
//! Sync by design — no async runtime; impls use blocking HTTP.

use tamias_core::{Env, Ref, Secret};

/// What a successful [`Deploy::push`] did at the sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushReport {
    /// The sink-side name the value was written under (e.g.
    /// `prod_billing_stripe`).
    pub name: String,
    /// `true` when the sink created a new entry; `false` when it
    /// updated an existing one.
    pub created: bool,
}

/// A write-only secret sink.
pub trait Deploy: Send + Sync {
    /// The configured target name (`[deploy.<name>]`) — for
    /// messages and audit context.
    fn name(&self) -> &str;

    /// Write `value` at the sink under the name derived from
    /// `(env, rref)`, creating or updating as needed.
    fn push(&self, env: &Env, rref: &Ref, value: &Secret) -> Result<PushReport, DeployError>;

    /// Delete the sink entry for `(env, rref)`; `true` when
    /// something was removed.
    fn remove(&self, env: &Env, rref: &Ref) -> Result<bool, DeployError>;
}

/// Deploy port errors — impls map their own failure modes onto
/// these so callers match without knowing the sink.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    /// Misconfigured target (missing account/store id, unset token
    /// env var).
    #[error("deploy config: {0}")]
    Config(String),
    /// Network/transport failure reaching the sink API.
    #[error("deploy i/o: {0}")]
    Io(String),
    /// The sink refused the credential (HTTP 401/403).
    #[error("deploy unauthorized: {0}")]
    Unauthorized(String),
    /// Anything else the sink wants to say.
    #[error("deploy backend: {0}")]
    Backend(String),
}
