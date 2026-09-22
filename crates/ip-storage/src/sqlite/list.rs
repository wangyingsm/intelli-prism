use async_trait::async_trait;
use ip_core::{
    ApiId, Endpoint, Grant, PluginRule, RouteKey, RouteRule, RouteTarget, TenantId, Timestamp,
    UserId,
};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::SqliteStore;
use super::identity::tenant_row_id;
use super::plugin::{plugin_record, plugin_rule};
use crate::codec::{capability, scope, standing};
use crate::error::StorageError;
use crate::list::{ListStore, Listed, Page};
use crate::model::Membership;
use crate::plugin::{PluginOwner, PluginRecord};

fn created_at(row: &SqliteRow) -> Result<Timestamp, StorageError> {
    Ok(Timestamp::from_unix_seconds(row.get("created_at"))?)
}

fn listed_grant(
    user: &UserId,
    tenant: Option<String>,
    row: &SqliteRow,
) -> Result<Listed<Grant>, StorageError> {
    let scope = scope(user, tenant, row.get("api_id"))?;
    Ok(Listed {
        item: Grant::new(capability(row.get("capability"))?, scope)?,
        created_at: created_at(row)?,
    })
}

fn endpoint(row: &SqliteRow, prefix: &str) -> Result<Endpoint, StorageError> {
    Ok(Endpoint::from_parts(
        row.get(format!("{prefix}protocol").as_str()),
        row.get(format!("{prefix}host").as_str()),
        row.get(format!("{prefix}port").as_str()),
        row.get(format!("{prefix}path").as_str()),
    )?)
}

#[async_trait]
impl ListStore for SqliteStore {
    async fn list_members(
        &self,
        tenant: &TenantId,
        page: Page,
    ) -> Result<Vec<Listed<Membership>>, StorageError> {
        let rows = sqlx::query(
            "SELECT u.id AS user_id, m.standing, m.created_at FROM memberships m \
             JOIN tenants t ON t.row_id = m.tenant_row_id \
             JOIN users u ON u.row_id = m.user_row_id \
             WHERE t.id = ? AND m.created_at > ? \
             ORDER BY m.created_at DESC, m.user_row_id DESC LIMIT ? OFFSET ?",
        )
        .bind(tenant.as_str())
        .bind(page.after_seconds())
        .bind(i64::from(page.limit()))
        .bind(i64::from(page.offset()))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.iter()
            .map(|row| {
                Ok(Listed {
                    item: Membership {
                        user: UserId::new(row.get("user_id"))?,
                        tenant: tenant.clone(),
                        standing: standing(row.get("standing"))?,
                    },
                    created_at: created_at(row)?,
                })
            })
            .collect()
    }

    async fn list_tenant_grants(
        &self,
        user: &UserId,
        tenant: &TenantId,
        page: Page,
    ) -> Result<Vec<Listed<Grant>>, StorageError> {
        let rows = sqlx::query(
            "SELECT g.api_id, g.capability, g.created_at FROM grants g \
             JOIN users u ON u.row_id = g.user_row_id \
             JOIN tenants t ON t.row_id = g.tenant_row_id \
             WHERE u.id = ? AND t.id = ? AND g.created_at > ? \
             ORDER BY g.created_at DESC, g.row_id DESC LIMIT ? OFFSET ?",
        )
        .bind(user.as_str())
        .bind(tenant.as_str())
        .bind(page.after_seconds())
        .bind(i64::from(page.limit()))
        .bind(i64::from(page.offset()))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.iter()
            .map(|row| listed_grant(user, Some(tenant.to_string()), row))
            .collect()
    }

    async fn list_account_grants(
        &self,
        user: &UserId,
        page: Page,
    ) -> Result<Vec<Listed<Grant>>, StorageError> {
        let rows = sqlx::query(
            "SELECT g.api_id, g.capability, g.created_at FROM grants g \
             JOIN users u ON u.row_id = g.user_row_id \
             WHERE u.id = ? AND g.tenant_row_id IS NULL AND g.created_at > ? \
             ORDER BY g.created_at DESC, g.row_id DESC LIMIT ? OFFSET ?",
        )
        .bind(user.as_str())
        .bind(page.after_seconds())
        .bind(i64::from(page.limit()))
        .bind(i64::from(page.offset()))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.iter()
            .map(|row| listed_grant(user, None, row))
            .collect()
    }

