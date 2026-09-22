//! Creating a user and attaching it to a tenant that is already there.

use ip_core::TenantId;
use typestate_txn::transaction;

use super::IdentityDialect;
use crate::error::StorageError;
use crate::model::{Membership, NewUser, Standing, User, UserWithMembership};

transaction! {
    name: MemberAdd,
    generics: <DB: IdentityDialect>,
    carrier: sqlx::Transaction<'static, DB>,
    error: StorageError,
    record: UserWithMembership,
    finish: { carrier.commit().await.map_err(StorageError::backend)? },
    abort: { let _ = carrier.rollback().await; },
    steps: {
        create_user(new: NewUser) -> user: User as UserSaved {
            DB::insert_user(carrier, new).await?
        }
        attach(tenant: TenantId, standing: Standing) -> membership: Membership as Attached {
            let membership = Membership {
                user: user.id.clone(),
                tenant,
                standing,
            };
            DB::insert_membership(carrier, membership.clone()).await?;
            membership
        }
    }
}

/// A store that can open a transaction adding a member to a tenant.
pub trait MemberAddTransactional: Send + Sync {
    /// The backend whose dialect the transaction writes in.
    type Db: IdentityDialect;

    /// Opens the transaction. Dropping it before committing rolls it back.
    fn begin_member_add(
        &self,
    ) -> impl Future<Output = Result<MemberAddTxn<Self::Db, MemberAddBegun>, StorageError>> + Send;
}
