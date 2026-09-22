use async_trait::async_trait;
use ip_core::{
    Checksum, NewPluginRule, PluginKind, PluginOrder, PluginRule, PluginScope, TenantId, Timestamp,
};
use sqlx::{PgConnection, Row};

use super::PostgresStore;
use super::identity::tenant_row_id;
use crate::codec::{plugin_order, plugin_scope, plugin_size};
use crate::error::{Entity, StorageError, is_foreign_key_violation, is_unique_violation};
use crate::plugin::{
    NewPlugin, Plugin, PluginOwner, PluginRecord, PluginRowId, PluginRuleStore, PluginStore,
};
use crate::transaction::PluginDialect;
use crate::transaction::plugin_disown::{
    PluginDisownBegun, PluginDisownTransactional, PluginDisownTxn,
};
use crate::transaction::plugin_upload::{
    PluginUploadBegun, PluginUploadTransactional, PluginUploadTxn,
};

pub(super) fn plugin_record(row: &sqlx::postgres::PgRow) -> Result<PluginRecord, StorageError> {
    Ok(PluginRecord {
        row_id: PluginRowId::new(row.get("row_id")),
        checksum: Checksum::from_hex(row.get("checksum"))?,
        kind: row.get::<String, _>("kind").parse()?,
        size: plugin_size(row.get("size"))?,
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

/// Rebuilds a rule from its row, so a row the code could not have written is refused on read.
pub(super) fn plugin_rule(row: &sqlx::postgres::PgRow) -> Result<PluginRule, StorageError> {
    let order = plugin_order(row.get("position"))?;
    let scope = plugin_scope(row.get("tenant_id"), row.get("user_id"), row.get("api_id"))?;
    Ok(PluginRule::new(
        Checksum::from_hex(row.get("checksum"))?,
        row.get::<String, _>("kind").parse()?,
        order,
        scope,
    )?)
}

#[async_trait]
impl PluginStore for PostgresStore {
    async fn put_plugin(&self, new: NewPlugin) -> Result<PluginRecord, StorageError> {
        let uploaded = self
            .begin_plugin_upload()
            .await?
            .store_wasm(new.kind, new.wasm)
            .await?
            .own(new.owner)
            .await?
            .commit()
            .await?;
        Ok(uploaded.plugin)
    }

    async fn plugin(&self, checksum: &Checksum) -> Result<Option<Plugin>, StorageError> {
        let Some(row) = sqlx::query(
            "SELECT row_id, checksum, kind, length(wasm)::BIGINT AS size, created_at, wasm \
             FROM plugins WHERE checksum = $1",
        )
        .bind(checksum.to_hex())
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::backend)?
        else {
            return Ok(None);
        };
        Ok(Some(Plugin {
            record: plugin_record(&row)?,
            wasm: row.get("wasm"),
        }))
    }

    async fn plugins(&self) -> Result<Vec<PluginRecord>, StorageError> {
        let rows = sqlx::query(
            "SELECT row_id, checksum, kind, length(wasm)::BIGINT AS size, created_at FROM plugins \
             ORDER BY row_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.iter().map(plugin_record).collect()
    }

    async fn disown_plugin(
        &self,
        checksum: &Checksum,
        owner: &PluginOwner,
    ) -> Result<bool, StorageError> {
        let disowned = self
            .begin_plugin_disown()
            .await?
            .disown(*checksum, owner.clone())
            .await?
            .sweep()
            .await?
            .commit()
            .await?;
        Ok(disowned.swept)
    }
}

#[async_trait]
impl PluginRuleStore for PostgresStore {
    async fn put_rule(&self, rule: NewPluginRule) -> Result<PluginRule, StorageError> {
        let (tenant_row_id, user_row_id, api) = match rule.scope() {
            PluginScope::Global => (None, None, None),
            PluginScope::Tenant { tenant, user, api } => {
                let tenant_row_id = self.tenant_row_id(tenant).await?.get();
                let user_row_id = match user {
                    Some(user) => Some(self.user_row_id(user).await?.get()),
                    None => None,
                };
                (Some(tenant_row_id), user_row_id, api.clone())
            }
        };

        let plugin = sqlx::query(
            "SELECT p.row_id, p.kind FROM plugins p \
             JOIN plugin_owners o ON o.plugin_row_id = p.row_id \
             WHERE p.checksum = $1 AND o.tenant_row_id IS NOT DISTINCT FROM $2",
        )
        .bind(rule.checksum().to_hex())
        .bind(tenant_row_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::backend)?
        .ok_or_else(|| StorageError::NotFound {
            entity: Entity::Plugin,
            id: rule.checksum().to_string(),
        })?;
        let plugin_row_id: i64 = plugin.get("row_id");
        let kind: PluginKind = plugin.get::<String, _>("kind").parse()?;

        let result = sqlx::query(
            "INSERT INTO plugin_rules \
             (plugin_row_id, tenant_row_id, user_row_id, api_id, kind, position, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(plugin_row_id)
        .bind(tenant_row_id)
        .bind(user_row_id)
        .bind(api.as_ref().map(|api| api.as_str()))
        .bind(kind.name())
        .bind(i64::from(rule.order().get()))
        .bind(Timestamp::now().unix_seconds())
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(rule.with_kind(kind)),
            Err(error) if is_unique_violation(&error) => Err(StorageError::Conflict {
                entity: Entity::PluginRule,
                id: format!("{kind} {}", rule.order()),
            }),
            Err(error) => Err(StorageError::backend(error)),
        }
    }

    async fn remove_rule(
        &self,
        tenant: Option<&TenantId>,
        kind: PluginKind,
        order: PluginOrder,
    ) -> Result<(), StorageError> {
        let query = match tenant {
            Some(tenant) => sqlx::query(
                "DELETE FROM plugin_rules \
                 WHERE tenant_row_id = (SELECT row_id FROM tenants WHERE id = $1) \
                 AND kind = $2 AND position = $3",
            )
            .bind(tenant.as_str()),
            None => sqlx::query(
                "DELETE FROM plugin_rules \
                 WHERE tenant_row_id IS NULL AND kind = $1 AND position = $2",
            ),
        };
        let deleted = query
            .bind(kind.name())
            .bind(i64::from(order.get()))
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::PluginRule,
                id: format!("{kind} {order}"),
            });
        }
        Ok(())
    }

    async fn rules_for_tenant(&self, tenant: &TenantId) -> Result<Vec<PluginRule>, StorageError> {
        let rows = sqlx::query(
            "SELECT p.checksum, r.kind, r.position, t.id AS tenant_id, u.id AS user_id, \
             r.api_id FROM plugin_rules r \
             JOIN plugins p ON p.row_id = r.plugin_row_id \
             LEFT JOIN tenants t ON t.row_id = r.tenant_row_id \
             LEFT JOIN users u ON u.row_id = r.user_row_id \
             WHERE r.tenant_row_id IS NULL OR t.id = $1 \
             ORDER BY r.kind, r.position DESC",
        )
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.iter().map(plugin_rule).collect()
    }

    async fn rules(&self) -> Result<Vec<PluginRule>, StorageError> {
        let rows = sqlx::query(
            "SELECT p.checksum, r.kind, r.position, t.id AS tenant_id, u.id AS user_id, \
             r.api_id FROM plugin_rules r \
             JOIN plugins p ON p.row_id = r.plugin_row_id \
             LEFT JOIN tenants t ON t.row_id = r.tenant_row_id \
             LEFT JOIN users u ON u.row_id = r.user_row_id \
             ORDER BY r.kind, r.position DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.iter().map(plugin_rule).collect()
    }
}

/// Stores wasm once under its checksum over whichever connection it is given, refusing wasm
/// stored already as another kind.
pub(super) async fn insert_wasm(
    connection: &mut PgConnection,
    kind: PluginKind,
    wasm: Vec<u8>,
) -> Result<PluginRecord, StorageError> {
    let checksum = Checksum::of(&wasm);
    sqlx::query(
        "INSERT INTO plugins (checksum, kind, wasm, created_at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (checksum) DO NOTHING",
    )
    .bind(checksum.to_hex())
    .bind(kind.name())
    .bind(wasm.as_slice())
    .bind(Timestamp::now().unix_seconds())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    let row = sqlx::query(
        "SELECT row_id, checksum, kind, length(wasm)::BIGINT AS size, created_at FROM plugins \
         WHERE checksum = $1",
    )
    .bind(checksum.to_hex())
    .fetch_one(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    let record = plugin_record(&row)?;
    if record.kind != kind {
        return Err(StorageError::Conflict {
            entity: Entity::Plugin,
            id: format!("{checksum} as a {} plugin", record.kind),
        });
    }
    Ok(record)
}

/// The row an owner's chain is kept under: its tenant's, or none for the global chain.
async fn owner_row_id(
    connection: &mut PgConnection,
    owner: &PluginOwner,
) -> Result<Option<i64>, StorageError> {
    Ok(match owner.tenant() {
        Some(tenant) => Some(tenant_row_id(connection, tenant).await?.get()),
        None => None,
    })
}

/// Records an owner's hold on a plugin over whichever connection it is given.
pub(super) async fn insert_owner(
    connection: &mut PgConnection,
    plugin: &PluginRecord,
    owner: &PluginOwner,
) -> Result<Timestamp, StorageError> {
    let tenant_row_id = owner_row_id(&mut *connection, owner).await?;
    sqlx::query(
        "INSERT INTO plugin_owners (plugin_row_id, tenant_row_id, created_at) VALUES ($1, $2, $3) \
         ON CONFLICT DO NOTHING",
    )
    .bind(plugin.row_id.get())
    .bind(tenant_row_id)
    .bind(Timestamp::now().unix_seconds())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    let owned_at: i64 = sqlx::query_scalar(
        "SELECT created_at FROM plugin_owners WHERE plugin_row_id = $1 AND tenant_row_id IS NOT DISTINCT FROM $2",
    )
    .bind(plugin.row_id.get())
    .bind(tenant_row_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    Ok(Timestamp::from_unix_seconds(owned_at)?)
}

/// Ends an owner's hold on a plugin over whichever connection it is given.
pub(super) async fn delete_owner(
    connection: &mut PgConnection,
    checksum: &Checksum,
    owner: &PluginOwner,
) -> Result<PluginRecord, StorageError> {
    let missing = || StorageError::NotFound {
        entity: Entity::Plugin,
        id: checksum.to_string(),
    };
    let tenant_row_id = owner_row_id(&mut *connection, owner).await?;
    let row = sqlx::query(
        "SELECT row_id, checksum, kind, length(wasm)::BIGINT AS size, created_at FROM plugins \
         WHERE checksum = $1",
    )
    .bind(checksum.to_hex())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StorageError::backend)?
    .ok_or_else(missing)?;
    let record = plugin_record(&row)?;
    let in_use: Option<i64> = sqlx::query_scalar(
        "SELECT row_id FROM plugin_rules WHERE plugin_row_id = $1 AND tenant_row_id IS NOT DISTINCT FROM $2 LIMIT 1",
    )
    .bind(record.row_id.get())
    .bind(tenant_row_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    if in_use.is_some() {
        return Err(StorageError::InUse {
            entity: Entity::Plugin,
            id: checksum.to_string(),
        });
    }
    let deleted = sqlx::query(
        "DELETE FROM plugin_owners WHERE plugin_row_id = $1 AND tenant_row_id IS NOT DISTINCT FROM $2",
    )
    .bind(record.row_id.get())
    .bind(tenant_row_id)
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?
    .rows_affected();
    if deleted == 0 {
        return Err(missing());
    }
    Ok(record)
}

/// Removes a plugin's wasm once nobody owns it, over whichever connection it is given.
pub(super) async fn delete_unowned(
    connection: &mut PgConnection,
    plugin: &PluginRecord,
) -> Result<bool, StorageError> {
    let result = sqlx::query(
        "DELETE FROM plugins WHERE row_id = $1 \
         AND NOT EXISTS (SELECT 1 FROM plugin_owners WHERE plugin_row_id = $2)",
    )
    .bind(plugin.row_id.get())
    .bind(plugin.row_id.get())
    .execute(connection)
    .await;
    match result {
        Ok(done) => Ok(done.rows_affected() > 0),
        Err(error) if is_foreign_key_violation(&error) => Err(StorageError::InUse {
            entity: Entity::Plugin,
            id: plugin.checksum.to_string(),
        }),
        Err(error) => Err(StorageError::backend(error)),
    }
}

#[async_trait]
impl PluginDialect for sqlx::Postgres {
    async fn insert_wasm(
        connection: &mut PgConnection,
        kind: PluginKind,
        wasm: Vec<u8>,
    ) -> Result<PluginRecord, StorageError> {
        insert_wasm(connection, kind, wasm).await
    }

    async fn insert_owner(
        connection: &mut PgConnection,
        plugin: &PluginRecord,
        owner: &PluginOwner,
    ) -> Result<Timestamp, StorageError> {
        insert_owner(connection, plugin, owner).await
    }

    async fn delete_owner(
        connection: &mut PgConnection,
        checksum: &Checksum,
        owner: &PluginOwner,
    ) -> Result<PluginRecord, StorageError> {
        delete_owner(connection, checksum, owner).await
    }

    async fn delete_unowned(
        connection: &mut PgConnection,
        plugin: &PluginRecord,
    ) -> Result<bool, StorageError> {
        delete_unowned(connection, plugin).await
    }
}

impl PluginUploadTransactional for PostgresStore {
    type Db = sqlx::Postgres;

    async fn begin_plugin_upload(
        &self,
    ) -> Result<PluginUploadTxn<Self::Db, PluginUploadBegun>, StorageError> {
        let inner = self.pool.begin().await.map_err(StorageError::backend)?;
        Ok(PluginUploadTxn::new(inner))
    }
}

impl PluginDisownTransactional for PostgresStore {
    type Db = sqlx::Postgres;

    async fn begin_plugin_disown(
        &self,
    ) -> Result<PluginDisownTxn<Self::Db, PluginDisownBegun>, StorageError> {
        let inner = self.pool.begin().await.map_err(StorageError::backend)?;
        Ok(PluginDisownTxn::new(inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::scratch;
    use crate::suite::fixture::*;
    use crate::suite::plugin::plugin;

    #[tokio::test]
    async fn the_database_refuses_a_global_rule_that_names_a_user() {
        let Some(store) = scratch::store().await else {
            return;
        };
        let (_, user) = tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        let written = sqlx::query(
            "INSERT INTO plugin_rules \
             (plugin_row_id, tenant_row_id, user_row_id, kind, position, created_at) \
             VALUES ($1, NULL, $2, 'req_body', 10, 0)",
        )
        .bind(record.row_id.get())
        .bind(user.row_id.get())
        .execute(&store.pool)
        .await;
        assert!(written.is_err());
    }

    #[tokio::test]
    async fn the_database_refuses_an_order_outside_the_scope() {
        let Some(store) = scratch::store().await else {
            return;
        };
        let (tenant, _) = tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        let written = sqlx::query(
            "INSERT INTO plugin_rules (plugin_row_id, tenant_row_id, kind, position, created_at) \
             VALUES ($1, $2, 'req_body', 10, 0)",
        )
        .bind(record.row_id.get())
        .bind(tenant.row_id.get())
        .execute(&store.pool)
        .await;
        assert!(written.is_err());
    }
}
