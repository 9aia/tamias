//! Actor ids — attribution strings like `devin`, `human:luis`,
//! `claude` carried on audit events. A validated non-empty string;
//! the `human:`/`agent:`-style prefix is allowed but not required.
//! Attribution only — never an identity or policy boundary
//! (`TAMIAS_ACTOR` is self-claimed and spoofable by construction).

use std::fmt;
use std::str::FromStr;

/// An actor (human or agent) — attribution only, not identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Actor(String);

impl Actor {
    /// Validate and wrap. Rules: non-empty; chars ∈
    /// `[A-Za-z0-9._:-]`; may not start or end with `:`; no `::`.
    pub fn new(s: &str) -> Result<Self, ParseActorError> {
        if is_valid_actor(s) {
            Ok(Actor(s.to_string()))
        } else {
            Err(ParseActorError(s.to_string()))
        }
    }

    /// The raw id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Is `s` a valid actor id? See [`Actor::new`].
pub fn is_valid_actor(s: &str) -> bool {
    !s.is_empty()
        && !s.contains("::")
        && !s.starts_with(':')
        && !s.ends_with(':')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
}

/// Failure to validate an [`Actor`] id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid actor id {0:?}")]
pub struct ParseActorError(pub String);

impl FromStr for Actor {
    type Err = ParseActorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Actor::new(s)
    }
}

impl TryFrom<String> for Actor {
    type Error = ParseActorError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Actor::new(&s)
    }
}

impl From<Actor> for String {
    fn from(a: Actor) -> Self {
        a.0
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for Actor {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Actor {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Actor {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Actor".into()
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
        for ok in [
            "devin",
            "human:luis",
            "agent:claude",
            "luis.p",
            "bot_1",
            "a-b",
        ] {
            assert!(is_valid_actor(ok), "{ok}");
            assert_eq!(Actor::new(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn invalid() {
        for bad in [
            "",
            ":x",
            "x:",
            "a::b",
            "has space",
            "a·b",
            "dot,comma",
            "🍄",
        ] {
            assert!(!is_valid_actor(bad), "{bad}");
            assert!(bad.parse::<Actor>().is_err(), "{bad}");
        }
    }

    #[test]
    fn conversions_and_display() {
        let a = Actor::new("human:luis").unwrap();
        assert_eq!(a.to_string(), "human:luis");
        assert_eq!(String::from(a), "human:luis");
        assert_eq!(
            Actor::try_from("devin".to_string()).unwrap(),
            Actor::new("devin").unwrap()
        );
        assert!(Actor::try_from("".to_string()).is_err());
    }

    #[test]
    fn serde_validates() {
        let a = Actor::new("devin").unwrap();
        assert_eq!(serde_json::to_string(&a).unwrap(), "\"devin\"");
        assert_eq!(serde_json::from_str::<Actor>("\"devin\"").unwrap(), a);
        assert!(serde_json::from_str::<Actor>("\"has space\"").is_err());
        // a non-string can't even reach the actor check
        assert!(serde_json::from_str::<Actor>("42").is_err());
        assert_eq!(
            "has space".parse::<Actor>().unwrap_err(),
            ParseActorError("has space".into())
        );
    }

    #[cfg(feature = "schema")]
    #[test]
    fn json_schema_compiles() {
        let schema = schemars::schema_for!(Actor);
        assert!(serde_json::to_value(&schema).is_ok());
    }
}
