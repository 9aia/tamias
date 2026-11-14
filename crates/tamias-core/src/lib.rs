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
pub mod atomic;
pub mod digest;
pub mod error;
pub mod identity;
pub mod manifest;
pub mod overlay;
pub mod process;
pub mod project;
pub mod records;
pub mod sealed;
pub use archive::{
    pack_tgz, pack_tgz_atomic, untar, untar_gz, untar_gz_limited, untar_limited,
    DEFAULT_MAX_ENTRIES, DEFAULT_MAX_UNPACK_BYTES,
};
pub use atomic::write_atomic;
pub use digest::{content_digest, dir_digest, dir_size, file_digest, sha256_hex, short_digest};
pub use error::Error;
pub use identity::{Identity, IdentityError};
pub use manifest::{
    is_valid_keyword, is_valid_kind, is_valid_name, is_valid_version, BinDecl, Manifest, Provider,
    ProviderHealth, ProviderHook, ProviderIngress, ProviderService, MANIFEST_NAMES,
    MAX_KEYWORD_LEN,
};
pub use overlay::{find_store_entry, merge_values, overlay, payload_root};
pub use process::output_with_timeout;
pub use project::{Lock, LockEntry};
pub use records::{copy_tree, StoreEntry, StoreRecord, RECORD_FILE};
pub use sealed::{
    check_ciphertext_only, has_age_magic, is_age_ciphertext, Sealed, AGE_FORMAT, AGE_MAGIC,
};
