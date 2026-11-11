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
                "[store]\nkind = \"local\"\n[publish]\ndefault = \"cloud\"\n[search]\ndefault = \"mirror\"\n[registries.cloud]\nkind = \"http\"\nurl = \"https://x\"\ntoken_env = \"T\"\n[registries.cloud.limits]\nmax_upload = \"256MiB\"\n[registries.cloud.retention]\nkeep = 3\n[limits]\nmax_download = 1073741824\n[limits.packages.\"kuda-*\"]\nmax_upload = \"2GiB\"\n[retention]\nkeep = 10\n[retention.packages.\"big-*\"]\nkeep = 50\n",
            )
            .unwrap();
        assert_eq!(cfg.store.kind.as_deref(), Some("local"));
        assert_eq!(cfg.publish.default.as_deref(), Some("cloud"));
        assert_eq!(cfg.search.default.as_deref(), Some("mirror"));
        assert_eq!(cfg.registries["cloud"].url.as_deref(), Some("https://x"));
        assert_eq!(cfg.registries["cloud"].limits.max_upload, Some(256 << 20));
        assert_eq!(cfg.limits.max_download, Some(1 << 30));
        assert_eq!(cfg.limits.packages["kuda-*"].max_upload, Some(2 << 30));
        assert_eq!(cfg.retention.keep, Some(10));
        assert_eq!(cfg.retention.packages["big-*"].keep, Some(50));
        assert_eq!(cfg.registries["cloud"].retention.keep, Some(3));
        assert!(f.parse("name: x").is_err());
        // render round-trips
        let text = f.render(&cfg).unwrap();
        assert_eq!(f.parse(&text).unwrap(), cfg);
        assert!(f.render(&Config::default()).is_ok());
    }
}
