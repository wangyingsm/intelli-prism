use async_trait::async_trait;
use sqlx::PgConnection;

use super::PostgresStore;
use super::limit::read_limits;
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
        let limits = read_limits(&mut transaction).await?;
        transaction.commit().await.map_err(StorageError::backend)?;
        Ok(RuleSet {
            revision,
            routes,
            rules,
            limits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A revision below zero is one no write could have made, so it reads as malformed rather
    /// than as some other number.
    #[tokio::test]
    async fn a_revision_below_zero_is_reported_as_malformed() {
        let Some(store) = crate::postgres::scratch::store().await else {
            return;
        };
        sqlx::query("UPDATE rule_revision SET revision = $1 WHERE id = 1")
            .bind(-1_i64)
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(matches!(
            store.rule_revision().await,
            Err(StorageError::Malformed {
                entity: Entity::RuleRevision,
                ..
            })
        ));
    }
}
