//! Creating a tenant together with the owner account that represents it.

use typestate_txn::transaction;

use super::IdentityDialect;
use crate::error::StorageError;
use crate::model::{Membership, NewTenant, NewUser, Standing, Tenant, TenantWithOwner, User};

transaction! {
    name: UserCreate,
    generics: <DB: IdentityDialect>,
    carrier: sqlx::Transaction<'static, DB>,
    error: StorageError,
    record: TenantWithOwner,
    finish: { carrier.commit().await.map_err(StorageError::backend)? },
    abort: { let _ = carrier.rollback().await; },
    steps: {
        create_tenant(new: NewTenant) -> tenant: Tenant as TenantSaved {
            DB::insert_tenant(carrier, new).await?
        }
        create_user(new: NewUser) -> owner: User as UserSaved {
            DB::insert_user(carrier, new).await?
        }
        attach(standing: Standing) -> membership: Membership as Attached {
            let membership = Membership {
                user: owner.id.clone(),
                tenant: tenant.id.clone(),
                standing,
            };
            DB::insert_membership(carrier, membership.clone()).await?;
            membership
        }
    }
}

/// A store that can open a transaction creating a tenant with its owner.
pub trait UserCreateTransactional: Send + Sync {
    /// The backend whose dialect the transaction writes in.
    type Db: IdentityDialect;

    /// Opens the transaction. Dropping it before committing rolls it back.
    fn begin_user_create(
        &self,
    ) -> impl Future<Output = Result<UserCreateTxn<Self::Db, UserCreateBegun>, StorageError>> + Send;
}
