//! Retrievers as they are stored: whose each one is, and what the trail says of one.

use async_trait::async_trait;
use ip_core::{TenantId, Timestamp};
use ip_retrieve::{Retriever, RetrieverId};

use crate::audit::By;
use crate::error::StorageError;
use crate::list::Page;

/// Whose a retriever is: the deployment's, or one tenant's.
///
/// **An owner is part of every lookup rather than something checked afterwards.** A retriever
/// holds a tenant's own documents, so one tenant reaching another's by guessing an id is the one
/// thing this type exists to make impossible: there is no method here that reads a retriever by
/// id alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetrieverOwner {
    /// The deployment's own.
    Global,
    /// One tenant's.
    Tenant(TenantId),
}

impl RetrieverOwner {
    /// The tenant it belongs to, and none where it belongs to the deployment.
    #[must_use]
    pub const fn tenant(&self) -> Option<&TenantId> {
        match self {
            Self::Global => None,
            Self::Tenant(tenant) => Some(tenant),
        }
    }
}

/// A retriever about to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRetriever {
    /// Whose it is.
    pub owner: RetrieverOwner,
    /// What it is.
    pub retriever: Retriever,
}

/// A retriever as it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRetriever {
    /// Whose it is.
    pub owner: RetrieverOwner,
    /// What it is.
    pub retriever: Retriever,
    /// When it was written.
    pub created_at: Timestamp,
}

impl crate::audit::Auditable for StoredRetriever {
    fn entity(&self) -> crate::error::Entity {
        crate::error::Entity::Retriever
    }

    fn entity_id(&self) -> String {
        self.retriever.id().as_str().to_owned()
    }

    fn tenant(&self) -> Option<TenantId> {
        self.owner.tenant().cloned()
    }

    /// Everything a retriever says, and of its credential only which kind it is.
    ///
    /// The key is left out, and so is any digest of it, for the reason an MCP server's token is:
    /// a digest would be a credential the api never hands back, written into a table kept for
    /// years. The cost is the same too — one key replaced by another reads as a write whose two
    /// sides are alike.
    fn audited(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.retriever.name().as_str(),
            "endpoint": self.retriever.endpoint().as_str(),
            "auth": self.retriever.auth().name(),
            "owner": match &self.owner {
                RetrieverOwner::Global => "global".to_owned(),
                RetrieverOwner::Tenant(tenant) => tenant.as_str().to_owned(),
            },
            "source": self.retriever.source(),
            "tools": self.retriever.tools(),
            "meta": self.retriever.meta(),
            "max_hits": self.retriever.max_hits().get(),
        })
    }
}

/// Writes and reads the retrievers an operator has described.
///
/// Nothing holds these in memory and no node is told to reload them, as with templates and
/// servers: whatever runs a node reads the retrievers its tools name when it needs them.
#[async_trait]
pub trait RetrieverStore: Send + Sync {
    /// Writes a retriever, replacing whatever that owner stored under the same id.
    async fn put_retriever(
        &self,
        retriever: NewRetriever,
        by: &By,
    ) -> Result<StoredRetriever, StorageError>;

    /// Removes one owner's retriever, or reports it missing.
    async fn remove_retriever(
        &self,
        owner: &RetrieverOwner,
        id: &RetrieverId,
        by: &By,
    ) -> Result<(), StorageError>;

    /// One owner's retriever, or nothing where that owner has none under the id.
    async fn retriever(
        &self,
        owner: &RetrieverOwner,
        id: &RetrieverId,
    ) -> Result<Option<StoredRetriever>, StorageError>;

    /// One owner's retrievers, newest first.
    ///
    /// An owner sees its own and no one else's, as with templates: a listing that mixed them
    /// would show one tenant's corpus to every tenant.
    async fn retrievers(
        &self,
        owner: &RetrieverOwner,
        page: Page,
    ) -> Result<Vec<StoredRetriever>, StorageError>;
}

