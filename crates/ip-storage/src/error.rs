use std::error::Error;
use std::fmt;

use ip_core::CoreError;

/// Every way a storage call can fail.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The record does not exist.
    #[error("no such {entity}: {id}")]
    NotFound {
        /// What was looked for.
        entity: Entity,
        /// How it was named.
        id: String,
    },

    /// A record with that identity already exists.
    #[error("{entity} already exists: {id}")]
    Conflict {
        /// What was being written.
        entity: Entity,
        /// How it was named.
        id: String,
    },

    /// A stored value no longer satisfies the rules of its type.
    #[error(transparent)]
    Value(#[from] CoreError),

    /// A stored value cannot be read back as the type it belongs to.
    #[error("stored {entity} is malformed: {detail}")]
    Malformed {
        /// What was being read.
        entity: Entity,
        /// What was wrong with it.
        detail: String,
    },

    /// The backend itself failed.
    #[error("storage backend failed")]
    Backend(#[source] Box<dyn Error + Send + Sync>),
}

impl StorageError {
    /// Wraps a backend failure, keeping the driver out of this crate's public api.
    pub fn backend(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Backend(Box::new(source))
    }
}

/// What a storage error is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entity {
    /// A tenant record.
    Tenant,
    /// A user record.
    User,
    /// A user's attachment to a tenant.
    Membership,
    /// One capability held at one scope.
    Grant,
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tenant => "tenant",
            Self::User => "user",
            Self::Membership => "membership",
            Self::Grant => "grant",
        })
    }
}
