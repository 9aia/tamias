#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! tamias-core — pure domain + the bundle engine for the package/content
//! manager.
//!
//! I/O-free types: package **identity** (`@scope/name@semver`), the **ref**
//! grammar, **manifests**, install **records**, and **digests**. Plus the
//! artifact engine the store and sources share: tar pack/unpack, the
//! `include:` deep-merge overlay, and payload walking. Ports and adapters
//! live in the sibling crates.

pub mod archive;
pub mod digest;
pub mod error;
pub mod identity;
pub mod manifest;
pub mod overlay;
pub mod project;
pub mod records;
pub mod refs;

pub use archive::{pack_tgz, untar, untar_gz};
pub use digest::{content_digest, dir_digest, dir_size, file_digest, sha256_hex};
pub use error::Error;
pub use identity::{Identity, IdentityError};
pub use manifest::{Manifest, MANIFEST_NAMES};
pub use overlay::{overlay, payload_root};
pub use project::{Lock, LockEntry};
pub use records::{copy_tree, StoreEntry, StoreRecord, RECORD_FILE};
pub use refs::{expand_path, parse_ref, GitRef, HttpRef, RefError, SourceRef};
