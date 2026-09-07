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
    /// A dynamic routing rule.
    Route,
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tenant => "tenant",
            Self::User => "user",
            Self::Membership => "membership",
            Self::Grant => "grant",
            Self::Route => "route",
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn every_entity_names_itself() {
        for (entity, name) in [
            (Entity::Tenant, "tenant"),
            (Entity::User, "user"),
            (Entity::Membership, "membership"),
            (Entity::Grant, "grant"),
            (Entity::Route, "route"),
        ] {
            assert_eq!(entity.to_string(), name);
        }
    }

    #[test]
    fn a_missing_record_names_what_was_looked_for() {
        let error = StorageError::NotFound {
            entity: Entity::Tenant,
            id: "acme".to_owned(),
        };
        assert_eq!(error.to_string(), "no such tenant: acme");
    }

    #[test]
    fn a_conflict_names_what_was_written() {
        let error = StorageError::Conflict {
            entity: Entity::User,
            id: "alice".to_owned(),
        };
        assert_eq!(error.to_string(), "user already exists: alice");
    }

    #[test]
    fn a_backend_failure_keeps_its_cause() {
        let cause = io::Error::other("disk went away");
        let error = StorageError::backend(cause);
        assert_eq!(error.to_string(), "storage backend failed");
        assert_eq!(
            error.source().map(ToString::to_string),
            Some("disk went away".to_owned())
        );
    }
}
