//! How a value every sql backend shares is spelled in the columns that hold it.

use ip_core::{
    ApiId, Capability, CapabilityScope, Decider, Dialect, Latency, Money, PluginOrder, PluginScope,
    Redacted, Served, TenantId, TnKey, TokenCount, UserId,
};
use ip_mcp::{McpAuth, McpError, McpToken};
use ip_retrieve::{RetrieveError, RetrieverAuth, RetrieverKey};

use crate::error::{Entity, StorageError};
use crate::model::{AccountKind, Standing};

/// Rebuilds a tenant key from its stored bytes.
pub(crate) fn tenant_key(bytes: Vec<u8>) -> Result<TnKey, StorageError> {
    let len = bytes.len();
    let bytes: [u8; ip_core::key::TN_KEY_BYTES] =
        bytes.try_into().map_err(|_| StorageError::Malformed {
            entity: Entity::Tenant,
            detail: format!("key is {len} bytes"),
        })?;
    Ok(TnKey::from(bytes))
}

/// The stored name of an account kind.
pub(crate) fn account_kind_name(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::SystemAdministrator => "sysadmin",
        AccountKind::Regular => "regular",
    }
}

/// Reads an account kind back from its stored name.
pub(crate) fn account_kind(name: &str) -> Result<AccountKind, StorageError> {
    match name {
        "sysadmin" => Ok(AccountKind::SystemAdministrator),
        "regular" => Ok(AccountKind::Regular),
        other => Err(StorageError::Malformed {
            entity: Entity::User,
            detail: format!("unknown account kind {other:?}"),
        }),
    }
}

/// The stored name of a standing.
pub(crate) fn standing_name(standing: Standing) -> &'static str {
    match standing {
        Standing::Owner => "owner",
        Standing::Member => "member",
    }
}

/// Reads a standing back from its stored name.
pub(crate) fn standing(name: &str) -> Result<Standing, StorageError> {
    match name {
        "owner" => Ok(Standing::Owner),
        "member" => Ok(Standing::Member),
        other => Err(StorageError::Malformed {
            entity: Entity::Membership,
            detail: format!("unknown standing {other:?}"),
        }),
    }
}

/// The stored name of a capability.
pub(crate) fn capability_name(capability: Capability) -> &'static str {
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

/// Reads a capability back from its stored name.
pub(crate) fn capability(name: &str) -> Result<Capability, StorageError> {
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
pub(crate) fn scope(
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

/// Reads a plugin's stored size back as a length.
pub(crate) fn plugin_size(size: i64) -> Result<usize, StorageError> {
    usize::try_from(size).map_err(|_| StorageError::Malformed {
        entity: Entity::Plugin,
        detail: format!("size {size} is not a length"),
    })
}

/// Reads a rule's stored position back as an order.
pub(crate) fn plugin_order(position: i64) -> Result<PluginOrder, StorageError> {
    let order = u8::try_from(position).map_err(|_| StorageError::Malformed {
        entity: Entity::PluginRule,
        detail: format!("position {position} is not an order"),
    })?;
    Ok(PluginOrder::new(order))
}

/// Rebuilds the scope a rule row was written for, refusing a global row that names a user or api.
pub(crate) fn plugin_scope(
    tenant: Option<String>,
    user: Option<String>,
    api: Option<String>,
) -> Result<PluginScope, StorageError> {
    match tenant {
        None if user.is_some() || api.is_some() => Err(StorageError::Malformed {
            entity: Entity::PluginRule,
            detail: "a global rule names a user or an api".to_owned(),
        }),
        None => Ok(PluginScope::Global),
        Some(tenant) => Ok(PluginScope::Tenant {
            tenant: TenantId::new(&tenant)?,
            user: user.as_deref().map(UserId::new).transpose()?,
            api: api.as_deref().map(ApiId::new).transpose()?,
        }),
    }
}

/// The stored name of where an answer came from.
pub(crate) fn served_name(served: Served) -> &'static str {
    match served {
        Served::Upstream => "upstream",
        Served::Cache => "cache",
        Served::Semantic => "semantic",
    }
}

/// Reads back where an answer came from.
pub(crate) fn served(name: &str) -> Result<Served, StorageError> {
    match name {
        "upstream" => Ok(Served::Upstream),
        "cache" => Ok(Served::Cache),
        "semantic" => Ok(Served::Semantic),
        other => Err(StorageError::Malformed {
            entity: Entity::Usage,
            detail: format!("unknown answer source {other:?}"),
        }),
    }
}

/// The stored name of which level named a row's class.
pub(crate) fn decided_by_name(decided_by: Option<Decider>) -> Option<&'static str> {
    decided_by.map(Decider::name)
}

/// Reads back which level named a row's class, refusing a name this code does not carry.
pub(crate) fn decided_by(named: Option<String>) -> Result<Option<Decider>, StorageError> {
    named
        .map(|name| Decider::named(&name))
        .transpose()
        .map_err(StorageError::from)
}

