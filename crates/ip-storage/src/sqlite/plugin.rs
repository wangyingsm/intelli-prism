use async_trait::async_trait;
use ip_core::{
    ApiId, Checksum, NewPluginRule, PluginKind, PluginOrder, PluginRule, PluginScope, TenantId,
    Timestamp, UserId,
};
use sqlx::Row;

use super::{SqliteStore, is_foreign_key_violation, is_unique_violation};
use crate::error::{Entity, StorageError};
use crate::plugin::{NewPlugin, Plugin, PluginRecord, PluginRowId, PluginRuleStore, PluginStore};

fn plugin_record(row: &sqlx::sqlite::SqliteRow) -> Result<PluginRecord, StorageError> {
    let size: i64 = row.get("size");
    Ok(PluginRecord {
        row_id: PluginRowId::new(row.get("row_id")),
        checksum: Checksum::from_hex(row.get("checksum"))?,
        kind: row.get::<String, _>("kind").parse()?,
        size: usize::try_from(size).map_err(|_| StorageError::Malformed {
            entity: Entity::Plugin,
            detail: format!("size {size} is not a length"),
        })?,
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

/// Rebuilds a rule from its row, so a row the code could not have written is refused on read.
fn plugin_rule(row: &sqlx::sqlite::SqliteRow) -> Result<PluginRule, StorageError> {
    let position: i64 = row.get("position");
    let order = u8::try_from(position).map_err(|_| StorageError::Malformed {
        entity: Entity::PluginRule,
        detail: format!("position {position} is not an order"),
    })?;
    let tenant: Option<String> = row.get("tenant_id");
    let user: Option<String> = row.get("user_id");
    let api: Option<String> = row.get("api_id");
    let scope = match tenant {
        None if user.is_some() || api.is_some() => {
            return Err(StorageError::Malformed {
                entity: Entity::PluginRule,
                detail: "a global rule names a user or an api".to_owned(),
            });
        }
        None => PluginScope::Global,
        Some(tenant) => PluginScope::Tenant {
            tenant: TenantId::new(&tenant)?,
            user: user.as_deref().map(UserId::new).transpose()?,
            api: api.as_deref().map(ApiId::new).transpose()?,
        },
    };
    Ok(PluginRule::new(
        Checksum::from_hex(row.get("checksum"))?,
        row.get::<String, _>("kind").parse()?,
        PluginOrder::new(order),
        scope,
    )?)
}

#[async_trait]
impl PluginStore for SqliteStore {
    async fn put_plugin(&self, new: NewPlugin) -> Result<PluginRecord, StorageError> {
        let checksum = Checksum::of(&new.wasm);
        sqlx::query(
            "INSERT INTO plugins (checksum, kind, wasm, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (checksum) DO NOTHING",
        )
        .bind(checksum.to_hex())
        .bind(new.kind.name())
        .bind(new.wasm.as_slice())
        .bind(Timestamp::now().unix_seconds())
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        let row = sqlx::query(
            "SELECT row_id, checksum, kind, length(wasm) AS size, created_at FROM plugins \
             WHERE checksum = ?",
        )
        .bind(checksum.to_hex())
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        plugin_record(&row)
    }

    async fn plugin(&self, checksum: &Checksum) -> Result<Option<Plugin>, StorageError> {
        let Some(row) = sqlx::query(
            "SELECT row_id, checksum, kind, length(wasm) AS size, created_at, wasm FROM plugins \
             WHERE checksum = ?",
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
            "SELECT row_id, checksum, kind, length(wasm) AS size, created_at FROM plugins \
             ORDER BY row_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.iter().map(plugin_record).collect()
    }

    async fn remove_plugin(&self, checksum: &Checksum) -> Result<(), StorageError> {
        let result = sqlx::query("DELETE FROM plugins WHERE checksum = ?")
            .bind(checksum.to_hex())
            .execute(&self.pool)
            .await;
        let deleted = match result {
            Ok(done) => done.rows_affected(),
            Err(error) if is_foreign_key_violation(&error) => {
                return Err(StorageError::InUse {
                    entity: Entity::Plugin,
                    id: checksum.to_string(),
                });
            }
            Err(error) => return Err(StorageError::backend(error)),
        };
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Plugin,
                id: checksum.to_string(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl PluginRuleStore for SqliteStore {
    async fn put_rule(&self, rule: NewPluginRule) -> Result<PluginRule, StorageError> {
        let plugin = sqlx::query("SELECT row_id, kind FROM plugins WHERE checksum = ?")
            .bind(rule.checksum().to_hex())
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .ok_or_else(|| StorageError::NotFound {
                entity: Entity::Plugin,
                id: rule.checksum().to_string(),
            })?;
        let plugin_row_id: i64 = plugin.get("row_id");
        let kind: PluginKind = plugin.get::<String, _>("kind").parse()?;

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

        let result = sqlx::query(
            "INSERT INTO plugin_rules \
             (plugin_row_id, tenant_row_id, user_row_id, api_id, kind, position) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(plugin_row_id)
        .bind(tenant_row_id)
        .bind(user_row_id)
        .bind(api.as_ref().map(|api| api.as_str()))
        .bind(kind.name())
        .bind(i64::from(rule.order().get()))
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
                 WHERE tenant_row_id = (SELECT row_id FROM tenants WHERE id = ?) \
                 AND kind = ? AND position = ?",
            )
            .bind(tenant.as_str()),
            None => sqlx::query(
                "DELETE FROM plugin_rules WHERE tenant_row_id IS NULL AND kind = ? AND position = ?",
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
             WHERE r.tenant_row_id IS NULL OR t.id = ? \
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

#[cfg(test)]
mod tests {
    use ip_core::TnKey;

    use super::*;
    use crate::model::NewTenant;
    use crate::sqlite::fixture::*;
    use crate::store::TenantStore;

    fn plugin(kind: PluginKind, body: &[u8]) -> NewPlugin {
        NewPlugin {
            kind,
            wasm: body.to_vec(),
        }
    }

    fn placed(checksum: Checksum, order: u8, scope: PluginScope) -> NewPluginRule {
        NewPluginRule::new(checksum, PluginOrder::new(order), scope).unwrap()
    }

    fn acme_wide() -> PluginScope {
        PluginScope::Tenant {
            tenant: tenant_id(),
            user: None,
            api: None,
        }
    }

    fn alice_only() -> PluginScope {
        PluginScope::Tenant {
            tenant: tenant_id(),
            user: Some(user_id()),
            api: None,
        }
    }

    #[tokio::test]
    async fn a_plugin_round_trips_with_its_wasm() {
        let store = store().await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"\0asm module"))
            .await
            .unwrap();
        assert_eq!(record.checksum, Checksum::of(b"\0asm module"));
        assert_eq!(record.kind, PluginKind::ReqBody);
        let read = store.plugin(&record.checksum).await.unwrap().unwrap();
        assert_eq!(read.record, record);
        assert_eq!(read.wasm, b"\0asm module");
    }

    #[tokio::test]
    async fn storing_the_same_plugin_twice_keeps_one_row() {
        let store = store().await;
        let first = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        let second = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(store.plugins().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn listing_plugins_reports_their_size() {
        let store = store().await;
        store
            .put_plugin(plugin(PluginKind::ReqBody, b"123456"))
            .await
            .unwrap();
        store
            .put_plugin(plugin(PluginKind::RespBody, b"12"))
            .await
            .unwrap();
        let sizes: Vec<usize> = store
            .plugins()
            .await
            .unwrap()
            .iter()
            .map(|record| record.size)
            .collect();
        assert_eq!(sizes, vec![6, 2]);
    }

    #[tokio::test]
    async fn removing_a_plugin_that_is_absent_reports_it_missing() {
        let store = store().await;
        assert!(matches!(
            store.remove_plugin(&Checksum::of(b"absent")).await,
            Err(StorageError::NotFound {
                entity: Entity::Plugin,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_plugin_a_rule_still_uses_is_not_removed_until_the_rule_goes() {
        let store = store().await;
        tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        store
            .put_rule(placed(record.checksum, 100, acme_wide()))
            .await
            .unwrap();
        match store.remove_plugin(&record.checksum).await {
            Err(StorageError::InUse {
                entity: Entity::Plugin,
                ..
            }) => {}
            other => panic!("expected the plugin to be refused as in use, got {other:?}"),
        }
        store
            .remove_rule(
                Some(&tenant_id()),
                PluginKind::ReqBody,
                PluginOrder::new(100),
            )
            .await
            .unwrap();
        store.remove_plugin(&record.checksum).await.unwrap();
    }

    #[tokio::test]
    async fn every_rule_scope_round_trips() {
        let store = store().await;
        tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        let anthropic = ApiId::new("anthropic").unwrap();
        let scopes = [
            (10, PluginScope::Global),
            (100, acme_wide()),
            (101, alice_only()),
            (
                102,
                PluginScope::Tenant {
                    tenant: tenant_id(),
                    user: None,
                    api: Some(anthropic.clone()),
                },
            ),
            (
                103,
                PluginScope::Tenant {
                    tenant: tenant_id(),
                    user: Some(user_id()),
                    api: Some(anthropic),
                },
            ),
        ];
        let mut expected = Vec::new();
        for (order, scope) in scopes {
            expected.push(
                store
                    .put_rule(placed(record.checksum, order, scope))
                    .await
                    .unwrap(),
            );
        }
        let read = store.rules().await.unwrap();
        assert_eq!(read.len(), expected.len());
        for rule in &expected {
            assert!(read.contains(rule), "{rule:?} did not survive a round trip");
        }
    }

    #[tokio::test]
    async fn a_stored_rule_takes_its_kind_from_the_plugin() {
        let store = store().await;
        tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::RespHeader, b"module"))
            .await
            .unwrap();
        let rule = store
            .put_rule(placed(record.checksum, 100, acme_wide()))
            .await
            .unwrap();
        assert_eq!(rule.kind(), PluginKind::RespHeader);
        assert_eq!(
            store.rules().await.unwrap()[0].kind(),
            PluginKind::RespHeader
        );
    }

    #[tokio::test]
    async fn a_rule_naming_an_absent_plugin_is_refused() {
        let store = store().await;
        tenant_with_user(&store).await;
        assert!(matches!(
            store
                .put_rule(placed(Checksum::of(b"absent"), 100, acme_wide()))
                .await,
            Err(StorageError::NotFound {
                entity: Entity::Plugin,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_rule_in_a_tenant_that_is_not_there_is_refused() {
        let store = store().await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        assert!(matches!(
            store
                .put_rule(placed(record.checksum, 100, acme_wide()))
                .await,
            Err(StorageError::NotFound {
                entity: Entity::Tenant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_order_a_tenant_already_uses_for_that_kind_is_refused() {
        let store = store().await;
        tenant_with_user(&store).await;
        let first = store
            .put_plugin(plugin(PluginKind::ReqBody, b"one"))
            .await
            .unwrap();
        let second = store
            .put_plugin(plugin(PluginKind::ReqBody, b"two"))
            .await
            .unwrap();
        store
            .put_rule(placed(first.checksum, 100, acme_wide()))
            .await
            .unwrap();
        assert!(matches!(
            store
                .put_rule(placed(second.checksum, 100, alice_only()))
                .await,
            Err(StorageError::Conflict {
                entity: Entity::PluginRule,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn the_same_order_under_another_kind_is_allowed() {
        let store = store().await;
        tenant_with_user(&store).await;
        let request = store
            .put_plugin(plugin(PluginKind::ReqBody, b"one"))
            .await
            .unwrap();
        let response = store
            .put_plugin(plugin(PluginKind::RespBody, b"two"))
            .await
            .unwrap();
        store
            .put_rule(placed(request.checksum, 100, acme_wide()))
            .await
            .unwrap();
        assert!(
            store
                .put_rule(placed(response.checksum, 100, acme_wide()))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_tenant_reads_its_own_rules_and_the_global_ones() {
        let store = store().await;
        tenant_with_user(&store).await;
        let globex = TenantId::new("globex").unwrap();
        store
            .create_tenant(NewTenant {
                id: globex.clone(),
                key: TnKey::generate().unwrap(),
            })
            .await
            .unwrap();
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        store
            .put_rule(placed(record.checksum, 10, PluginScope::Global))
            .await
            .unwrap();
        store
            .put_rule(placed(record.checksum, 100, acme_wide()))
            .await
            .unwrap();
        store
            .put_rule(placed(
                record.checksum,
                100,
                PluginScope::Tenant {
                    tenant: globex,
                    user: None,
                    api: None,
                },
            ))
            .await
            .unwrap();
        let seen = store.rules_for_tenant(&tenant_id()).await.unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|rule| {
            rule.scope()
                .tenant()
                .is_none_or(|owner| owner == &tenant_id())
        }));
    }

    #[tokio::test]
    async fn rules_come_back_highest_order_first() {
        let store = store().await;
        tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        for order in [100, 200, 150] {
            store
                .put_rule(placed(record.checksum, order, acme_wide()))
                .await
                .unwrap();
        }
        let orders: Vec<u8> = store
            .rules_for_tenant(&tenant_id())
            .await
            .unwrap()
            .iter()
            .map(|rule| rule.order().get())
            .collect();
        assert_eq!(orders, vec![200, 150, 100]);
    }

    #[tokio::test]
    async fn deleting_a_tenant_takes_its_rules_with_it() {
        let store = store().await;
        tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        store
            .put_rule(placed(record.checksum, 10, PluginScope::Global))
            .await
            .unwrap();
        store
            .put_rule(placed(record.checksum, 100, acme_wide()))
            .await
            .unwrap();
        store.delete_tenant(&tenant_id()).await.unwrap();
        let left = store.rules().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].scope(), &PluginScope::Global);
    }

    #[tokio::test]
    async fn removing_a_global_rule_leaves_a_tenant_rule_at_the_same_kind() {
        let store = store().await;
        tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        store
            .put_rule(placed(record.checksum, 10, PluginScope::Global))
            .await
            .unwrap();
        store
            .put_rule(placed(record.checksum, 100, acme_wide()))
            .await
            .unwrap();
        store
            .remove_rule(None, PluginKind::ReqBody, PluginOrder::new(10))
            .await
            .unwrap();
        assert_eq!(store.rules().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_database_refuses_a_global_rule_that_names_a_user() {
        let store = store().await;
        let (_, user) = tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        let written = sqlx::query(
            "INSERT INTO plugin_rules (plugin_row_id, tenant_row_id, user_row_id, kind, position) \
             VALUES (?, NULL, ?, 'req_body', 10)",
        )
        .bind(record.row_id.get())
        .bind(user.row_id.get())
        .execute(&store.pool)
        .await;
        assert!(written.is_err());
    }

    #[tokio::test]
    async fn the_database_refuses_an_order_outside_the_scope() {
        let store = store().await;
        let (tenant, _) = tenant_with_user(&store).await;
        let record = store
            .put_plugin(plugin(PluginKind::ReqBody, b"module"))
            .await
            .unwrap();
        let written = sqlx::query(
            "INSERT INTO plugin_rules (plugin_row_id, tenant_row_id, kind, position) \
             VALUES (?, ?, 'req_body', 10)",
        )
        .bind(record.row_id.get())
        .bind(tenant.row_id.get())
        .execute(&store.pool)
        .await;
        assert!(written.is_err());
    }
}
