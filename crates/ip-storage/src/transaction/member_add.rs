//! Creating a user and attaching it to a tenant that is already there.

use ip_core::TenantId;
use sqlx::Database;

use super::{IdentityDialect, sealed};
use crate::error::StorageError;
use crate::model::{Membership, NewUser, Standing, User, UserWithMembership};

/// How far a `MemberAddTxn` has got. Only the stages below are ones.
pub trait MemberAddStage: sealed::Sealed {}

/// Nothing is written yet.
pub struct MemberAddBegun;

/// The user is written, and is attached to nothing yet.
pub struct MemberAddUserSaved {
    user: User,
}

/// The user is attached to its tenant, and only committing is left.
pub struct MemberAddAttached {
    user: User,
    membership: Membership,
}

macro_rules! stages {
    ($($stage:ty),* $(,)?) => {
        $(
            impl sealed::Sealed for $stage {}
            impl MemberAddStage for $stage {}
        )*
    };
}

stages!(MemberAddBegun, MemberAddUserSaved, MemberAddAttached);

/// Adding a member: one user, attached to one tenant, as one transaction.
///
/// Each step is implemented only on the stage before it, so a user cannot be left behind
/// attached to nothing: the call that would commit before attaching does not compile.
/// Dropping the transaction before committing rolls back everything it wrote.
///
/// ```
/// # use ip_core::TenantId;
/// # use ip_storage::transaction::member_add::{MemberAddBegun, MemberAddTxn};
/// # use ip_storage::{IdentityDialect, NewUser, Standing};
/// # async fn run<DB: IdentityDialect>(
/// #     txn: MemberAddTxn<DB, MemberAddBegun>,
/// #     member: NewUser,
/// #     tenant: TenantId,
/// # ) {
/// let txn = txn.create_user(member).await.unwrap();
/// let txn = txn.attach(tenant, Standing::Member).await.unwrap();
/// let added = txn.commit().await.unwrap();
/// # }
/// ```
///
/// Committing a user who is attached to nothing does not compile:
///
/// ```compile_fail
/// # use ip_storage::transaction::member_add::{MemberAddBegun, MemberAddTxn};
/// # use ip_storage::{IdentityDialect, NewUser};
/// # async fn run<DB: IdentityDialect>(txn: MemberAddTxn<DB, MemberAddBegun>, member: NewUser) {
/// let txn = txn.create_user(member).await.unwrap();
/// let added = txn.commit().await.unwrap();
/// # }
/// ```
pub struct MemberAddTxn<DB: Database, S: MemberAddStage> {
    inner: sqlx::Transaction<'static, DB>,
    stage: S,
}

impl<DB: IdentityDialect> MemberAddTxn<DB, MemberAddBegun> {
    /// Takes over a transaction the backend opened.
    pub fn new(inner: sqlx::Transaction<'static, DB>) -> Self {
        Self {
            inner,
            stage: MemberAddBegun,
        }
    }

    /// Writes the user, or reports a conflict when the id is taken.
    pub async fn create_user(
        mut self,
        new: NewUser,
    ) -> Result<MemberAddTxn<DB, MemberAddUserSaved>, StorageError> {
        let user = DB::insert_user(&mut self.inner, new).await?;
        Ok(MemberAddTxn {
            inner: self.inner,
            stage: MemberAddUserSaved { user },
        })
    }
}

impl<DB: IdentityDialect> MemberAddTxn<DB, MemberAddUserSaved> {
    /// The user written so far.
    pub fn user(&self) -> &User {
        &self.stage.user
    }

    /// Attaches the user this transaction wrote to `tenant`, or reports that tenant missing.
    pub async fn attach(
        mut self,
        tenant: TenantId,
        standing: Standing,
    ) -> Result<MemberAddTxn<DB, MemberAddAttached>, StorageError> {
        let membership = Membership {
            user: self.stage.user.id.clone(),
            tenant,
            standing,
        };
        DB::insert_membership(&mut self.inner, membership.clone()).await?;
        Ok(MemberAddTxn {
            inner: self.inner,
            stage: MemberAddAttached {
                user: self.stage.user,
                membership,
            },
        })
    }
}

impl<DB: IdentityDialect> MemberAddTxn<DB, MemberAddAttached> {
    /// Lands the user and its attachment together.
    pub async fn commit(self) -> Result<UserWithMembership, StorageError> {
        self.inner.commit().await.map_err(StorageError::backend)?;
        Ok(UserWithMembership {
            user: self.stage.user,
            membership: self.stage.membership,
        })
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
