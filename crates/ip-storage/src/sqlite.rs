use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use ip_core::{
    ApiId, Capability, CapabilityScope, Endpoint, Grant, Grants, PassphraseHash, RouteKey,
    RouteRule, RouteTarget, TenantId, Timestamp, TnKey, UserId,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use ip_core::{Checksum, NewPluginRule, PluginKind, PluginOrder, PluginRule, PluginScope};

use crate::error::{Entity, StorageError};
use crate::model::{
    AccountKind, Membership, NewTenant, NewUser, Standing, Tenant, TenantRowId, User, UserRowId,
};
use crate::plugin::{NewPlugin, Plugin, PluginRecord, PluginRowId, PluginRuleStore, PluginStore};
use crate::route::RouteStore;
use crate::store::{GrantStore, MembershipStore, TenantStore, UserStore};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: u32 = 8;

/// The standalone backend: one local sqlite file.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Opens the database at `path`, creating and migrating it when it is not there yet.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(BUSY_TIMEOUT);
        Self::connect(options, MAX_CONNECTIONS).await
    }

    /// Opens a private database that lives only as long as the store, for tests and dry runs.
    pub async fn in_memory() -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(StorageError::backend)?
            .foreign_keys(true);
        Self::connect(options, 1).await
    }

    async fn connect(
        options: SqliteConnectOptions,
        max_connections: u32,
    ) -> Result<Self, StorageError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .map_err(StorageError::backend)?;
        sqlx::migrate!("./migrations/sqlite")
            .run(&pool)
            .await
            .map_err(StorageError::backend)?;
        Ok(Self { pool })
    }

    /// Closes every pooled connection.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    async fn tenant_row_id(&self, id: &TenantId) -> Result<TenantRowId, StorageError> {
        sqlx::query("SELECT row_id FROM tenants WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .map(|row| TenantRowId::new(row.get("row_id")))
            .ok_or_else(|| StorageError::NotFound {
                entity: Entity::Tenant,
                id: id.to_string(),
            })
    }

    async fn user_row_id(&self, id: &UserId) -> Result<UserRowId, StorageError> {
        sqlx::query("SELECT row_id FROM users WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .map(|row| UserRowId::new(row.get("row_id")))
            .ok_or_else(|| StorageError::NotFound {
                entity: Entity::User,
                id: id.to_string(),
            })
    }
}

fn tenant_key(bytes: Vec<u8>) -> Result<TnKey, StorageError> {
    let len = bytes.len();
    let bytes: [u8; ip_core::key::TN_KEY_BYTES] =
        bytes.try_into().map_err(|_| StorageError::Malformed {
            entity: Entity::Tenant,
            detail: format!("key is {len} bytes"),
        })?;
    Ok(TnKey::from(bytes))
}

fn account_kind_name(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::SystemAdministrator => "sysadmin",
        AccountKind::Regular => "regular",
    }
}

fn account_kind(name: &str) -> Result<AccountKind, StorageError> {
    match name {
        "sysadmin" => Ok(AccountKind::SystemAdministrator),
        "regular" => Ok(AccountKind::Regular),
        other => Err(StorageError::Malformed {
            entity: Entity::User,
            detail: format!("unknown account kind {other:?}"),
        }),
    }
}

fn standing_name(standing: Standing) -> &'static str {
    match standing {
        Standing::Owner => "owner",
        Standing::Member => "member",
    }
}

fn standing(name: &str) -> Result<Standing, StorageError> {
    match name {
        "owner" => Ok(Standing::Owner),
        "member" => Ok(Standing::Member),
        other => Err(StorageError::Malformed {
            entity: Entity::Membership,
            detail: format!("unknown standing {other:?}"),
        }),
    }
}

fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::TenantMgr => "tenant_mgr",
        Capability::UserMgr => "user_mgr",
        Capability::ApiAccess => "api_access",
        Capability::ApiAdvMgr => "api_adv_mgr",
        Capability::LimitMgr => "limit_mgr",
        Capability::SysAgent => "sys_agent",
        Capability::Observer => "observer",
    }
}

fn capability(name: &str) -> Result<Capability, StorageError> {
    match name {
        "tenant_mgr" => Ok(Capability::TenantMgr),
        "user_mgr" => Ok(Capability::UserMgr),
        "api_access" => Ok(Capability::ApiAccess),
        "api_adv_mgr" => Ok(Capability::ApiAdvMgr),
        "limit_mgr" => Ok(Capability::LimitMgr),
        "sys_agent" => Ok(Capability::SysAgent),
        "observer" => Ok(Capability::Observer),
        other => Err(StorageError::Malformed {
            entity: Entity::Grant,
            detail: format!("unknown capability {other:?}"),
        }),
    }
}

