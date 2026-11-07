//! Engine failures — filesystem operations carry context; everything
//! else is a message.

use std::io;

/// A bundle/engine operation failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// Manifest/lookup/archive validation failure.
    #[error("{0}")]
    Failed(String),
}

impl Error {
    /// Shorthand for [`Error::Io`] — `map_err(Error::io("read x"))`.
    pub fn io(context: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io {
            context: context.into(),
            source,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn io_and_failed_display() {
        let e = Error::io("read x")(std::io::Error::other("boom"));
        assert!(e.to_string().contains("read x"));
        assert!(Error::Failed("nope".into()).to_string().contains("nope"));
    }
}
