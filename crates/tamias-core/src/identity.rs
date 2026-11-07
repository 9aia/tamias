//! Package identity — `[@scope/]name@version`.
//!
//! A **scope** namespaces a package (npm-style `@9aia/ci-gate`) so two
//! orgs can publish the same `name` without clashing. The **version** is
//! semver (the `semver` crate). The identity is what the store path, the
//! registry key, and the lockfile all key on.

use std::fmt;

use semver::Version;
use serde::{Deserialize, Serialize};

/// A package identity: optional scope, name, and semver version.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    /// Namespace, without the leading `@` (`9aia` → `@9aia/…`).
    pub scope: Option<String>,
    /// The bare package name (`ci-gate`).
    pub name: String,
    /// Semver version.
    pub version: Version,
}

/// Identity parse/validation failures.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The input isn't `[@scope/]name@version`.
    #[error("invalid identity '{0}': expected [@scope/]name@version")]
    Invalid(String),
    /// The name/scope is empty or has a bad shape.
    #[error("invalid {what} '{value}' in identity")]
    Segment {
        /// `scope` or `name`.
        what: &'static str,
        /// The offending value.
        value: String,
    },
    /// The version isn't valid semver.
    #[error("invalid version '{0}'")]
    Version(String),
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Identity {
    /// Build from parts, validating the name segments and semver.
    ///
    /// # Errors
    /// Returns [`IdentityError`] on a bad name/scope/version.
    pub fn new(scope: Option<String>, name: &str, version: &str) -> Result<Self, IdentityError> {
        if !valid_segment(name) {
            return Err(IdentityError::Segment {
                what: "name",
                value: name.to_string(),
            });
        }
        if let Some(scope) = &scope {
            if !valid_segment(scope) {
                return Err(IdentityError::Segment {
                    what: "scope",
                    value: scope.clone(),
                });
            }
        }
        let version =
            Version::parse(version).map_err(|_| IdentityError::Version(version.to_string()))?;
        Ok(Self {
            scope,
            name: name.to_string(),
            version,
        })
    }

    /// Parse `[@scope/]name@version`.
    ///
    /// # Errors
    /// Returns [`IdentityError`] when the shape or version is invalid.
    pub fn parse(input: &str) -> Result<Self, IdentityError> {
        let input = input.trim();
        let (scope, rest) = if let Some(r) = input.strip_prefix('@') {
            let (scope, rest) = r
                .split_once('/')
                .ok_or_else(|| IdentityError::Invalid(input.to_string()))?;
            (Some(scope.to_string()), rest)
        } else {
            (None, input)
        };
        let (name, version) = rest
            .rsplit_once('@')
            .ok_or_else(|| IdentityError::Invalid(input.to_string()))?;
        Self::new(scope, name, version)
    }

    /// `@scope/name` (no version) — the store dir / registry namespace.
    #[must_use]
    pub fn scoped(&self) -> String {
        match &self.scope {
            Some(scope) => format!("@{scope}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.scoped(), self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scoped() {
        let id = Identity::parse("@9aia/ci-gate@0.2.0").unwrap();
        assert_eq!(id.scope.as_deref(), Some("9aia"));
        assert_eq!(id.name, "ci-gate");
        assert_eq!(id.version.to_string(), "0.2.0");
        assert_eq!(id.scoped(), "@9aia/ci-gate");
        assert_eq!(id.to_string(), "@9aia/ci-gate@0.2.0");
    }

    #[test]
    fn parse_unscoped() {
        let id = Identity::parse("ci-gate@1.0.0").unwrap();
        assert_eq!(id.scope, None);
        assert_eq!(id.name, "ci-gate");
        assert_eq!(id.to_string(), "ci-gate@1.0.0");
    }

    #[test]
    fn rejects_bad() {
        assert!(Identity::parse("ci-gate").is_err()); // no version
        assert!(Identity::parse("@9aia@1.0.0").is_err()); // scope needs /
        assert!(Identity::parse("name@nope").is_err()); // bad semver
        assert!(Identity::parse("@/x@1.0.0").is_err()); // empty scope
    }

    #[test]
    fn roundtrip() {
        let id = Identity::new(Some("acme".into()), "pack", "2.3.4").unwrap();
        assert_eq!(Identity::parse(&id.to_string()).unwrap(), id);
    }
}
