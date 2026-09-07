use std::net::IpAddr;
use std::sync::Arc;

use ip_core::{
    Capability, CapabilityScope, Grants, Nonce, Role, Signature, TenantId, UserId, UtKey,
};
use ip_storage::{AccountKind, Standing, Storage};

use crate::error::AuthError;

/// What the four `X-Ip-*` headers carry, once each has been read as its own type.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedRequest {
    /// From `X-Ip-Tnid`.
    pub tenant: TenantId,
    /// From `X-Ip-Userid`.
    pub user: UserId,
    /// From `X-Ip-Nonce`.
    pub nonce: Nonce,
    /// From `X-Ip-Signature`.
    pub signature: Signature,
}

/// Who a verified request is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The user that signed.
    pub user: UserId,
    /// The tenant it signed for.
    pub tenant: TenantId,
    /// What it is, before any explicit grant.
    pub role: Role,
}

/// An identity together with everything it was granted.
#[derive(Debug, Clone)]
pub struct Authority {
    identity: Identity,
    grants: Grants,
}

impl Authority {
    /// Pairs an identity with the grants read for it.
    pub fn new(identity: Identity, grants: Grants) -> Self {
        Self { identity, grants }
    }

    /// Who this is.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Whether this request may do the thing, by role or by explicit grant.
    pub fn allows(&self, capability: Capability, scope: &CapabilityScope) -> bool {
        if scope.user() != &self.identity.user {
            return false;
        }
        if scope
            .tenant()
            .is_some_and(|tenant| tenant != &self.identity.tenant)
        {
            return false;
        }
        self.identity.role.implies(capability, scope) || self.grants.holds(capability, scope)
    }
}

/// Checks the signature on an api request against the stored tenant and user.
#[derive(Clone)]
pub struct RequestVerifier {
    store: Arc<dyn Storage>,
}

impl RequestVerifier {
    /// Reads identities out of this store.
    pub fn new(store: Arc<dyn Storage>) -> Self {
        Self { store }
    }

    /// Establishes who a request is from, or refuses it.
    pub async fn verify(
        &self,
        request: &SignedRequest,
        origin: IpAddr,
    ) -> Result<Identity, AuthError> {
        let tenant =
            self.store
                .tenant(&request.tenant)
                .await?
                .ok_or_else(|| AuthError::UnknownTenant {
                    tenant: request.tenant.clone(),
                })?;
        let user = self
            .store
            .user(&request.user)
            .await?
            .ok_or_else(|| AuthError::UnknownUser {
                user: request.user.clone(),
            })?;
        let membership = self
            .store
            .membership(&request.user, &request.tenant)
            .await?
            .ok_or_else(|| AuthError::NotAMember {
                user: request.user.clone(),
                tenant: request.tenant.clone(),
            })?;

        if user.kind == AccountKind::SystemAdministrator && !origin.is_loopback() {
            return Err(AuthError::AdminOffLocalhost { origin });
        }

        let ut_key = UtKey::derive(&user.id, &tenant.key);
        let by_user = Signature::of_user(&ut_key, &request.nonce).verify(&request.signature);
        let by_owner = membership.standing == Standing::Owner
            && Signature::of_tenant_owner(&tenant.key, &request.nonce).verify(&request.signature);
        if !(by_user | by_owner) {
            return Err(AuthError::BadSignature);
        }

        Ok(Identity {
            user: user.id,
            tenant: tenant.id,
            role: role_of(user.kind, membership.standing, request.tenant.clone()),
        })
    }

    /// Reads what an established identity was granted.
    pub async fn authority(&self, identity: Identity) -> Result<Authority, AuthError> {
        let grants = self.store.grants_of(&identity.user).await?;
        Ok(Authority::new(identity, grants))
    }
}

fn role_of(kind: AccountKind, standing: Standing, tenant: TenantId) -> Role {
    match (kind, standing) {
        (AccountKind::SystemAdministrator, _) => Role::SysAdmin,
        (AccountKind::Regular, Standing::Owner) => Role::TenantAdmin(tenant),
        (AccountKind::Regular, Standing::Member) => Role::Member,
    }
}

#[cfg(test)]
mod tests {
    use ip_core::{ApiId, Grant, PassphraseHash, TnKey};
    use ip_storage::{
        Membership, MembershipStore, NewTenant, NewUser, SqliteStore, TenantStore, UserStore,
    };