/// Rebuilds the scope a grant row was written from, so a nonsensical row is rejected on read.
fn scope(
    user: &UserId,
    tenant: Option<String>,
    api: Option<String>,
) -> Result<CapabilityScope, StorageError> {
    let user = user.clone();
    Ok(match (tenant, api) {
        (None, None) => CapabilityScope::User { user },
        (Some(tenant), None) => CapabilityScope::Tenant {
            user,
            tenant: TenantId::new(&tenant)?,
        },
        (Some(tenant), Some(api)) => CapabilityScope::Api {
            user,
            tenant: TenantId::new(&tenant)?,
            api: ApiId::new(&api)?,
        },
        (None, Some(api)) => {
            return Err(StorageError::Malformed {
                entity: Entity::Grant,
                detail: format!("api {api} is scoped to no tenant"),
            });
        }
    })
}

fn is_foreign_key_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.is_foreign_key_violation())
}

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

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.is_unique_violation())
}

#[async_trait]
impl TenantStore for SqliteStore {
    async fn create_tenant(&self, new: NewTenant) -> Result<Tenant, StorageError> {
        let created_at = Timestamp::now();
        let result = sqlx::query(
            "INSERT INTO tenants (id, key, created_at) VALUES (?, ?, ?) RETURNING row_id",
        )
        .bind(new.id.as_str())
        .bind(new.key.as_bytes().as_slice())
        .bind(created_at.unix_seconds())
        .fetch_one(&self.pool)
        .await;
        match result {
            Ok(row) => Ok(Tenant {
                row_id: TenantRowId::new(row.get("row_id")),
                id: new.id,
                key: new.key,
                created_at,
            }),
            Err(error) if is_unique_violation(&error) => Err(StorageError::Conflict {
                entity: Entity::Tenant,
                id: new.id.to_string(),
            }),
            Err(error) => Err(StorageError::backend(error)),
        }
    }

