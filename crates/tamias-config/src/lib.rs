//! Config port — where the `Config` value comes from.
//! Impl: `tamias-config-toml` (file); env/remote sources = new impls.

/// A config source — returns the loaded `Config`.
pub trait ConfigSource: Send + Sync {
    /// Load + validate the config; Err carries a readable message.
    fn load(&self) -> Result<tamias_core::Config, String>;
}