#[cfg(test)]
mod tests {
    use ip_core::UserId;
    use ip_retrieve::{
        CollectionName, MaxHits, MetaSchema, RetrieverAuth, RetrieverEndpoint, RetrieverKey,
        RetrieverName, RetrieverParts, RetrieverTool, RetrieverTools, Source,
    };

    use super::*;
    use crate::audit::{Auditable, NewAudit};
    use crate::error::Entity;

    const KEY: &str = "qd-live-9f2c4a7b1e";

    fn retriever(auth: RetrieverAuth) -> Retriever {
        Retriever::new(RetrieverParts {
            id: RetrieverId::new("handbook").unwrap(),
            name: RetrieverName::new("The handbook").unwrap(),
            endpoint: RetrieverEndpoint::new("https://qdrant.acme.test:6333").unwrap(),
            auth,
            source: Source::Qdrant {
                collection: CollectionName::new("handbook").unwrap(),
                vector: None,
            },
            tools: RetrieverTools::new(vec![RetrieverTool::Search]).unwrap(),
            meta: MetaSchema::default(),
            max_hits: MaxHits::new(6).unwrap(),
        })
        .unwrap()
    }

    fn keyed(key: &str) -> RetrieverAuth {
        RetrieverAuth::Key {
            key: RetrieverKey::new(key).unwrap(),
        }
    }

    fn stored(owner: RetrieverOwner) -> StoredRetriever {
        StoredRetriever {
            owner,
            retriever: retriever(keyed(KEY)),
            created_at: Timestamp::from_unix_seconds(1_700_000_000).unwrap(),
        }
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    #[test]
    fn the_deployments_own_retriever_belongs_to_no_tenant() {
        assert_eq!(RetrieverOwner::Global.tenant(), None);
        assert_eq!(stored(RetrieverOwner::Global).tenant(), None);
    }

    #[test]
    fn a_tenants_retriever_names_the_tenant_it_is_recorded_against() {
        let owner = RetrieverOwner::Tenant(tenant());
        assert_eq!(owner.tenant(), Some(&tenant()));
        assert_eq!(stored(owner).tenant(), Some(tenant()));
    }

    #[test]
    fn a_recorded_retriever_names_itself_by_the_id_a_tenant_spelled() {
        let one = stored(RetrieverOwner::Global);
        assert_eq!(one.entity(), Entity::Retriever);
        assert_eq!(one.entity_id(), "handbook");
    }

    #[test]
    fn the_trail_carries_everything_but_the_key() {
        let recorded = stored(RetrieverOwner::Tenant(tenant())).audited();
        assert_eq!(recorded["name"], "The handbook");
        assert_eq!(recorded["owner"], "acme");
        assert_eq!(recorded["endpoint"], "https://qdrant.acme.test:6333");
        assert_eq!(recorded["auth"], "key");
        assert_eq!(recorded["source"]["kind"], "qdrant");
        assert_eq!(recorded["source"]["collection"], "handbook");
        assert_eq!(recorded["tools"][0], "search");
        assert_eq!(recorded["max_hits"], 6);

        let whole = serde_json::to_string(&recorded).unwrap();
        assert!(
            !whole.contains(KEY),
            "the trail carries a credential: {whole}"
        );
    }

    #[test]
    fn the_trail_says_global_where_a_retriever_is_the_deployments() {
        assert_eq!(stored(RetrieverOwner::Global).audited()["owner"], "global");
    }

    #[test]
    fn a_recorded_change_carries_the_retriever_on_each_side_it_has_one() {
        let by = By::new(UserId::new("alice").unwrap(), "/_ip/retrievers");
        let one = stored(RetrieverOwner::Global);

        let created = NewAudit::created(by.clone(), &one);
        assert_eq!(created.entity, Entity::Retriever);
        assert_eq!(created.entity_id, "handbook");
        assert_eq!(created.before, None);
        assert_eq!(created.after, Some(one.audited()));

        let removed = NewAudit::removed(by, &one);
        assert_eq!(removed.before, Some(one.audited()));
        assert_eq!(removed.after, None);
    }
}