/// Reads back what a filter took out of a request, refusing a count with no kinds
/// named and kinds with nothing counted: the two describe each other, and a row
/// holding one without the other describes nothing.
pub(crate) fn redacted(
    count: Option<i64>,
    kinds: Option<String>,
) -> Result<Redacted, StorageError> {
    match (count, kinds) {
        (None, None) => Ok(Redacted::NONE),
        (Some(count), Some(kinds)) => {
            let count = u32::try_from(count).map_err(|_| StorageError::Malformed {
                entity: Entity::Usage,
                detail: format!("a redaction of {count} is not a count"),
            })?;
            Redacted::of(count, &kinds).map_err(|error| StorageError::Malformed {
                entity: Entity::Usage,
                detail: error.to_string(),
            })
        }
        (count, kinds) => Err(StorageError::Malformed {
            entity: Entity::Usage,
            detail: format!("a redaction of {count:?} naming {kinds:?} describes nothing"),
        }),
    }
}

/// What a row cost as its column holds it, refusing a sum past what the column takes.
pub(crate) fn cost_micros(cost: Option<Money>) -> Result<Option<i64>, StorageError> {
    cost.map(|cost| {
        i64::try_from(cost.micros()).map_err(|_| StorageError::Malformed {
            entity: Entity::Usage,
            detail: format!("a cost of {cost} is past what a column holds"),
        })
    })
    .transpose()
}

/// Reads back what a row cost, refusing a negative sum, which no cost is.
pub(crate) fn cost(micros: Option<i64>) -> Result<Option<Money>, StorageError> {
    micros
        .map(|micros| {
            u64::try_from(micros)
                .map(Money::from_micros)
                .map_err(|_| StorageError::Malformed {
                    entity: Entity::Usage,
                    detail: format!("a cost of {micros} micros is not a cost"),
                })
        })
        .transpose()
}

/// The stored name of a provider family.
pub(crate) fn dialect_name(dialect: Option<Dialect>) -> Option<&'static str> {
    dialect.map(Dialect::name)
}

/// Reads a provider family back, refusing a name this code does not carry.
pub(crate) fn dialect(named: Option<String>) -> Result<Option<Dialect>, StorageError> {
    named
        .map(|name| Dialect::named(&name))
        .transpose()
        .map_err(StorageError::from)
}

/// Reads a stored token count back.
pub(crate) fn token_count(count: i64) -> Result<TokenCount, StorageError> {
    let count = u32::try_from(count).map_err(|_| StorageError::Malformed {
        entity: Entity::Usage,
        detail: format!("{count} is not a token count"),
    })?;
    Ok(TokenCount::new(count))
}

/// Reads a stored span back as a latency.
pub(crate) fn latency(millis: i64) -> Result<Latency, StorageError> {
    let millis = u128::try_from(millis).map_err(|_| StorageError::Malformed {
        entity: Entity::Usage,
        detail: format!("{millis} is not a span of milliseconds"),
    })?;
    Ok(Latency::from_millis(millis))
}

/// Rebuilds what an MCP server requires from the two columns that hold it.
///
/// The kind and the token are stored apart and no constraint pairs them, so a pair that
/// disagrees is a row a database will hold and this is what refuses it.
pub(crate) fn mcp_auth(kind: &str, token: Option<String>) -> Result<McpAuth, StorageError> {
    match (kind, token) {
        ("none", None) => Ok(McpAuth::None),
        ("bearer", Some(token)) => Ok(McpAuth::Bearer {
            token: McpToken::new(&token).map_err(mcp_refused)?,
        }),
        ("none" | "bearer", _) => Err(mcp_refused(McpError::TokenWithoutBearer)),
        (other, _) => Err(StorageError::Malformed {
            entity: Entity::Mcp,
            detail: format!("unknown credential kind {other:?}"),
        }),
    }
}

/// A stored value an MCP server's own type refuses, reported against the server it was read for.
pub(crate) fn mcp_refused(error: McpError) -> StorageError {
    StorageError::Malformed {
        entity: Entity::Mcp,
        detail: error.to_string(),
    }
}

/// Rebuilds what a retriever's backend requires from the two columns that hold it.
///
/// The same shape as an MCP server's, and for the same reason: the kind and the key are stored
/// apart with no constraint pairing them, so a pair that disagrees is a row a database will hold.
pub(crate) fn retriever_auth(
    kind: &str,
    key: Option<String>,
) -> Result<RetrieverAuth, StorageError> {
    match (kind, key) {
        ("none", None) => Ok(RetrieverAuth::None),
        ("key", Some(key)) => Ok(RetrieverAuth::Key {
            key: RetrieverKey::new(&key).map_err(retriever_refused)?,
        }),
        ("none" | "key", _) => Err(retriever_refused(RetrieveError::MisplacedKey)),
        (other, _) => Err(StorageError::Malformed {
            entity: Entity::Retriever,
            detail: format!("unknown credential kind {other:?}"),
        }),
    }
}

/// A stored value a retriever's own type refuses, reported against the retriever it was read for.
pub(crate) fn retriever_refused(error: RetrieveError) -> StorageError {
    StorageError::Malformed {
        entity: Entity::Retriever,
        detail: error.to_string(),
    }
}
