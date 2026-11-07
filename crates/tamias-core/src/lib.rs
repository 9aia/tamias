//! tamias-core — pure domain for the secrets broker: `tamias://` refs,
//! the redacting [`Secret`] newtype, the env axis (dev/staging/prod +
//! arbitrary names), actor attribution, the op vocabulary, and the
//! `tamias.toml` config model. No I/O, no ports — vaults, config
//! sources, and the audit wire live in their own crates.

pub mod actor;
pub mod config;
pub mod env;
pub mod op;
pub mod refs;
pub mod secret;

pub use actor::{is_valid_actor, Actor, ParseActorError};
pub use config::{
    CacheScope, Config, DeployConfig, EnvRoute, EnvRouteTable, GuardDefault, Guardrails, PdpConfig,
    ServeConfig, VaultConfig,
};
pub use env::{is_valid_env, Env, ParseEnvError};
pub use op::{Op, ParseOpError};
pub use refs::{is_valid_segment, ParseRefError, Ref};
pub use secret::Secret;

/// Env var names for `[secrets]` / `-s VAR=tamias://ref` — the
/// conventional `^[A-Za-z_][A-Za-z0-9_]*$` shell identifier.
pub fn is_valid_env_var(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn env_var_names() {
        for ok in ["STRIPE_KEY", "_x", "A1", "lower_ok"] {
            assert!(is_valid_env_var(ok), "{ok}");
        }
        for bad in ["", "1X", "HAS-DASH", "has space", "A.B"] {
            assert!(!is_valid_env_var(bad), "{bad}");
        }
    }
}
