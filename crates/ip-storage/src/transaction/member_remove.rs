//! Detaching a user from a tenant, together with everything it held inside it.

use ip_core::{TenantId, UserId};
use typestate_txn::transaction;

use super::IdentityDialect;
use crate::error::StorageError;
use crate::model::{MemberRemoved, Membership};

transaction! {
    name: MemberRemove,
    generics: <DB: IdentityDialect>,
    carrier: sqlx::Transaction<'static, DB>,
    error: StorageError,
    record: MemberRemoved,
    finish: { carrier.commit().await.map_err(StorageError::backend)? },
    steps: {
        detach(user: UserId, tenant: TenantId) -> membership: Membership as Detached {
            DB::delete_membership(carrier, &user, &tenant).await?
        }
        revoke_grants() -> revoked: u64 as GrantsRevoked {
            DB::delete_grants_in_tenant(carrier, &membership.user, &membership.tenant).await?
        }
    }
}

/// A store that can open a transaction removing a member from a tenant.
pub trait MemberRemoveTransactional: Send + Sync {
    /// The backend whose dialect the transaction writes in.
    type Db: IdentityDialect;

    /// Opens the transaction. Dropping it before committing rolls it back.
    fn begin_member_remove(
        &self,
    ) -> impl Future<Output = Result<MemberRemoveTxn<Self::Db, MemberRemoveBegun>, StorageError>> + Send;
}