    async fn tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StorageError> {
        let Some(row) = sqlx::query("SELECT row_id, key, created_at FROM tenants WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::backend)?
        else {
            return Ok(None);
        };
        Ok(Some(Tenant {
            row_id: TenantRowId::new(row.get("row_id")),
            id: id.clone(),
            key: tenant_key(row.get("key"))?,
            created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
        }))
    }

    async fn delete_tenant(&self, id: &TenantId) -> Result<(), StorageError> {
        let deleted = sqlx::query("DELETE FROM tenants WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Tenant,
                id: id.to_string(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl UserStore for SqliteStore {
    async fn create_user(&self, new: NewUser) -> Result<User, StorageError> {
        let created_at = Timestamp::now();
        let result = sqlx::query(
            "INSERT INTO users (id, passphrase, kind, created_at) VALUES (?, ?, ?, ?) RETURNING row_id",
        )
        .bind(new.id.as_str())
        .bind(new.passphrase.as_str())
        .bind(account_kind_name(new.kind))
        .bind(created_at.unix_seconds())
        .fetch_one(&self.pool)
        .await;
        match result {
            Ok(row) => Ok(User {
                row_id: UserRowId::new(row.get("row_id")),
                id: new.id,
                passphrase: new.passphrase,
                kind: new.kind,
                created_at,
            }),
            Err(error) if is_unique_violation(&error) => Err(StorageError::Conflict {
                entity: Entity::User,
                id: new.id.to_string(),
            }),
            Err(error) => Err(StorageError::backend(error)),
        }
    }

    async fn user(&self, id: &UserId) -> Result<Option<User>, StorageError> {
        let Some(row) =
            sqlx::query("SELECT row_id, passphrase, kind, created_at FROM users WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(StorageError::backend)?
        else {
            return Ok(None);
        };
        Ok(Some(User {
            row_id: UserRowId::new(row.get("row_id")),
            id: id.clone(),
            passphrase: PassphraseHash::new(row.get("passphrase"))?,
            kind: account_kind(row.get("kind"))?,
            created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
        }))
    }

    async fn set_passphrase(
        &self,
        id: &UserId,
        passphrase: &PassphraseHash,
    ) -> Result<(), StorageError> {
        let updated = sqlx::query("UPDATE users SET passphrase = ? WHERE id = ?")
            .bind(passphrase.as_str())
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        if updated == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::User,
                id: id.to_string(),
            });
        }
        Ok(())
    }

    async fn delete_user(&self, id: &UserId) -> Result<(), StorageError> {
        let deleted = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(StorageError::backend)?
            .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::User,
                id: id.to_string(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl MembershipStore for SqliteStore {
    async fn attach(&self, membership: Membership) -> Result<(), StorageError> {
        let tenant_row_id = self.tenant_row_id(&membership.tenant).await?;
        let user_row_id = self.user_row_id(&membership.user).await?;
        sqlx::query(
            "INSERT INTO memberships (tenant_row_id, user_row_id, standing) VALUES (?, ?, ?) \
             ON CONFLICT (tenant_row_id, user_row_id) DO UPDATE SET standing = excluded.standing",
        )
        .bind(tenant_row_id.get())
        .bind(user_row_id.get())
        .bind(standing_name(membership.standing))
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        Ok(())
    }

    async fn detach(&self, user: &UserId, tenant: &TenantId) -> Result<(), StorageError> {
        let deleted = sqlx::query(
            "DELETE FROM memberships WHERE tenant_row_id = (SELECT row_id FROM tenants WHERE id = ?) \
             AND user_row_id = (SELECT row_id FROM users WHERE id = ?)",
        )
        .bind(tenant.as_str())
        .bind(user.as_str())
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Membership,
                id: format!("{user}@{tenant}"),
            });
        }
        Ok(())
    }

    async fn membership(
        &self,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Option<Membership>, StorageError> {
        let Some(row) = sqlx::query(
            "SELECT m.standing FROM memberships m \
             JOIN tenants t ON t.row_id = m.tenant_row_id \
             JOIN users u ON u.row_id = m.user_row_id \
             WHERE t.id = ? AND u.id = ?",
        )
        .bind(tenant.as_str())
        .bind(user.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::backend)?
        else {
            return Ok(None);
        };
        Ok(Some(Membership {
            user: user.clone(),
            tenant: tenant.clone(),
            standing: standing(row.get("standing"))?,
        }))
    }

    async fn memberships_of_user(&self, user: &UserId) -> Result<Vec<Membership>, StorageError> {
        let rows = sqlx::query(
            "SELECT t.id AS tenant_id, m.standing FROM memberships m \
             JOIN tenants t ON t.row_id = m.tenant_row_id \
             JOIN users u ON u.row_id = m.user_row_id \
             WHERE u.id = ? ORDER BY t.id",
        )
        .bind(user.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| {
                Ok(Membership {
                    user: user.clone(),
                    tenant: TenantId::new(row.get("tenant_id"))?,
                    standing: standing(row.get("standing"))?,
                })
            })
            .collect()
    }

    async fn members_of_tenant(&self, tenant: &TenantId) -> Result<Vec<Membership>, StorageError> {
        let rows = sqlx::query(
            "SELECT u.id AS user_id, m.standing FROM memberships m \
             JOIN tenants t ON t.row_id = m.tenant_row_id \
             JOIN users u ON u.row_id = m.user_row_id \
             WHERE t.id = ? ORDER BY u.id",
        )
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| {
                Ok(Membership {
                    user: UserId::new(row.get("user_id"))?,
                    tenant: tenant.clone(),
                    standing: standing(row.get("standing"))?,
                })
            })
            .collect()
    }
}

#[async_trait]
impl GrantStore for SqliteStore {
    async fn grant(&self, grant: &Grant) -> Result<(), StorageError> {
        let user_row_id = self.user_row_id(grant.scope().user()).await?;
        let tenant_row_id = match grant.scope().tenant() {
            Some(tenant) => Some(self.tenant_row_id(tenant).await?.get()),
            None => None,
        };
        let result = sqlx::query(
            "INSERT INTO grants (user_row_id, tenant_row_id, api_id, capability) VALUES (?, ?, ?, ?)",
        )
        .bind(user_row_id.get())
        .bind(tenant_row_id)
        .bind(grant.scope().api().map(|api| api.as_str()))
        .bind(capability_name(grant.capability()))
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if is_unique_violation(&error) => Ok(()),
            Err(error) => Err(StorageError::backend(error)),
        }
    }

    async fn revoke(&self, grant: &Grant) -> Result<(), StorageError> {
        let user_row_id = self.user_row_id(grant.scope().user()).await?;
        let tenant_row_id = match grant.scope().tenant() {
            Some(tenant) => Some(self.tenant_row_id(tenant).await?.get()),
            None => None,
        };
        let deleted = sqlx::query(
            "DELETE FROM grants WHERE user_row_id = ? AND tenant_row_id IS ? \
             AND api_id IS ? AND capability = ?",
        )
        .bind(user_row_id.get())
        .bind(tenant_row_id)
        .bind(grant.scope().api().map(|api| api.as_str()))
        .bind(capability_name(grant.capability()))
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Grant,
                id: format!("{:?}", grant.capability()),
            });
        }
        Ok(())
    }

    async fn grants_of(&self, user: &UserId) -> Result<Grants, StorageError> {
        let rows = sqlx::query(
            "SELECT t.id AS tenant_id, g.api_id, g.capability FROM grants g \
             JOIN users u ON u.row_id = g.user_row_id \
             LEFT JOIN tenants t ON t.row_id = g.tenant_row_id \
             WHERE u.id = ?",
        )
        .bind(user.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| {
                let scope = scope(user, row.get("tenant_id"), row.get("api_id"))?;
                Ok(Grant::new(capability(row.get("capability"))?, scope)?)
            })
            .collect()
    }

    async fn grants_in_tenant(
        &self,
        user: &UserId,
        tenant: &TenantId,
    ) -> Result<Grants, StorageError> {
        let rows = sqlx::query(
            "SELECT t.id AS tenant_id, g.api_id, g.capability FROM grants g \
             JOIN users u ON u.row_id = g.user_row_id \
             JOIN tenants t ON t.row_id = g.tenant_row_id \
             WHERE u.id = ? AND t.id = ?",
        )
        .bind(user.as_str())
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        rows.into_iter()
            .map(|row| {
                let scope = scope(user, row.get("tenant_id"), row.get("api_id"))?;
                Ok(Grant::new(capability(row.get("capability"))?, scope)?)
            })
            .collect()
    }
}

