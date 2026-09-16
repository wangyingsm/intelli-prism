//! How identity and plugin values are spelled in the columns every sql backend stores them in.

use ip_core::{
    ApiId, Capability, CapabilityScope, PluginOrder, PluginScope, TenantId, TnKey, UserId,
};

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
