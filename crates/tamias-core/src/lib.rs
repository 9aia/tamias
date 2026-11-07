#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-core — pure domain for the package/content manager.
//!
//! I/O-free model: package **identity** (`@scope/name@semver`), the **ref**
//! grammar, **manifests**, install **records**, and **digests**. Ports and
//! adapters live in the sibling crates.

pub mod digest;
pub mod identity;
pub mod refs;

pub use digest::{file_digest, sha256_hex};
pub use identity::{Identity, IdentityError};
pub use refs::{expand_path, parse_ref, GitRef, HttpRef, RefError, SourceRef};