#[async_trait]
impl RouteStore for SqliteStore {
    async fn put_route(&self, rule: RouteRule) -> Result<(), StorageError> {
        let key = rule.key.endpoint();
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        let route_row_id: i64 = sqlx::query_scalar(
            "INSERT INTO routes (api_id, key_protocol, key_host, key_port, key_path) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (key_protocol, key_host, key_port, key_path) DO UPDATE SET \
             api_id = excluded.api_id \
             RETURNING row_id",
        )
        .bind(rule.api.as_str())
        .bind(key.protocol.name())
        .bind(key.host.as_str())
        .bind(i64::from(key.port.get()))
        .bind(key.path.as_str())
        .fetch_one(&mut *transaction)
        .await
        .map_err(StorageError::backend)?;

        sqlx::query("DELETE FROM route_targets WHERE route_row_id = ?")
            .bind(route_row_id)
            .execute(&mut *transaction)
            .await
            .map_err(StorageError::backend)?;

        for (position, endpoint) in rule.target.endpoints().iter().enumerate() {
            sqlx::query(
                "INSERT INTO route_targets (route_row_id, position, protocol, host, port, path) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(route_row_id)
            .bind(i64::try_from(position).unwrap_or(i64::MAX))
            .bind(endpoint.protocol.name())
            .bind(endpoint.host.as_str())
            .bind(i64::from(endpoint.port.get()))
            .bind(endpoint.path.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(StorageError::backend)?;
        }

        transaction.commit().await.map_err(StorageError::backend)
    }

    async fn remove_route(&self, key: &RouteKey) -> Result<(), StorageError> {
        let endpoint = key.endpoint();
        let deleted = sqlx::query(
            "DELETE FROM routes WHERE key_protocol = ? AND key_host = ? \
             AND key_port = ? AND key_path = ?",
        )
        .bind(endpoint.protocol.name())
        .bind(endpoint.host.as_str())
        .bind(i64::from(endpoint.port.get()))
        .bind(endpoint.path.as_str())
        .execute(&self.pool)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
        if deleted == 0 {
            return Err(StorageError::NotFound {
                entity: Entity::Route,
                id: key.to_string(),
            });
        }
        Ok(())
    }

    async fn route(&self, key: &RouteKey) -> Result<Option<RouteRule>, StorageError> {
        let endpoint = key.endpoint();
        let rows = sqlx::query(
            "SELECT r.api_id, t.protocol, t.host, t.port, t.path FROM routes r \
             JOIN route_targets t ON t.route_row_id = r.row_id \
             WHERE r.key_protocol = ? AND r.key_host = ? AND r.key_port = ? AND r.key_path = ? \
             ORDER BY t.position",
        )
        .bind(endpoint.protocol.name())
        .bind(endpoint.host.as_str())
        .bind(i64::from(endpoint.port.get()))
        .bind(endpoint.path.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let api = ApiId::new(first.get("api_id"))?;
        let targets = rows.iter().map(target_of).collect::<Result<Vec<_>, _>>()?;
        Ok(Some(RouteRule {
            api,
            key: key.clone(),
            target: RouteTarget::from_endpoints(targets)?,
        }))
    }

    async fn routes(&self) -> Result<Vec<RouteRule>, StorageError> {
        let rows = sqlx::query(
            "SELECT r.row_id, r.api_id, r.key_protocol, r.key_host, r.key_port, r.key_path, \
             t.protocol, t.host, t.port, t.path FROM routes r \
             JOIN route_targets t ON t.route_row_id = r.row_id \
             ORDER BY r.key_host, r.key_path, r.key_protocol, r.key_port, t.position",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::backend)?;

        let mut rules: Vec<RouteRule> = Vec::new();
        let mut current: Option<i64> = None;
        for row in &rows {
            let route_row_id: i64 = row.get("row_id");
            let endpoint = target_of(row)?;
            if current == Some(route_row_id)
                && let Some(rule) = rules.last_mut()
            {
                let mut endpoints = rule.target.endpoints().to_vec();
                endpoints.push(endpoint);
                rule.target = RouteTarget::from_endpoints(endpoints)?;
                continue;
            }
            current = Some(route_row_id);
            rules.push(RouteRule {
                api: ApiId::new(row.get("api_id"))?,
                key: RouteKey::new(Endpoint::from_parts(
                    row.get("key_protocol"),
                    row.get("key_host"),
                    row.get("key_port"),
                    row.get("key_path"),
                )?),
                target: RouteTarget::new(endpoint),
            });
        }
        Ok(rules)
    }
}

fn target_of(row: &sqlx::sqlite::SqliteRow) -> Result<Endpoint, StorageError> {
    Ok(Endpoint::from_parts(
        row.get("protocol"),
        row.get("host"),
        row.get("port"),
        row.get("path"),
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
    use ip_core::{AbsPath, Host, Port, Protocol};

    use super::*;

    async fn store() -> SqliteStore {
        SqliteStore::in_memory().await.unwrap()
    }

    fn tenant_id() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn user_id() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn hash() -> PassphraseHash {
        PassphraseHash::new("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA").unwrap()
    }

    fn new_tenant() -> NewTenant {
        NewTenant {
            id: tenant_id(),
            key: TnKey::generate().unwrap(),
        }
    }

    fn new_user() -> NewUser {
        NewUser {
            id: user_id(),
            passphrase: hash(),
            kind: AccountKind::Regular,
        }
    }

    async fn tenant_with_user(store: &SqliteStore) -> (Tenant, User) {
        let tenant = store.create_tenant(new_tenant()).await.unwrap();
        let user = store.create_user(new_user()).await.unwrap();
        (tenant, user)
    }

    fn api_grant(user: &UserId, tenant: &TenantId, capability: Capability) -> Grant {
        Grant::new(
            capability,
            CapabilityScope::Api {
                user: user.clone(),
                tenant: tenant.clone(),
                api: ApiId::new("chat").unwrap(),
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_fresh_database_is_migrated_and_empty() {
        let store = store().await;
        assert_eq!(store.tenant(&tenant_id()).await.unwrap(), None);
        assert_eq!(store.user(&user_id()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_tenant_round_trips_with_its_key_intact() {
        let store = store().await;
        let created = store.create_tenant(new_tenant()).await.unwrap();
        let read = store.tenant(&tenant_id()).await.unwrap().unwrap();
        assert_eq!(read.row_id, created.row_id);
        assert_eq!(read.key, created.key);
        assert_eq!(read.created_at, created.created_at);
    }

    #[tokio::test]
    async fn the_primary_key_is_an_integer_the_backend_assigns() {
        let store = store().await;
        let first = store.create_tenant(new_tenant()).await.unwrap();
        let second = store
            .create_tenant(NewTenant {
                id: TenantId::new("globex").unwrap(),
                key: TnKey::generate().unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(first.row_id, TenantRowId::new(1));
        assert_eq!(second.row_id, TenantRowId::new(2));
    }

    #[tokio::test]
    async fn a_repeated_tenant_id_conflicts() {
        let store = store().await;
        store.create_tenant(new_tenant()).await.unwrap();
        assert!(matches!(
            store.create_tenant(new_tenant()).await,
            Err(StorageError::Conflict {
                entity: Entity::Tenant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_repeated_user_id_conflicts() {
        let store = store().await;
        store.create_user(new_user()).await.unwrap();
        assert!(matches!(
            store.create_user(new_user()).await,
            Err(StorageError::Conflict {
                entity: Entity::User,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_user_round_trips_and_its_passphrase_can_be_replaced() {
        let store = store().await;
        let created = store.create_user(new_user()).await.unwrap();
        assert_eq!(store.user(&user_id()).await.unwrap(), Some(created));
        let replacement =
            PassphraseHash::new("$argon2id$v=19$m=19456,t=2,p=1$b3RoZXI$b3RoZXJoYXNo").unwrap();
        store
            .set_passphrase(&user_id(), &replacement)
            .await
            .unwrap();
        assert_eq!(
            store.user(&user_id()).await.unwrap().unwrap().passphrase,
            replacement
        );
    }

    #[tokio::test]
    async fn the_account_kind_survives_a_round_trip() {
        let store = store().await;
        store
            .create_user(NewUser {
                id: user_id(),
                passphrase: hash(),
                kind: AccountKind::SystemAdministrator,
            })
            .await
            .unwrap();
        assert_eq!(
            store.user(&user_id()).await.unwrap().unwrap().kind,
            AccountKind::SystemAdministrator
        );
    }

    #[tokio::test]
    async fn deleting_what_is_absent_reports_it_missing() {
        let store = store().await;
        assert!(matches!(
            store.delete_tenant(&tenant_id()).await,
            Err(StorageError::NotFound {
                entity: Entity::Tenant,
                ..
            })
        ));
        assert!(matches!(
            store.set_passphrase(&user_id(), &hash()).await,
            Err(StorageError::NotFound {
                entity: Entity::User,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn attaching_twice_replaces_the_standing() {
        let store = store().await;
        tenant_with_user(&store).await;
        for standing in [Standing::Owner, Standing::Member] {
            store
                .attach(Membership {
                    user: user_id(),
                    tenant: tenant_id(),
                    standing,
                })
                .await
                .unwrap();
        }
        let held = store.memberships_of_user(&user_id()).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].standing, Standing::Member);
    }

    #[tokio::test]
    async fn attaching_to_a_tenant_that_is_not_there_reports_it_missing() {
        let store = store().await;
        store.create_user(new_user()).await.unwrap();
        assert!(matches!(
            store
                .attach(Membership {
                    user: user_id(),
                    tenant: tenant_id(),
                    standing: Standing::Member,
                })
                .await,
            Err(StorageError::NotFound {
                entity: Entity::Tenant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_tenant_is_listed_from_both_sides() {
        let store = store().await;
        tenant_with_user(&store).await;
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing: Standing::Owner,
            })
            .await
            .unwrap();
        assert_eq!(
            store.memberships_of_user(&user_id()).await.unwrap().len(),
            1
        );
        assert_eq!(
            store.members_of_tenant(&tenant_id()).await.unwrap()[0].user,
            user_id()
        );
        assert_eq!(
            store
                .membership(&user_id(), &tenant_id())
                .await
                .unwrap()
                .unwrap()
                .standing,
            Standing::Owner
        );
    }

    #[tokio::test]
    async fn deleting_a_tenant_takes_its_memberships_and_grants_with_it() {
        let store = store().await;
        tenant_with_user(&store).await;
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing: Standing::Member,
            })
            .await
            .unwrap();
        store
            .grant(&api_grant(&user_id(), &tenant_id(), Capability::ApiAccess))
            .await
            .unwrap();
        store.delete_tenant(&tenant_id()).await.unwrap();
        assert!(
            store
                .memberships_of_user(&user_id())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.grants_of(&user_id()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn granting_the_same_capability_twice_changes_nothing() {
        let store = store().await;
        tenant_with_user(&store).await;
        let grant = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
        store.grant(&grant).await.unwrap();
        store.grant(&grant).await.unwrap();
        assert_eq!(store.grants_of(&user_id()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn every_scope_shape_round_trips() {
        let store = store().await;
        tenant_with_user(&store).await;
        let user_scoped = Grant::new(
            Capability::TenantMgr,
            CapabilityScope::User { user: user_id() },
        )
        .unwrap();
        let tenant_scoped = Grant::new(
            Capability::UserMgr,
            CapabilityScope::Tenant {
                user: user_id(),
                tenant: tenant_id(),
            },
        )
        .unwrap();
        let api_scoped = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
        for grant in [&user_scoped, &tenant_scoped, &api_scoped] {
            store.grant(grant).await.unwrap();
        }
        let held = store.grants_of(&user_id()).await.unwrap();
        assert_eq!(held.len(), 3);
        assert!(held.holds(Capability::TenantMgr, user_scoped.scope()));
        assert!(held.holds(Capability::UserMgr, tenant_scoped.scope()));
        assert!(held.holds(Capability::ApiAccess, api_scoped.scope()));
    }

    #[tokio::test]
    async fn grants_in_a_tenant_exclude_the_user_scoped_ones() {
        let store = store().await;
        tenant_with_user(&store).await;
        store
            .grant(
                &Grant::new(
                    Capability::TenantMgr,
                    CapabilityScope::User { user: user_id() },
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let api_scoped = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
        store.grant(&api_scoped).await.unwrap();
        let held = store
            .grants_in_tenant(&user_id(), &tenant_id())
            .await
            .unwrap();
        assert_eq!(held.len(), 1);
        assert!(held.holds(Capability::ApiAccess, api_scoped.scope()));
    }

    #[tokio::test]
    async fn revoking_what_is_not_held_reports_it_missing() {
        let store = store().await;
        tenant_with_user(&store).await;
        assert!(matches!(
            store
                .revoke(&api_grant(&user_id(), &tenant_id(), Capability::ApiAccess))
                .await,
            Err(StorageError::NotFound {
                entity: Entity::Grant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn revoking_removes_only_the_named_grant() {
        let store = store().await;
        tenant_with_user(&store).await;
        let access = api_grant(&user_id(), &tenant_id(), Capability::ApiAccess);
        let limits = api_grant(&user_id(), &tenant_id(), Capability::LimitMgr);
        store.grant(&access).await.unwrap();
        store.grant(&limits).await.unwrap();
        store.revoke(&limits).await.unwrap();
        let held = store.grants_of(&user_id()).await.unwrap();
        assert_eq!(held.len(), 1);
        assert!(held.holds(Capability::ApiAccess, access.scope()));
    }

    #[tokio::test]
    async fn a_capability_the_code_does_not_know_is_reported_as_malformed() {
        let store = store().await;
        let (_, user) = tenant_with_user(&store).await;
        sqlx::query("INSERT INTO grants (user_row_id, capability) VALUES (?, 'wat')")
            .bind(user.row_id.get())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.grants_of(&user_id()).await,
            Err(StorageError::Malformed {
                entity: Entity::Grant,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_api_grant_with_no_tenant_is_reported_as_malformed() {
        let store = store().await;
        let (_, user) = tenant_with_user(&store).await;
        sqlx::query(
            "INSERT INTO grants (user_row_id, api_id, capability) VALUES (?, 'chat', 'api_access')",
        )
        .bind(user.row_id.get())
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(matches!(
            store.grants_of(&user_id()).await,
            Err(StorageError::Malformed {
                entity: Entity::Grant,
                ..
            })
        ));
    }

    fn route_endpoint(protocol: Protocol, host: &str, port: u16, path: &str) -> Endpoint {
        Endpoint::new(
            protocol,
            Host::new(host).unwrap(),
            Port::new(port).unwrap(),
            AbsPath::new(path).unwrap(),
        )
    }

    fn rule(host: &str, path: &str, upstream: &str) -> RouteRule {
        RouteRule {
            api: ApiId::new("anthropic").unwrap(),
            key: RouteKey::new(route_endpoint(Protocol::Https, host, 443, path)),
            target: RouteTarget::new(route_endpoint(Protocol::Https, upstream, 443, path)),
        }
    }

    #[tokio::test]
    async fn a_route_round_trips() {
        let store = store().await;
        let rule = rule("gateway.local", "/v1/messages", "api.example.com");
        store.put_route(rule.clone()).await.unwrap();
        assert_eq!(store.route(&rule.key).await.unwrap(), Some(rule.clone()));
        assert_eq!(store.routes().await.unwrap(), vec![rule]);
    }

    /// Writes a route around the driver, so a row the code could not have written is read back.
    async fn broken_route(store: &SqliteStore, key_protocol: &str, protocol: &str, port: i64) {
        let route_row_id: i64 = sqlx::query_scalar(
            "INSERT INTO routes (api_id, key_protocol, key_host, key_port, key_path) \
             VALUES ('anthropic', ?, 'gateway.local', 443, '/v1') RETURNING row_id",
        )
        .bind(key_protocol)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO route_targets (route_row_id, position, protocol, host, port, path) \
             VALUES (?, 0, ?, 'api.example.com', ?, '/v1')",
        )
        .bind(route_row_id)
        .bind(protocol)
        .bind(port)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_route_names_the_api_it_serves() {
        let store = store().await;
        let mut rule = rule("gateway.local", "/internal", "llm.corp");
        rule.api = ApiId::new("internal").unwrap();
        store.put_route(rule.clone()).await.unwrap();
        assert_eq!(
            store.route(&rule.key).await.unwrap().unwrap().api,
            ApiId::new("internal").unwrap()
        );
        assert_eq!(store.routes().await.unwrap()[0].api, rule.api);
    }

    #[tokio::test]
    async fn writing_the_same_route_key_replaces_its_target() {
        let store = store().await;
        store
            .put_route(rule("gateway.local", "/v1", "first.example.com"))
            .await
            .unwrap();
        let second = rule("gateway.local", "/v1", "second.example.com");
        store.put_route(second.clone()).await.unwrap();
        assert_eq!(store.routes().await.unwrap(), vec![second]);
    }

    #[tokio::test]
    async fn a_route_key_carries_every_part_of_the_tuple() {
        let store = store().await;
        let secure = rule("gateway.local", "/v1", "api.example.com");
        let plain = RouteRule {
            api: ApiId::new("anthropic").unwrap(),
            key: RouteKey::new(route_endpoint(Protocol::Http, "gateway.local", 443, "/v1")),
            target: RouteTarget::new(route_endpoint(
                Protocol::Https,
                "api.example.com",
                443,
                "/v1",
            )),
        };
        store.put_route(secure).await.unwrap();
        store.put_route(plain).await.unwrap();
        assert_eq!(store.routes().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn every_protocol_survives_a_round_trip() {
        let store = store().await;
        for protocol in [
            Protocol::Http,
            Protocol::Https,
            Protocol::Ws,
            Protocol::Wss,
            Protocol::Tcp,
        ] {
            let key = RouteKey::new(route_endpoint(protocol, "gateway.local", 443, "/v1"));
            store
                .put_route(RouteRule {
                    api: ApiId::new("anthropic").unwrap(),
                    key: key.clone(),
                    target: RouteTarget::new(route_endpoint(
                        protocol,
                        "api.example.com",
                        443,
                        "/v1",
                    )),
                })
                .await
                .unwrap();
            let read = store.route(&key).await.unwrap().unwrap();
            assert_eq!(read.target.primary().protocol, protocol);
        }
    }

    #[tokio::test]
    async fn removing_a_route_that_is_absent_reports_it_missing() {
        let store = store().await;
        let key = RouteKey::new(route_endpoint(Protocol::Https, "gateway.local", 443, "/v1"));
        assert!(matches!(
            store.remove_route(&key).await,
            Err(StorageError::NotFound {
                entity: Entity::Route,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_route_may_stand_for_several_endpoints() {
        let store = store().await;
        let mut rule = rule("gateway.local", "/v1", "one.example.com");
        rule.target = RouteTarget::from_endpoints(vec![
            route_endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
            route_endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
            route_endpoint(Protocol::Http, "127.0.0.1", 9000, "/v1"),
        ])
        .unwrap();
        store.put_route(rule.clone()).await.unwrap();

        let read = store.route(&rule.key).await.unwrap().unwrap();
        assert_eq!(read, rule);
        assert_eq!(read.target.endpoints().len(), 3);
        assert_eq!(read.target.primary().host.as_str(), "one.example.com");
        assert_eq!(store.routes().await.unwrap(), vec![rule]);
    }

    #[tokio::test]
    async fn rewriting_a_route_replaces_its_whole_endpoint_list() {
        let store = store().await;
        let mut replicated = rule("gateway.local", "/v1", "one.example.com");
        replicated.target = RouteTarget::from_endpoints(vec![
            route_endpoint(Protocol::Https, "one.example.com", 443, "/v1"),
            route_endpoint(Protocol::Https, "two.example.com", 443, "/v1"),
        ])
        .unwrap();
        store.put_route(replicated).await.unwrap();

        let narrowed = rule("gateway.local", "/v1", "three.example.com");
        store.put_route(narrowed.clone()).await.unwrap();
        assert_eq!(store.routes().await.unwrap(), vec![narrowed]);
    }

    #[tokio::test]
    async fn removing_a_route_takes_its_endpoints_with_it() {
        let store = store().await;
        let rule = rule("gateway.local", "/v1", "one.example.com");
        store.put_route(rule.clone()).await.unwrap();
        store.remove_route(&rule.key).await.unwrap();
        let orphans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_targets")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[tokio::test]
    async fn a_stored_protocol_the_code_does_not_know_is_refused() {
        let store = store().await;
        broken_route(&store, "gopher", "https", 443).await;
        assert!(matches!(
            store.routes().await,
            Err(StorageError::Value(
                ip_core::CoreError::UnknownProtocol { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn a_stored_port_of_zero_is_refused_on_read() {
        let store = store().await;
        broken_route(&store, "https", "https", 0).await;
        assert!(matches!(
            store.routes().await,
            Err(StorageError::Value(ip_core::CoreError::ZeroPort))
        ));
    }

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
