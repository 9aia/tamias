#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! YAML config impl (`tamias-config-yml`) — **the standard**. Claims
//! `tamias.yml` / `tamias.yaml`; TOML (`tamias-config-toml`) is the
//! alternative, and both parse into the same [`Config`](tamias_config::Config).

use tamias_config::{Config, ConfigFormat};
use tamias_core::Error;

/// The YAML config format — the standard.
#[derive(Clone, Copy, Debug, Default)]
pub struct YmlFormat;

impl ConfigFormat for YmlFormat {
    fn extensions(&self) -> &'static [&'static str] {
        &["yml", "yaml"]
    }

    fn parse(&self, text: &str) -> Result<Config, Error> {
        serde_yaml_ng::from_str(text).map_err(|e| Error::Failed(format!("parse yaml config: {e}")))
    }

    fn render(&self, config: &Config) -> Result<String, Error> {
        serde_yaml_ng::to_string(config)
            .map_err(|e| Error::Failed(format!("render yaml config: {e}")))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_and_rejects_toml() {
        let f = YmlFormat;
        assert_eq!(f.extensions(), &["yml", "yaml"]);
        let cfg = f
            .parse(
                "store:\n  kind: local\npublish:\n  default: cloud\nregistries:\n  cloud:\n    url: https://x\n    token_env: T\n",
            )
            .unwrap();
        assert_eq!(cfg.store.kind.as_deref(), Some("local"));
        assert_eq!(cfg.publish.default.as_deref(), Some("cloud"));
        assert_eq!(cfg.registries["cloud"].url.as_deref(), Some("https://x"));
        assert!(f.parse("[store]\nkind = \"local\"").is_err());
        // render round-trips
        let text = f.render(&cfg).unwrap();
        assert_eq!(f.parse(&text).unwrap(), cfg);
        assert!(f.render(&Config::default()).is_ok());
    }
}
