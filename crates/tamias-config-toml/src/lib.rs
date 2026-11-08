#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! TOML config impl (`tamias-config-toml`) — the **alternative** to the
//! standard YAML. Claims `tamias.toml`.

use tamias_config::{Config, ConfigFormat};
use tamias_core::Error;

/// The TOML config format.
#[derive(Clone, Copy, Debug, Default)]
pub struct TomlFormat;

impl ConfigFormat for TomlFormat {
    fn extensions(&self) -> &'static [&'static str] {
        &["toml"]
    }

    fn parse(&self, text: &str) -> Result<Config, Error> {
        toml::from_str(text).map_err(|e| Error::Failed(format!("parse toml config: {e}")))
    }

    fn render(&self, config: &Config) -> Result<String, Error> {
        toml::to_string(config).map_err(|e| Error::Failed(format!("render toml config: {e}")))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_and_rejects_yaml() {
        let f = TomlFormat;
        assert_eq!(f.extensions(), &["toml"]);
        let cfg = f
            .parse(
                "[store]\nkind = \"local\"\n[publish]\ndefault = \"cloud\"\n[registries.cloud]\nurl = \"https://x\"\ntoken_env = \"T\"\n",
            )
            .unwrap();
        assert_eq!(cfg.store.kind.as_deref(), Some("local"));
        assert_eq!(cfg.publish.default.as_deref(), Some("cloud"));
        assert_eq!(cfg.registries["cloud"].url.as_deref(), Some("https://x"));
        assert!(f.parse("name: x").is_err());
        // render round-trips
        let text = f.render(&cfg).unwrap();
        assert_eq!(f.parse(&text).unwrap(), cfg);
        assert!(f.render(&Config::default()).is_ok());
    }
}
