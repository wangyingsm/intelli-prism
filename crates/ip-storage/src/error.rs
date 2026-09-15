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

    /// The record is still referenced, so removing it would break what depends on it.
    #[error("{entity} is still in use: {id}")]
    InUse {
        /// What was being removed.
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

/// Whether the database refused a write for breaking a foreign key.
#[cfg(feature = "standalone-storage")]
pub(crate) fn is_foreign_key_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.is_foreign_key_violation())
}

/// Whether the database refused a write for breaking a unique constraint.
#[cfg(any(feature = "standalone-storage", feature = "fast-storage"))]
pub(crate) fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.is_unique_violation())
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
    /// A stored wasm plugin.
    Plugin,
    /// A plugin placed in a chain.
    PluginRule,
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tenant => "tenant",
            Self::User => "user",
            Self::Membership => "membership",
            Self::Grant => "grant",
            Self::Route => "route",
            Self::Plugin => "plugin",
            Self::PluginRule => "plugin rule",
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
            (Entity::Plugin, "plugin"),
            (Entity::PluginRule, "plugin rule"),
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
    fn a_record_in_use_names_what_holds_it() {
        let error = StorageError::InUse {
            entity: Entity::Plugin,
            id: "abc".to_owned(),
        };
        assert_eq!(error.to_string(), "plugin is still in use: abc");
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
