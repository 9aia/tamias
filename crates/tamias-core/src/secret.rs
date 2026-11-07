//! The `Secret` newtype — a plaintext value that redacts itself.
//!
//! `Debug`/`Display` print `Secret(***)`/`***`, never the value; the
//! only way out is the explicit [`.expose()`](Secret::expose) call,
//! which greps loud in review. No serde impls — serialization of a
//! value (e.g. the vault payload) goes through `.expose()` at the
//! edge, deliberately. The inner string is [`zeroize::Zeroizing`] so
//! drops scrub the buffer.

use std::fmt;

use zeroize::Zeroizing;

/// A secret value. Construction is free; extraction is explicit.
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wrap a plaintext value.
    pub fn new(value: impl Into<String>) -> Self {
        Secret(Zeroizing::new(value.into()))
    }

    /// The plaintext — the *only* way out, and deliberately named so
    /// every leak surface is an `expose()` call site.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Length of the plaintext in bytes — safe metadata for UIs.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Is the plaintext empty?
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.expose() == other.expose()
    }
}
impl Eq for Secret {}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn redacts_everything_but_expose() {
        let s = Secret::new("fake-test-value");
        assert_eq!(s.expose(), "fake-test-value");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(format!("{s}"), "***");
        assert!(!format!("{s:?}").contains("fake"));
        assert_eq!(s.len(), 15);
        assert!(!s.is_empty());
        assert!(Secret::new("").is_empty());
    }

    #[test]
    fn equality_uses_value() {
        assert_eq!(Secret::new("x"), Secret::new("x".to_string()));
        assert_ne!(Secret::new("x"), Secret::new("y"));
    }
}
