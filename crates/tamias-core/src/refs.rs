//! `tamias://` references — how code and config name a logical secret
//! without carrying its value.
//!
//! `tamias://billing/stripe` names the secret `billing/stripe`; the env
//! (`dev`/`prod`/…) is *resolution context*, selected by
//! `--env`/`TAMIAS_ENV`/`[envs]` — never part of the name. The one
//! exception is an explicit pin: when the first path segment is a
//! known env name (`tamias://prod/billing/stripe`), it overrides the
//! context for cross-env access. The ref itself stays pure — which
//! names count as envs is decided by the caller via [`Ref::env_pin`];
//! config-aware resolution lives in `tamias-ops`.

use std::fmt;
use std::str::FromStr;

use crate::Env;

/// The URI scheme every ref carries.
pub const SCHEME: &str = "tamias://";

/// A parsed `tamias://` reference — a non-empty path of safe segments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ref {
    segments: Vec<String>,
}

impl Ref {
    /// Parse `tamias://seg/seg/…`. The scheme is required; each segment
    /// must satisfy [`is_valid_segment`].
    pub fn new(s: &str) -> Result<Self, ParseRefError> {
        let rest = s
            .strip_prefix(SCHEME)
            .ok_or_else(|| ParseRefError(format!("{s:?} — missing {SCHEME} scheme")))?;
        Self::from_path_str(rest).map_err(|_| ParseRefError(format!("{s:?} — invalid ref path")))
    }

    /// Parse a bare `seg/seg/…` path (no scheme) — how vaults store
    /// keys.
    pub fn from_path(path: &str) -> Result<Self, ParseRefError> {
        Self::from_path_str(path).map_err(|_| ParseRefError(format!("{path:?} — invalid ref path")))
    }

    fn from_path_str(path: &str) -> Result<Self, ()> {
        let segments: Vec<String> = path.split('/').map(str::to_string).collect();
        if segments.is_empty() || segments.iter().any(|s| !is_valid_segment(s)) {
            return Err(());
        }
        Ok(Ref { segments })
    }

    /// The path segments, `["billing", "stripe"]`.
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The bare path, `billing/stripe` — the vault storage key.
    pub fn path(&self) -> String {
        self.segments.join("/")
    }

    /// Env-pin resolution: if the first segment names a known env
    /// (`is_known_env`), return that env plus the ref with the pin
    /// stripped — `tamias://prod/billing/stripe` → `(prod,
    /// tamias://billing/stripe)`. `None` otherwise, including a bare
    /// `tamias://prod` (env name with no path is not a secret).
    pub fn env_pin(&self, is_known_env: impl Fn(&str) -> bool) -> Option<(Env, Ref)> {
        let (first, rest) = self.segments.split_first()?;
        if rest.is_empty() || !is_known_env(first) {
            return None;
        }
        let env = Env::new(first).ok()?;
        Some((
            env,
            Ref {
                segments: rest.to_vec(),
            },
        ))
    }
}

/// Is `s` a valid ref path segment? Non-empty, not `.`/`..`, chars ∈
/// `[A-Za-z0-9._-]`. Path-safe by construction so segments can never
/// escape a vault keyspace onto a filesystem path.
pub fn is_valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Failure to parse an [`Ref`] (see [`Ref::new`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tamias ref {0}")]
pub struct ParseRefError(pub String);

impl FromStr for Ref {
    type Err = ParseRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ref::new(s)
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}{}", self.path())
    }
}

impl serde::Serialize for Ref {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Ref {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Ref {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Ref".into()
    }

    fn json_schema(gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        String::json_schema(gen)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn known(name: &str) -> bool {
        Env::WELL_KNOWN.contains(&name)
    }

    #[test]
    fn parse_display() {
        let r = Ref::new("tamias://billing/stripe").unwrap();
        assert_eq!(r.segments(), &["billing", "stripe"]);
        assert_eq!(r.path(), "billing/stripe");
        assert_eq!(r.to_string(), "tamias://billing/stripe");
        let one = Ref::new("tamias://token").unwrap();
        assert_eq!(one.segments(), &["token"]);
    }

    #[test]
    fn rejects() {
        for bad in [
            "billing/stripe",  // no scheme
            "https://x/y",     // wrong scheme
            "tamias://",       // empty
            "tamias://a//b",   // empty segment
            "tamias://a/../b", // traversal
            "tamias://a b/c",  // whitespace
            "tamias://a:b",    // port-ish
            "tamias://a?x",    // query
            "tamias://a#f",    // fragment
        ] {
            assert!(Ref::new(bad).is_err(), "{bad}");
        }
        assert!(Ref::from_path("a/b").is_ok());
        assert!(Ref::from_path("a//b").is_err());
    }

    #[test]
    fn env_pin() {
        let r = Ref::new("tamias://prod/billing/stripe").unwrap();
        let (env, rest) = r.env_pin(known).unwrap();
        assert_eq!(env.as_str(), "prod");
        assert_eq!(rest.to_string(), "tamias://billing/stripe");

        // First segment not an env → no pin.
        let r = Ref::new("tamias://billing/stripe").unwrap();
        assert!(r.env_pin(known).is_none());
        // Bare env name → not a secret, no pin.
        let r = Ref::new("tamias://prod").unwrap();
        assert!(r.env_pin(known).is_none());
        // Custom envs count when the caller says so.
        let r = Ref::new("tamias://ci/token").unwrap();
        assert!(r.env_pin(known).is_none());
        assert!(r.env_pin(|s| s == "ci").is_some());
        // PROD isn't a valid env name even if the predicate says yes.
        let r = Ref::new("tamias://PROD/x").unwrap();
        assert!(r.env_pin(|_| true).is_none());
        // A ref with no segments at all can't pin — unreachable via
        // the public constructors, but the invariant guard stays.
        let r = Ref {
            segments: Vec::new(),
        };
        assert!(r.env_pin(|_| true).is_none());
    }

    #[test]
    fn serde_roundtrip() {
        let r = Ref::new("tamias://a/b").unwrap();
        assert_eq!(serde_json::to_string(&r).unwrap(), "\"tamias://a/b\"");
        assert_eq!(serde_json::from_str::<Ref>("\"tamias://a/b\"").unwrap(), r);
        assert!(serde_json::from_str::<Ref>("\"a/b\"").is_err());
        assert!(serde_json::from_str::<Ref>("42").is_err());
    }
}
