use std::collections::HashSet;

use crate::error::CoreError;
use crate::id::{ApiId, TenantId, UserId};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
/// A capability a subject may hold.
pub enum Capability {
    TenantMgr,
    UserMgr,
    ApiAccess,
    ApiAdvMgr,
    LimitMgr,
    SysAgent,
    Observer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
/// The tuple shape a capability is scoped by.
pub enum ScopeKind {
    User,
    Tenant,
    Api,
}

impl Capability {
    /// The scope shape this capability must be granted at.
    pub fn scope_kind(self) -> ScopeKind {
        match self {
            Self::TenantMgr => ScopeKind::User,
            Self::UserMgr | Self::SysAgent | Self::Observer => ScopeKind::Tenant,
            Self::ApiAccess | Self::ApiAdvMgr | Self::LimitMgr => ScopeKind::Api,
        }
    }

    /// A capability that only exists while its prerequisite is held at the same scope.
    pub fn prerequisite(self) -> Option<Self> {
        match self {
            Self::ApiAdvMgr | Self::LimitMgr => Some(Self::ApiAccess),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
/// The subject and range one grant applies to.
pub enum CapabilityScope {
    User {
        user: UserId,
    },
    Tenant {
        user: UserId,
        tenant: TenantId,
    },
    Api {
        user: UserId,
        tenant: TenantId,
        api: ApiId,
    },
}

impl CapabilityScope {
    /// The shape of this scope.
    pub fn kind(&self) -> ScopeKind {
        match self {
            Self::User { .. } => ScopeKind::User,
            Self::Tenant { .. } => ScopeKind::Tenant,
            Self::Api { .. } => ScopeKind::Api,
        }
    }

    /// The user this scope belongs to.
    pub fn user(&self) -> &UserId {
        match self {
            Self::User { user } | Self::Tenant { user, .. } | Self::Api { user, .. } => user,
        }
    }

    /// The tenant this scope sits inside, if any.
    pub fn tenant(&self) -> Option<&TenantId> {
        match self {
            Self::User { .. } => None,
            Self::Tenant { tenant, .. } | Self::Api { tenant, .. } => Some(tenant),
        }
    }

    /// The api this scope names, if any.
    pub fn api(&self) -> Option<&ApiId> {
        match self {
            Self::Api { api, .. } => Some(api),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
/// One capability held at one scope.
pub struct Grant {
    capability: Capability,
    scope: CapabilityScope,
}

impl Grant {
    /// Pairs a capability with a scope of the shape it requires.
    pub fn new(capability: Capability, scope: CapabilityScope) -> Result<Self, CoreError> {
        let expected = capability.scope_kind();
        let actual = scope.kind();
        if expected != actual {
            return Err(CoreError::ScopeMismatch {
                capability,
                expected,
                actual,
            });
        }
        Ok(Self { capability, scope })
    }

    /// The capability granted.
    pub fn capability(&self) -> Capability {
        self.capability
    }

    /// The scope it is granted at.
    pub fn scope(&self) -> &CapabilityScope {
        &self.scope
    }
}

/// Everything explicitly granted to one subject. Nothing is held unless it is in here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants(HashSet<Grant>);

impl Grants {
    /// An empty set: nothing is held.
    pub fn new() -> Self {
        Self(HashSet::new())
    }

    /// Adds a grant, reporting whether it was new.
    pub fn insert(&mut self, grant: Grant) -> bool {
        self.0.insert(grant)
    }

    /// Revokes a grant, reporting whether it was present.
    pub fn remove(&mut self, grant: &Grant) -> bool {
        self.0.remove(grant)
    }

    /// How many grants are held.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every grant held.
    pub fn iter(&self) -> impl Iterator<Item = &Grant> {
        self.0.iter()
    }

    /// Whether the capability is held at this scope, prerequisite included.
    pub fn holds(&self, capability: Capability, scope: &CapabilityScope) -> bool {
        if capability.scope_kind() != scope.kind() {
            return false;
        }
        if let Some(prerequisite) = capability.prerequisite()
            && !self.granted(prerequisite, scope)
        {
            return false;
        }
        self.granted(capability, scope)
    }

    fn granted(&self, capability: Capability, scope: &CapabilityScope) -> bool {
        self.0
            .iter()
            .any(|grant| grant.capability == capability && &grant.scope == scope)
    }
}

impl FromIterator<Grant> for Grants {
    fn from_iter<I: IntoIterator<Item = Grant>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
/// The standing a subject has before any explicit grant.
pub enum Role {
    SysAdmin,
    TenantAdmin(TenantId),
    Member,
}

impl Role {
    /// Capabilities a role carries on its own, before any explicit grant is consulted.
    pub fn implies(&self, capability: Capability, scope: &CapabilityScope) -> bool {
        match self {
            Self::SysAdmin => capability.scope_kind() == scope.kind(),
            Self::TenantAdmin(owned) => {
                capability != Capability::TenantMgr
                    && capability.scope_kind() == scope.kind()
                    && scope.tenant() == Some(owned)
            }
            Self::Member => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn api_scope() -> CapabilityScope {
        CapabilityScope::Api {
            user: user(),
            tenant: tenant(),
            api: ApiId::new("chat.completions").unwrap(),
        }
    }

    fn tenant_scope() -> CapabilityScope {
        CapabilityScope::Tenant {
            user: user(),
            tenant: tenant(),
        }
    }

    fn user_scope() -> CapabilityScope {
        CapabilityScope::User { user: user() }
    }

    #[test]
    fn grant_rejects_a_scope_of_the_wrong_kind() {
        assert_eq!(
            Grant::new(Capability::ApiAccess, tenant_scope()),
            Err(CoreError::ScopeMismatch {
                capability: Capability::ApiAccess,
                expected: ScopeKind::Api,
                actual: ScopeKind::Tenant,
            })
        );
    }

    #[test]
    fn nothing_is_held_by_default() {
        assert!(!Grants::new().holds(Capability::ApiAccess, &api_scope()));
        assert!(!Grants::new().holds(Capability::TenantMgr, &user_scope()));
    }

    #[test]
    fn holds_what_was_granted() {
        let grants: Grants = [Grant::new(Capability::ApiAccess, api_scope()).unwrap()]
            .into_iter()
            .collect();
        assert!(grants.holds(Capability::ApiAccess, &api_scope()));
    }

    #[test]
    fn a_capability_asked_about_at_the_wrong_kind_of_scope_is_not_held() {
        let grants: Grants = [Grant::new(Capability::ApiAccess, api_scope()).unwrap()]
            .into_iter()
            .collect();
        assert!(!grants.holds(Capability::ApiAccess, &tenant_scope()));
    }

    #[test]
    fn a_grant_does_not_reach_another_tenant() {
        let grants: Grants = [Grant::new(Capability::UserMgr, tenant_scope()).unwrap()]
            .into_iter()
            .collect();
        let elsewhere = CapabilityScope::Tenant {
            user: user(),
            tenant: TenantId::new("globex").unwrap(),
        };
        assert!(!grants.holds(Capability::UserMgr, &elsewhere));
    }

    #[test]
    fn advanced_api_capabilities_die_without_api_access() {
        let grants: Grants = [Grant::new(Capability::ApiAdvMgr, api_scope()).unwrap()]
            .into_iter()
            .collect();
        assert!(!grants.holds(Capability::ApiAdvMgr, &api_scope()));
    }

    #[test]
    fn advanced_api_capabilities_live_alongside_api_access() {
        let grants: Grants = [
            Grant::new(Capability::ApiAccess, api_scope()).unwrap(),
            Grant::new(Capability::LimitMgr, api_scope()).unwrap(),
        ]
        .into_iter()
        .collect();
        assert!(grants.holds(Capability::LimitMgr, &api_scope()));
    }

    #[test]
    fn system_administrator_implies_every_capability() {
        assert!(Role::SysAdmin.implies(Capability::TenantMgr, &user_scope()));
        assert!(Role::SysAdmin.implies(Capability::ApiAdvMgr, &api_scope()));
    }

    #[test]
    fn tenant_owner_implies_capabilities_inside_its_own_tenant() {
        let role = Role::TenantAdmin(tenant());
        assert!(role.implies(Capability::UserMgr, &tenant_scope()));
        assert!(role.implies(Capability::ApiAccess, &api_scope()));
    }

    #[test]
    fn tenant_owner_implies_nothing_in_another_tenant() {
        let role = Role::TenantAdmin(TenantId::new("globex").unwrap());
        assert!(!role.implies(Capability::UserMgr, &tenant_scope()));
    }

    #[test]
    fn tenant_owner_can_not_manage_tenants() {
        assert!(!Role::TenantAdmin(tenant()).implies(Capability::TenantMgr, &user_scope()));
    }

    #[test]
    fn a_member_implies_nothing() {
        assert!(!Role::Member.implies(Capability::Observer, &tenant_scope()));
    }
}
