use async_trait::async_trait;
use ip_core::Timestamp;
use ip_retrieve::{
    MaxHits, MetaSchema, Retriever, RetrieverEndpoint, RetrieverId, RetrieverKey, RetrieverName,
    RetrieverParts, RetrieverTools, Source,
};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};

use super::PostgresStore;
use super::identity::tenant_row_id;
use crate::audit::{By, NewAudit};
use crate::codec::{retriever_auth, retriever_refused};
use crate::error::{Entity, StorageError};
use crate::list::Page;
use crate::retriever::{NewRetriever, RetrieverOwner, RetrieverStore, StoredRetriever};

#[async_trait]
impl RetrieverStore for PostgresStore {
    async fn put_retriever(
        &self,
        retriever: NewRetriever,
        by: &By,
    ) -> Result<StoredRetriever, StorageError> {
        // One transaction, so a retriever that lands without its record is not a state this
        // store can be left in, and neither is a record whose retriever rolled back.
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match writing(&mut transaction, retriever, by).await {
            Ok(written) => transaction
                .commit()
                .await
                .map_err(StorageError::backend)
                .map(|()| written),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn remove_retriever(
        &self,
        owner: &RetrieverOwner,
        id: &RetrieverId,
        by: &By,
    ) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match removing(&mut transaction, owner, id, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn retriever(
        &self,
        owner: &RetrieverOwner,
        id: &RetrieverId,
    ) -> Result<Option<StoredRetriever>, StorageError> {
        read_retriever(&mut *self.connection().await?, owner, id).await
    }

    async fn retrievers(
        &self,
        owner: &RetrieverOwner,
        page: Page,
    ) -> Result<Vec<StoredRetriever>, StorageError> {
        let tenant_row_id = owner_row_id(&mut *self.connection().await?, owner).await?;
        let rows = sqlx::query(
            "SELECT retriever_id, name, endpoint, auth_kind, auth_key, source, tools, meta, \
             max_hits, created_at FROM retrievers \
             WHERE tenant_row_id IS NOT DISTINCT FROM $1 AND created_at > $2 \
             ORDER BY created_at DESC, row_id DESC LIMIT $3 OFFSET $4",
        )
        .bind(tenant_row_id)
        .bind(page.after_seconds())
        .bind(i64::from(page.limit()))
        .bind(i64::from(page.offset()))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| stored(row, owner.clone()))
            .collect()
    }
}

/// Writes a retriever and records the change, both on the one connection its caller commits.
///
/// Apart from the method so that every way out of it is a `?` the caller can roll back, rather
/// than a return that leaves a transaction open.
async fn writing(
    connection: &mut PgConnection,
    new: NewRetriever,
    by: &By,
) -> Result<StoredRetriever, StorageError> {
    let tenant_row_id = owner_row_id(connection, &new.owner).await?;
    let before = read_retriever(connection, &new.owner, new.retriever.id()).await?;
    // Removed and written rather than upserted: the unique index is over an expression, and a
    // conflict target has to repeat that expression exactly on each backend to match it.
    sqlx::query(
        "DELETE FROM retrievers WHERE tenant_row_id IS NOT DISTINCT FROM $1 AND retriever_id = $2",
    )
    .bind(tenant_row_id)
    .bind(new.retriever.id().as_str())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;

    let created_at = Timestamp::now();
    sqlx::query(
        "INSERT INTO retrievers (tenant_row_id, retriever_id, name, endpoint, auth_kind, \
         auth_key, source, tools, meta, max_hits, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(tenant_row_id)
    .bind(new.retriever.id().as_str())
    .bind(new.retriever.name().as_str())
    .bind(new.retriever.endpoint().as_str())
    .bind(new.retriever.auth().name())
    .bind(new.retriever.auth().key().map(RetrieverKey::expose))
    .bind(encoded(new.retriever.source())?)
    .bind(encoded(new.retriever.tools())?)
    .bind(encoded(new.retriever.meta())?)
    .bind(i64::from(new.retriever.max_hits().get()))
    .bind(created_at.unix_seconds())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;

    let written = StoredRetriever {
        owner: new.owner,
        retriever: new.retriever,
        created_at,
    };
    super::audit::record(
        connection,
        NewAudit::changed(by.clone(), before.as_ref(), &written),
    )
    .await?;
    Ok(written)
}

/// Removes a retriever and records the removal, on the one connection its caller commits.
async fn removing(
    connection: &mut PgConnection,
    owner: &RetrieverOwner,
    id: &RetrieverId,
    by: &By,
) -> Result<(), StorageError> {
    let tenant_row_id = owner_row_id(connection, owner).await?;
    let before = read_retriever(connection, owner, id).await?;
    let removed = sqlx::query(
        "DELETE FROM retrievers WHERE tenant_row_id IS NOT DISTINCT FROM $1 AND retriever_id = $2",
    )
    .bind(tenant_row_id)
    .bind(id.as_str())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?
    .rows_affected();
    if removed == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::Retriever,
            id: id.as_str().to_owned(),
        });
    }
    // A row was deleted, so something was there to read a moment ago in the same transaction;
    // an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::Retriever,
            detail: "a retriever was removed that nothing could read".to_owned(),
        });
    };
    super::audit::record(connection, NewAudit::removed(by.clone(), &before)).await?;
    Ok(())
}