    async fn list_routes(&self, page: Page) -> Result<Vec<Listed<RouteRule>>, StorageError> {
        let rows = sqlx::query(
            "SELECT r.row_id, r.api_id, r.key_protocol, r.key_host, r.key_port, r.key_path, \
             r.created_at, t.protocol, t.host, t.port, t.path FROM \
             (SELECT * FROM routes WHERE created_at > ? \
              ORDER BY created_at DESC, row_id DESC LIMIT ? OFFSET ?) r \
             JOIN route_targets t ON t.route_row_id = r.row_id \
             ORDER BY r.created_at DESC, r.row_id DESC, t.position",
        )
        .bind(page.after_seconds())
        .bind(i64::from(page.limit()))
        .bind(i64::from(page.offset()))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;

        let mut listed: Vec<Listed<RouteRule>> = Vec::new();
        let mut current: Option<i64> = None;
        for row in &rows {
            let route_row_id: i64 = row.get("row_id");
            let target = endpoint(row, "")?;
            if current == Some(route_row_id)
                && let Some(last) = listed.last_mut()
            {
                let mut endpoints = last.item.target.endpoints().to_vec();
                endpoints.push(target);
                last.item.target = RouteTarget::from_endpoints(endpoints)?;
                continue;
            }
            current = Some(route_row_id);
            listed.push(Listed {
                item: RouteRule {
                    api: ApiId::new(row.get("api_id"))?,
                    key: RouteKey::new(endpoint(row, "key_")?),
                    target: RouteTarget::new(target),
                },
                created_at: created_at(row)?,
            });
        }
        Ok(listed)
    }

    async fn list_plugins(
        &self,
        owner: &PluginOwner,
        page: Page,
    ) -> Result<Vec<Listed<PluginRecord>>, StorageError> {
        let tenant_row_id = match owner.tenant() {
            Some(tenant) => Some(
                tenant_row_id(&mut *self.connection().await?, tenant)
                    .await?
                    .get(),
            ),
            None => None,
        };
        let rows = sqlx::query(
            "SELECT p.row_id, p.checksum, p.kind, length(p.wasm) AS size, o.created_at FROM plugin_owners o \
             JOIN plugins p ON p.row_id = o.plugin_row_id \
             WHERE o.tenant_row_id IS ? AND o.created_at > ? \
             ORDER BY o.created_at DESC, p.row_id DESC LIMIT ? OFFSET ?",
        )
        .bind(tenant_row_id)
        .bind(page.after_seconds())
        .bind(i64::from(page.limit()))
        .bind(i64::from(page.offset()))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.iter()
            .map(|row| {
                let record = plugin_record(row)?;
                Ok(Listed {
                    created_at: record.created_at,
                    item: record,
                })
            })
            .collect()
    }

    async fn list_rules(
        &self,
        tenant: Option<&TenantId>,
        page: Page,
    ) -> Result<Vec<Listed<PluginRule>>, StorageError> {
        let query = match tenant {
            Some(tenant) => sqlx::query(
                "SELECT p.checksum, r.kind, r.position, t.id AS tenant_id, u.id AS user_id, \
                 r.api_id, r.created_at FROM plugin_rules r \
                 JOIN plugins p ON p.row_id = r.plugin_row_id \
                 JOIN tenants t ON t.row_id = r.tenant_row_id \
                 LEFT JOIN users u ON u.row_id = r.user_row_id \
                 WHERE t.id = ? AND r.created_at > ? \
                 ORDER BY r.created_at DESC, r.row_id DESC LIMIT ? OFFSET ?",
            )
            .bind(tenant.as_str()),
            None => sqlx::query(
                "SELECT p.checksum, r.kind, r.position, NULL AS tenant_id, NULL AS user_id, \
                 r.api_id, r.created_at FROM plugin_rules r \
                 JOIN plugins p ON p.row_id = r.plugin_row_id \
                 WHERE r.tenant_row_id IS NULL AND r.created_at > ? \
                 ORDER BY r.created_at DESC, r.row_id DESC LIMIT ? OFFSET ?",
            ),
        };
        let rows = query
            .bind(page.after_seconds())
            .bind(i64::from(page.limit()))
            .bind(i64::from(page.offset()))
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::backend)?;
        rows.iter()
            .map(|row| {
                Ok(Listed {
                    item: plugin_rule(row)?,
                    created_at: created_at(row)?,
                })
            })
            .collect()
    }
}
