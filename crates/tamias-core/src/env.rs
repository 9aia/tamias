//! The env axis — `dev`/`staging`/`prod` plus arbitrary lowercase
//! names. Env is *resolution context* (`--env`/`TAMIAS_ENV`/`[envs]`),
//! not part of a ref's name — and the routing axis that maps an env
//! to a vault backend.

use std::fmt;
use std::str::FromStr;

/// An environment name — `dev`, `staging`, `prod`, or any lowercase
/// `[a-z][a-z0-9_-]*` slug.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Env(String);

impl Env {
    /// The well-known env names, always recognized (e.g. as an
    /// `tamias://prod/…` pin) even without `[envs]` config.
    pub const WELL_KNOWN: [&'static str; 3] = ["dev", "staging", "prod"];

    /// Validate and wrap. Rules: `[a-z][a-z0-9_-]*`, ≤ 64 chars.
    pub fn new(s: &str) -> Result<Self, ParseEnvError> {
        if is_valid_env(s) {
            Ok(Env(s.to_string()))
        } else {
            Err(ParseEnvError(s.to_string()))
        }
    }

    /// `dev`.
    pub fn dev() -> Self {
        Env("dev".into())
    }

    /// The raw name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// One of `dev`/`staging`/`prod`.
    pub fn is_well_known(&self) -> bool {
        Self::WELL_KNOWN.contains(&self.0.as_str())
    }
}

/// Is `s` a valid env name? See [`Env::new`].
pub fn is_valid_env(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_'))
}

/// Failure to validate an [`Env`] name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid env name {0:?}")]
pub struct ParseEnvError(pub String);

impl FromStr for Env {
    type Err = ParseEnvError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Env::new(s)
    }
}

impl From<Env> for String {
    fn from(e: Env) -> Self {
        e.0
    }
}

impl fmt::Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for Env {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Env {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Env {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Env".into()
    }

    fn json_schema(gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        String::json_schema(gen)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn valid() {
        for ok in ["dev", "staging", "prod", "ci", "eu-west-1", "a"] {
            assert!(is_valid_env(ok), "{ok}");
            assert_eq!(Env::new(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn invalid() {
        for bad in ["", "Dev", "PROD", "1x", "-x", "has space", "a/b"] {
            assert!(!is_valid_env(bad), "{bad}");
            assert!(bad.parse::<Env>().is_err(), "{bad}");
        }
    }

    #[test]
    fn well_known() {
        assert!(Env::dev().is_well_known());
        assert!(!Env::new("ci").unwrap().is_well_known());
        assert_eq!(Env::dev().to_string(), "dev");
    }

    #[test]
    fn serde_roundtrip() {
        let e = Env::new("prod").unwrap();
        assert_eq!(serde_json::to_string(&e).unwrap(), "\"prod\"");
        assert_eq!(serde_json::from_str::<Env>("\"prod\"").unwrap(), e);
        assert!(serde_json::from_str::<Env>("\"PROD\"").is_err());
        // a non-string fails before validation is even consulted
        assert!(serde_json::from_str::<Env>("42").is_err());
    }

    #[test]
    fn into_string_gives_back_the_name() {
        let e = Env::new("staging").unwrap();
        assert_eq!(String::from(e), "staging");
        assert_eq!(Env::dev().to_string(), "dev");
    }
}
