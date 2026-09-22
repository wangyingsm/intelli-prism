use async_trait::async_trait;
use sqlx::PgConnection;

use super::PostgresStore;
use super::plugin::read_rules;
use super::route::read_routes;
use crate::error::{Entity, StorageError};
use crate::revision::{RevisionStore, RuleRevision, RuleSet};

/// The revision the rules stand at, over whichever connection it is given.
async fn read_revision(connection: &mut PgConnection) -> Result<RuleRevision, StorageError> {
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM rule_revision WHERE id = 1")
        .fetch_one(connection)
        .await
        .map_err(StorageError::backend)?;
    u64::try_from(revision)
        .map(RuleRevision::new)
        .map_err(|_| StorageError::Malformed {
            entity: Entity::RuleRevision,
            detail: format!("revision {revision} is below zero"),
        })
}

#[async_trait]
impl RevisionStore for PostgresStore {
    async fn rule_revision(&self) -> Result<RuleRevision, StorageError> {
        read_revision(&mut *self.connection().await?).await
    }

    async fn rule_set(&self) -> Result<RuleSet, StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *transaction)
            .await
            .map_err(StorageError::backend)?;
        let revision = read_revision(&mut transaction).await?;
        let routes = read_routes(&mut transaction).await?;
        let rules = read_rules(&mut transaction).await?;
        transaction.commit().await.map_err(StorageError::backend)?;
        Ok(RuleSet {
            revision,
            routes,
            rules,
        })
    }
}