    use super::*;

    const LOCAL: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    const REMOTE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7));

    fn tenant_id() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn user_id() -> UserId {
        UserId::new("alice").unwrap()
    }

    fn nonce() -> Nonce {
        Nonce::new("0123456789abcdef").unwrap()
    }

    fn hash() -> PassphraseHash {
        PassphraseHash::new("$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aGFzaA").unwrap()
    }

    /// Builds a store holding one tenant and one user attached with `standing`.
    async fn fixture(kind: AccountKind, standing: Standing) -> (Arc<dyn Storage>, TnKey) {
        let store = SqliteStore::in_memory().await.unwrap();
        let key = TnKey::generate().unwrap();
        store
            .create_tenant(NewTenant {
                id: tenant_id(),
                key: key.clone(),
            })
            .await
            .unwrap();
        store
            .create_user(NewUser {
                id: user_id(),
                passphrase: hash(),
                kind,
            })
            .await
            .unwrap();
        store
            .attach(Membership {
                user: user_id(),
                tenant: tenant_id(),
                standing,
            })
            .await
            .unwrap();
        (Arc::new(store), key)
    }

    fn signed_by_user(key: &TnKey) -> SignedRequest {
        let ut_key = UtKey::derive(&user_id(), key);
        SignedRequest {
            tenant: tenant_id(),
            user: user_id(),
            nonce: nonce(),
            signature: Signature::of_user(&ut_key, &nonce()),
        }
    }

    fn signed_by_tenant_key(key: &TnKey) -> SignedRequest {
        SignedRequest {
            tenant: tenant_id(),
            user: user_id(),
            nonce: nonce(),
            signature: Signature::of_tenant_owner(key, &nonce()),
        }
    }

    #[tokio::test]
    async fn a_correctly_signed_request_names_its_user() {
        let (store, key) = fixture(AccountKind::Regular, Standing::Member).await;
        let identity = RequestVerifier::new(store)
            .verify(&signed_by_user(&key), REMOTE)
            .await
            .unwrap();
        assert_eq!(
            identity,
            Identity {
                user: user_id(),
                tenant: tenant_id(),
                role: Role::Member,
            }
        );
    }

    #[tokio::test]
    async fn a_signature_over_another_nonce_is_refused() {
        let (store, key) = fixture(AccountKind::Regular, Standing::Member).await;
        let mut request = signed_by_user(&key);
        request.nonce = Nonce::new("fedcba9876543210").unwrap();
        assert!(matches!(
            RequestVerifier::new(store).verify(&request, REMOTE).await,
            Err(AuthError::BadSignature)
        ));
    }

    #[tokio::test]
    async fn a_signature_made_with_another_tenant_key_is_refused() {
        let (store, _) = fixture(AccountKind::Regular, Standing::Member).await;
        let stolen = TnKey::generate().unwrap();
        assert!(matches!(
            RequestVerifier::new(store)
                .verify(&signed_by_user(&stolen), REMOTE)
                .await,
            Err(AuthError::BadSignature)
        ));
    }

    #[tokio::test]
    async fn a_tenant_owner_may_sign_with_the_tenant_key() {
        let (store, key) = fixture(AccountKind::Regular, Standing::Owner).await;
        let identity = RequestVerifier::new(store)
            .verify(&signed_by_tenant_key(&key), REMOTE)
            .await
            .unwrap();
        assert_eq!(identity.role, Role::TenantAdmin(tenant_id()));
    }

    #[tokio::test]
    async fn an_ordinary_member_may_not_sign_with_the_tenant_key() {
        let (store, key) = fixture(AccountKind::Regular, Standing::Member).await;
        assert!(matches!(
            RequestVerifier::new(store)
                .verify(&signed_by_tenant_key(&key), REMOTE)
                .await,
            Err(AuthError::BadSignature)
        ));
    }

    #[tokio::test]
    async fn a_user_not_attached_to_the_tenant_is_refused() {
        let store = SqliteStore::in_memory().await.unwrap();
        let key = TnKey::generate().unwrap();
        store
            .create_tenant(NewTenant {
                id: tenant_id(),
                key: key.clone(),
            })
            .await
            .unwrap();
        store
            .create_user(NewUser {
                id: user_id(),
                passphrase: hash(),
                kind: AccountKind::Regular,
            })
            .await
            .unwrap();
        assert!(matches!(
            RequestVerifier::new(Arc::new(store))
                .verify(&signed_by_user(&key), REMOTE)
                .await,
            Err(AuthError::NotAMember { .. })
        ));
    }

    #[tokio::test]
    async fn an_unknown_tenant_or_user_is_refused() {
        let store = Arc::new(SqliteStore::in_memory().await.unwrap());
        let verifier = RequestVerifier::new(store.clone());
        let key = TnKey::generate().unwrap();
        assert!(matches!(
            verifier.verify(&signed_by_user(&key), REMOTE).await,
            Err(AuthError::UnknownTenant { .. })
        ));
        store
            .create_tenant(NewTenant {
                id: tenant_id(),
                key: key.clone(),
            })
            .await
            .unwrap();
        assert!(matches!(
            verifier.verify(&signed_by_user(&key), REMOTE).await,
            Err(AuthError::UnknownUser { .. })
        ));
    }

    #[tokio::test]
    async fn the_system_administrator_is_confined_to_localhost() {
        let (store, key) = fixture(AccountKind::SystemAdministrator, Standing::Member).await;
        let verifier = RequestVerifier::new(store);
        assert!(matches!(
            verifier.verify(&signed_by_user(&key), REMOTE).await,
            Err(AuthError::AdminOffLocalhost { .. })
        ));
        assert_eq!(
            verifier
                .verify(&signed_by_user(&key), LOCAL)
                .await
                .unwrap()
                .role,
            Role::SysAdmin
        );
    }

    #[tokio::test]
    async fn an_authority_allows_what_a_grant_holds() {
        let (store, key) = fixture(AccountKind::Regular, Standing::Member).await;
        let scope = CapabilityScope::Api {
            user: user_id(),
            tenant: tenant_id(),
            api: ApiId::new("chat").unwrap(),
        };
        store
            .grant(&Grant::new(Capability::ApiAccess, scope.clone()).unwrap())
            .await
            .unwrap();
        let verifier = RequestVerifier::new(store);
        let identity = verifier
            .verify(&signed_by_user(&key), REMOTE)
            .await
            .unwrap();
        let authority = verifier.authority(identity).await.unwrap();
        assert!(authority.allows(Capability::ApiAccess, &scope));
        assert!(!authority.allows(Capability::ApiAdvMgr, &scope));
    }

    #[tokio::test]
    async fn an_authority_refuses_a_scope_belonging_to_someone_else() {
        let (store, key) = fixture(AccountKind::SystemAdministrator, Standing::Member).await;
        let verifier = RequestVerifier::new(store);
        let identity = verifier.verify(&signed_by_user(&key), LOCAL).await.unwrap();
        let authority = verifier.authority(identity).await.unwrap();
        let elsewhere = CapabilityScope::Tenant {
            user: UserId::new("mallory").unwrap(),
            tenant: tenant_id(),
        };
        assert!(!authority.allows(Capability::UserMgr, &elsewhere));
    }

    #[tokio::test]
    async fn an_authority_refuses_a_scope_in_another_tenant() {
        let (store, key) = fixture(AccountKind::SystemAdministrator, Standing::Member).await;
        let verifier = RequestVerifier::new(store);
        let identity = verifier.verify(&signed_by_user(&key), LOCAL).await.unwrap();
        let authority = verifier.authority(identity).await.unwrap();
        let elsewhere = CapabilityScope::Tenant {
            user: user_id(),
            tenant: TenantId::new("globex").unwrap(),
        };
        assert!(!authority.allows(Capability::UserMgr, &elsewhere));
    }

    #[tokio::test]
    async fn a_tenant_owner_needs_no_grant_inside_its_own_tenant() {
        let (store, key) = fixture(AccountKind::Regular, Standing::Owner).await;
        let verifier = RequestVerifier::new(store);
        let identity = verifier
            .verify(&signed_by_user(&key), REMOTE)
            .await
            .unwrap();
        let authority = verifier.authority(identity).await.unwrap();
        let scope = CapabilityScope::Tenant {
            user: user_id(),
            tenant: tenant_id(),
        };
        assert!(authority.allows(Capability::UserMgr, &scope));
        assert!(!authority.allows(
            Capability::TenantMgr,
            &CapabilityScope::User { user: user_id() }
        ));
    }
}