/// One owner's retriever, and nothing where that owner has none under the id. Read inside the
/// transaction that is about to change it, so the before it records is the before that was replaced.
async fn read_retriever(
    connection: &mut PgConnection,
    owner: &RetrieverOwner,
    id: &RetrieverId,
) -> Result<Option<StoredRetriever>, StorageError> {
    let tenant_row_id = owner_row_id(connection, owner).await?;
    let row = sqlx::query(
        "SELECT retriever_id, name, endpoint, auth_kind, auth_key, source, tools, meta, \
         max_hits, created_at FROM retrievers \
         WHERE tenant_row_id IS NOT DISTINCT FROM $1 AND retriever_id = $2",
    )
    .bind(tenant_row_id)
    .bind(id.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    row.map(|row| stored(row, owner.clone())).transpose()
}

/// The primary key of the tenant an owner names, and none where the owner is the deployment.
///
/// A tenant that is not there is reported rather than read as the deployment: without this, a
/// lookup for a tenant nobody created would answer with the deployment's own retrievers.
async fn owner_row_id(
    connection: &mut PgConnection,
    owner: &RetrieverOwner,
) -> Result<Option<i64>, StorageError> {
    Ok(match owner.tenant() {
        Some(tenant) => Some(tenant_row_id(connection, tenant).await?.get()),
        None => None,
    })
}

/// One of a retriever's json columns as the column holds it.
fn encoded<T: serde::Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| StorageError::Malformed {
        entity: Entity::Retriever,
        detail: error.to_string(),
    })
}

/// Rebuilds a retriever, refusing a row whose stored value its own type will not take.
pub(super) fn stored(row: PgRow, owner: RetrieverOwner) -> Result<StoredRetriever, StorageError> {
    let parts = RetrieverParts {
        id: RetrieverId::new(row.get::<String, _>("retriever_id").as_str())
            .map_err(retriever_refused)?,
        name: RetrieverName::new(row.get::<String, _>("name").as_str())
            .map_err(retriever_refused)?,
        endpoint: RetrieverEndpoint::new(row.get::<String, _>("endpoint").as_str())
            .map_err(retriever_refused)?,
        auth: retriever_auth(
            row.get::<String, _>("auth_kind").as_str(),
            row.get("auth_key"),
        )?,
        source: decoded::<Source>(row.get("source"))?,
        tools: decoded::<RetrieverTools>(row.get("tools"))?,
        meta: decoded::<MetaSchema>(row.get("meta"))?,
        max_hits: hits(row.get("max_hits"))?,
    };
    Ok(StoredRetriever {
        owner,
        retriever: Retriever::new(parts).map_err(retriever_refused)?,
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

/// One of a retriever's json columns, refused when it is not what its own type reads.
fn decoded<T: serde::de::DeserializeOwned>(stored: String) -> Result<T, StorageError> {
    serde_json::from_str(&stored).map_err(|error| StorageError::Malformed {
        entity: Entity::Retriever,
        detail: error.to_string(),
    })
}

/// The stored ceiling, refusing one past what the type holds. The column carries no CHECK, which
/// is what leaves this reachable from a row at all.
fn hits(stored: i64) -> Result<MaxHits, StorageError> {
    let asked = u32::try_from(stored).map_err(|_| StorageError::Malformed {
        entity: Entity::Retriever,
        detail: format!("a ceiling of {stored} hits"),
    })?;
    MaxHits::new(asked).map_err(retriever_refused)
}
