//! The op vocabulary — the verb names guardrails, audit events, and
//! (later) the `serve` wire all share. Lowercase serde/parse forms
//! match the CLI subcommands.

use std::fmt;
use std::str::FromStr;

/// An tamias verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename_all = "lowercase"))]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// Bootstrap identity + vault.
    Init,
    /// Store a secret.
    Put,
    /// Read a secret's value.
    Get,
    /// List refs in an env.
    List,
    /// Remove a secret.
    Rm,
    /// Rotate a secret (new value, version bump).
    Rotate,
    /// Inject declared secrets into a child process.
    Run,
    /// Print `export` lines for `eval`.
    Env,
    /// Validate config + declared surface.
    Check,
    /// Push a stored value to a write-only deploy target.
    Deploy,
    /// Rewrite stored records for a wire/config upgrade.
    Migrate,
    /// Interactive TUI.
    Ui,
    /// ndjson verb dispatch (later ADR).
    Serve,
    /// Agent self-discovery.
    Manifest,
    /// Machine-readable schemas.
    Schema,
}

impl Op {
    /// All ops, in declaration order.
    pub const ALL: [Op; 15] = [
        Op::Init,
        Op::Put,
        Op::Get,
        Op::List,
        Op::Rm,
        Op::Rotate,
        Op::Run,
        Op::Env,
        Op::Check,
        Op::Deploy,
        Op::Migrate,
        Op::Ui,
        Op::Serve,
        Op::Manifest,
        Op::Schema,
    ];
}

/// Failure to parse an [`Op`] name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid op {0:?}")]
pub struct ParseOpError(pub String);

impl Op {
    /// The CLI verb name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Op::Init => "init",
            Op::Put => "put",
            Op::Get => "get",
            Op::List => "list",
            Op::Rm => "rm",
            Op::Rotate => "rotate",
            Op::Run => "run",
            Op::Env => "env",
            Op::Check => "check",
            Op::Deploy => "deploy",
            Op::Migrate => "migrate",
            Op::Ui => "ui",
            Op::Serve => "serve",
            Op::Manifest => "manifest",
            Op::Schema => "schema",
        }
    }
}

impl FromStr for Op {
    type Err = ParseOpError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Op::ALL
            .iter()
            .copied()
            .find(|op| op.as_str() == s)
            .ok_or_else(|| ParseOpError(s.to_string()))
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        for op in Op::ALL {
            assert_eq!(op.as_str().parse::<Op>().unwrap(), op);
            assert_eq!(op.to_string(), op.as_str());
        }
        assert!("nope".parse::<Op>().is_err());
    }

    #[test]
    fn serde_is_lowercase() {
        assert_eq!(serde_json::to_string(&Op::Rotate).unwrap(), "\"rotate\"");
        assert_eq!(serde_json::from_str::<Op>("\"run\"").unwrap(), Op::Run);
        assert!(serde_json::from_str::<Op>("\"Run\"").is_err());
    }

    #[test]
    fn all_covers_schema() {
        assert!(Op::ALL.contains(&Op::Schema));
        assert_eq!(Op::Schema.as_str(), "schema");
    }
}
