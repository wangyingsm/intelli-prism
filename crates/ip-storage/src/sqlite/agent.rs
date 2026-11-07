use async_trait::async_trait;
use ip_agents::{AgentConfig, AgentId, AgentName, AgentTemplate, Sandbox};
use ip_core::Timestamp;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use super::identity::tenant_row_id;
use crate::agent::{AgentOwner, AgentStore, NewAgent, StoredAgent};
use crate::audit::{By, NewAudit};
use crate::error::{Entity, StorageError};
use crate::list::Page;

#[async_trait]
impl AgentStore for SqliteStore {
    async fn put_agent(&self, agent: NewAgent, by: &By) -> Result<StoredAgent, StorageError> {
        // One transaction, so a template that lands without its record is not a state this
        // store can be left in, and neither is a record whose template rolled back.
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match writing(&mut transaction, agent, by).await {
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

    async fn remove_agent(
        &self,
        owner: &AgentOwner,
        id: &AgentId,
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

    async fn agent(
        &self,
        owner: &AgentOwner,
        id: &AgentId,
    ) -> Result<Option<StoredAgent>, StorageError> {
        read_agent(&mut *self.connection().await?, owner, id).await
    }

    async fn agents(
        &self,
        owner: &AgentOwner,
        page: Page,
    ) -> Result<Vec<StoredAgent>, StorageError> {
        let tenant_row_id = owner_row_id(&mut *self.connection().await?, owner).await?;
        let rows = sqlx::query(
            "SELECT agent_id, name, sandbox, config, created_at FROM agents \
             WHERE tenant_row_id IS ?1 AND created_at > ?2 \
             ORDER BY created_at DESC, row_id DESC LIMIT ?3 OFFSET ?4",
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

/// Writes a template and records the change, both on the one connection its caller commits.
///
/// Apart from the method so that every way out of it is a `?` the caller can roll back, rather
/// than a return that leaves a transaction open.
async fn writing(
    connection: &mut SqliteConnection,
    agent: NewAgent,
    by: &By,
) -> Result<StoredAgent, StorageError> {
    let tenant_row_id = owner_row_id(connection, &agent.owner).await?;
    let before = read_agent(connection, &agent.owner, &agent.template.id).await?;
    // Removed and written rather than upserted: the unique index is over an expression, and a
    // conflict target has to repeat that expression exactly on each backend to match it.
    sqlx::query("DELETE FROM agents WHERE tenant_row_id IS ? AND agent_id = ?")
        .bind(tenant_row_id)
        .bind(agent.template.id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?;

    let created_at = Timestamp::now();
    sqlx::query(
        "INSERT INTO agents (tenant_row_id, agent_id, name, sandbox, config, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant_row_id)
    .bind(agent.template.id.as_str())
    .bind(agent.template.name.as_str())
    .bind(encoded(&agent.template.sandbox)?)
    .bind(encoded(&agent.template.config)?)
    .bind(created_at.unix_seconds())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;

    let written = StoredAgent {
        owner: agent.owner,
        template: agent.template,
        created_at,
    };
    super::audit::record(
        connection,
        NewAudit::changed(by.clone(), before.as_ref(), &written),
    )
    .await?;
    Ok(written)
}

/// Removes a template and records the removal, on the one connection its caller commits.
async fn removing(
    connection: &mut SqliteConnection,
    owner: &AgentOwner,
    id: &AgentId,
    by: &By,
) -> Result<(), StorageError> {
    let tenant_row_id = owner_row_id(connection, owner).await?;
    let before = read_agent(connection, owner, id).await?;
    let removed = sqlx::query("DELETE FROM agents WHERE tenant_row_id IS ? AND agent_id = ?")
        .bind(tenant_row_id)
        .bind(id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
    if removed == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::Agent,
            id: id.as_str().to_owned(),
        });
    }
    // A row was deleted, so something was there to read a moment ago in the same transaction;
    // an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::Agent,
            detail: "a template was removed that nothing could read".to_owned(),
        });
    };
    super::audit::record(connection, NewAudit::removed(by.clone(), &before)).await?;
    Ok(())
}

/// One owner's template, and nothing where that owner stores none under the id. Read inside the
/// transaction that is about to change it, so the before it records is the before that was replaced.
async fn read_agent(
    connection: &mut SqliteConnection,
    owner: &AgentOwner,
    id: &AgentId,
) -> Result<Option<StoredAgent>, StorageError> {
    let tenant_row_id = owner_row_id(connection, owner).await?;
    let row = sqlx::query(
        "SELECT agent_id, name, sandbox, config, created_at FROM agents \
         WHERE tenant_row_id IS ? AND agent_id = ?",
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
/// lookup for a tenant nobody created would answer with the deployment's own templates.
async fn owner_row_id(
    connection: &mut SqliteConnection,
    owner: &AgentOwner,
) -> Result<Option<i64>, StorageError> {
    Ok(match owner.tenant() {
        Some(tenant) => Some(tenant_row_id(connection, tenant).await?.get()),
        None => None,
    })
}

/// One of a template's json columns as the column holds it.
fn encoded<T: serde::Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| StorageError::Malformed {
        entity: Entity::Agent,
        detail: error.to_string(),
    })
}

/// Rebuilds a template, refusing a row whose stored value its own type will not take.
pub(super) fn stored(row: SqliteRow, owner: AgentOwner) -> Result<StoredAgent, StorageError> {
    Ok(StoredAgent {
        owner,
        template: AgentTemplate {
            id: AgentId::new(row.get::<String, _>("agent_id").as_str()).map_err(malformed)?,
            name: AgentName::new(row.get::<String, _>("name").as_str()).map_err(malformed)?,
            sandbox: decoded::<Sandbox>(row.get("sandbox"))?,
            config: decoded::<AgentConfig>(row.get("config"))?,
        },
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

/// One of a template's json columns, refused when it is not what its own type reads.
fn decoded<T: serde::de::DeserializeOwned>(stored: String) -> Result<T, StorageError> {
    serde_json::from_str(&stored).map_err(|error| StorageError::Malformed {
        entity: Entity::Agent,
        detail: error.to_string(),
    })
}

/// A value a template's own type refuses, reported against the template it was read for.
fn malformed(error: ip_agents::AgentError) -> StorageError {
    StorageError::Malformed {
        entity: Entity::Agent,
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::fixture::*;
    use crate::suite::agent::{by, written_by};

    /// Writes a template, then writes over one column with something the code cannot produce.
    async fn written_over(store: &SqliteStore, update: &'static str) {
        store
            .put_agent(written_by(AgentOwner::Global, "triage", "Triage"), &by())
            .await
            .unwrap();
        sqlx::query(update).execute(&store.pool).await.unwrap();
    }

    /// Every column whose stored value its own type can refuse, and a value that it refuses.
    /// The json columns carry no CHECK, which is why each of these is reachable at all.
    const REFUSED: [&str; 7] = [
        "UPDATE agents SET agent_id = 'not an id'",
        "UPDATE agents SET name = ' untrimmed'",
        "UPDATE agents SET sandbox = 'not json'",
        "UPDATE agents SET sandbox = '{\"..\":\"s3://b/f\"}'",
        "UPDATE agents SET config = 'not json'",
        "UPDATE agents SET config = '{\"role\":\"assistant\"}'",
        "UPDATE agents SET config = '{\"role\":\"system\",\"prompt\":\"Be helpful.\",\"max_turns\":0,\"time_limit\":60,\"req_timeout\":10}'",
    ];

    /// Asserted through the listing rather than a single read, because one of these columns is
    /// the id a single read looks the row up by: a refused id would simply not be found.
    #[tokio::test]
    async fn a_row_no_type_of_ours_will_take_is_refused_on_the_way_back() {
        for update in REFUSED {
            let store = store().await;
            written_over(&store, update).await;
            let listed = store.agents(&AgentOwner::Global, Page::default()).await;
            assert!(
                matches!(
                    listed,
                    Err(StorageError::Malformed {
                        entity: Entity::Agent,
                        ..
                    })
                ),
                "{update} was read back as {listed:?}"
            );
        }
    }

    /// A single read rebuilds through the same types, so it refuses the same rows.
    #[tokio::test]
    async fn a_single_read_refuses_what_a_listing_refuses() {
        let store = store().await;
        written_over(&store, "UPDATE agents SET config = 'not json'").await;

        let read = store
            .agent(&AgentOwner::Global, &AgentId::new("triage").unwrap())
            .await;
        assert!(
            matches!(
                read,
                Err(StorageError::Malformed {
                    entity: Entity::Agent,
                    ..
                })
            ),
            "{read:?}"
        );
    }

    /// The one column that is neither json nor text, and the only value it holds that the
    /// timestamp type will not take. A second before the epoch is a time, and is not refused.
    #[tokio::test]
    async fn a_row_timed_past_what_a_timestamp_holds_is_refused_on_the_way_back() {
        let store = store().await;
        written_over(&store, "UPDATE agents SET created_at = 9223372036854775807").await;

        let read = store
            .agent(&AgentOwner::Global, &AgentId::new("triage").unwrap())
            .await;
        assert!(matches!(read, Err(StorageError::Value(_))), "{read:?}");
    }
}
