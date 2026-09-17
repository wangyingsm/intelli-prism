use std::error::Error;

/// Every way a cache call can fail.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// A key was built without an id.
    #[error("a cache key needs an id")]
    EmptyKey,

    /// A key id holds characters a backend cannot carry.
    #[error("cache key id {id:?} holds whitespace or control characters")]
    UnusableKey {
        /// The id that was refused.
        id: String,
    },

    /// A span of no time would drop the entry before anything could read it.
    #[error("a ttl must be longer than zero")]
    ZeroTtl,

    /// The backend itself failed.
    #[error("cache backend failed")]
    Backend(#[source] Box<dyn Error + Send + Sync>),
}

impl CacheError {
    /// Wraps a backend failure, keeping the driver out of this crate's public api.
    pub fn backend(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Backend(Box::new(source))
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn a_backend_failure_keeps_its_cause() {
        let error = CacheError::backend(io::Error::other("connection reset"));
        assert_eq!(error.to_string(), "cache backend failed");
        assert_eq!(
            error.source().map(ToString::to_string),
            Some("connection reset".to_owned())
        );
    }

    #[test]
    fn an_unusable_key_names_what_was_refused() {
        let error = CacheError::UnusableKey {
            id: "two words".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "cache key id \"two words\" holds whitespace or control characters"
        );
    }
}
