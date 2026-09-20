use async_trait::async_trait;
use sqlx::Database;

use crate::error::StorageError;
use crate::model::{Membership, NewTenant, NewUser, Standing, Tenant, TenantWithOwner, User};

/// The writes one backend contributes to a transaction, in its own dialect.
///
/// Sqlite binds `?` and postgres binds `$1`, so the statements cannot be shared. What is
/// shared is the order they run in, which `UserCreateTxn` fixes at compile time.
#[async_trait]
pub trait UserCreateDialect: Database {
    /// Writes a tenant row.
    async fn insert_tenant(
        connection: &mut Self::Connection,
        new: NewTenant,
    ) -> Result<Tenant, StorageError>;

    /// Writes a user row.
    async fn insert_user(
        connection: &mut Self::Connection,
        new: NewUser,
    ) -> Result<User, StorageError>;

    /// Attaches a user to a tenant.
    async fn insert_membership(
        connection: &mut Self::Connection,
        membership: Membership,
    ) -> Result<(), StorageError>;
}

mod sealed {
    pub trait Sealed {}
}

/// How far a `UserCreateTxn` has got. Only the stages below are ones.
pub trait UserCreateStage: sealed::Sealed {}

/// Nothing is written yet.
pub struct UserCreateBegun;

/// The tenant is written.
pub struct UserCreateTenantSaved {
    tenant: Tenant,
}

/// The tenant and its owner account are written.
pub struct UserCreateUserSaved {
    tenant: Tenant,
    user: User,
}

/// The owner is attached to the tenant, and only committing is left.
pub struct UserCreateAttached {
    tenant: Tenant,
    user: User,
    membership: Membership,
}

macro_rules! stages {
    ($($stage:ty),* $(,)?) => {
        $(
            impl sealed::Sealed for $stage {}
            impl UserCreateStage for $stage {}
        )*
    };
}

stages!(
    UserCreateBegun,
    UserCreateTenantSaved,
    UserCreateUserSaved,
    UserCreateAttached
);

/// Creating a tenant with its owner, as one transaction whose steps run in one order.
///
/// Each step is implemented only on the stage before it, so a step cannot be skipped or
/// reordered: the call that would do so does not compile. Dropping the transaction before
/// committing rolls back everything it wrote.
///
/// ```
/// # use ip_storage::transaction::{UserCreateBegun, UserCreateTxn};
/// # use ip_storage::{UserCreateDialect, Membership, NewTenant, NewUser, Standing};
/// # async fn run<DB: UserCreateDialect>(txn: UserCreateTxn<DB, UserCreateBegun>, tenant: NewTenant, owner: NewUser) {
/// let txn = txn.create_tenant(tenant).await.unwrap();
/// let txn = txn.create_user(owner).await.unwrap();
/// let txn = txn.attach(Standing::Owner).await.unwrap();
/// let created = txn.commit().await.unwrap();
/// # }
/// ```
///
/// Attaching before there is anything to attach does not compile:
///
/// ```compile_fail
/// # use ip_storage::transaction::{UserCreateBegun, UserCreateTxn};
/// # use ip_storage::{UserCreateDialect, Standing};
/// # async fn run<DB: UserCreateDialect>(txn: UserCreateTxn<DB, UserCreateBegun>) {
/// // attach belongs to UserCreateTxn<DB, UserCreateUserSaved>, so no owner can be attached to nothing.
/// let txn = txn.attach(Standing::Owner).await.unwrap();
/// # }
/// ```
///
/// Committing before the owner is attached does not compile either:
///
/// ```compile_fail
/// # use ip_storage::transaction::{UserCreateBegun, UserCreateTxn};
/// # use ip_storage::{UserCreateDialect, NewTenant};
/// # async fn run<DB: UserCreateDialect>(txn: UserCreateTxn<DB, UserCreateBegun>, tenant: NewTenant) {
/// let txn = txn.create_tenant(tenant).await.unwrap();
/// let created = txn.commit().await.unwrap();
/// # }
/// ```
pub struct UserCreateTxn<DB: Database, S: UserCreateStage> {
    inner: sqlx::Transaction<'static, DB>,
    stage: S,
}

impl<DB: UserCreateDialect> UserCreateTxn<DB, UserCreateBegun> {
    /// Takes over a transaction the backend opened.
    pub fn new(inner: sqlx::Transaction<'static, DB>) -> Self {
        Self {
            inner,
            stage: UserCreateBegun,
        }
    }

    /// Writes the tenant, or reports a conflict when the id is taken.
    pub async fn create_tenant(
        mut self,
        new: NewTenant,
    ) -> Result<UserCreateTxn<DB, UserCreateTenantSaved>, StorageError> {
        let tenant = DB::insert_tenant(&mut self.inner, new).await?;
        Ok(UserCreateTxn {
            inner: self.inner,
            stage: UserCreateTenantSaved { tenant },
        })
    }
}

impl<DB: UserCreateDialect> UserCreateTxn<DB, UserCreateTenantSaved> {
    /// The tenant written so far.
    pub fn tenant(&self) -> &Tenant {
        &self.stage.tenant
    }

    /// Writes the owner account, or reports a conflict when the id is taken.
    pub async fn create_user(
        mut self,
        new: NewUser,
    ) -> Result<UserCreateTxn<DB, UserCreateUserSaved>, StorageError> {
        let user = DB::insert_user(&mut self.inner, new).await?;
        Ok(UserCreateTxn {
            inner: self.inner,
            stage: UserCreateUserSaved {
                tenant: self.stage.tenant,
                user,
            },
        })
    }
}

impl<DB: UserCreateDialect> UserCreateTxn<DB, UserCreateUserSaved> {
    /// The tenant written so far.
    pub fn tenant(&self) -> &Tenant {
        &self.stage.tenant
    }

    /// The user written so far.
    pub fn user(&self) -> &User {
        &self.stage.user
    }

    /// Attaches the user to the tenant this transaction wrote, and no other.
    pub async fn attach(
        mut self,
        standing: Standing,
    ) -> Result<UserCreateTxn<DB, UserCreateAttached>, StorageError> {
        let membership = Membership {
            user: self.stage.user.id.clone(),
            tenant: self.stage.tenant.id.clone(),
            standing,
        };
        DB::insert_membership(&mut self.inner, membership.clone()).await?;
        Ok(UserCreateTxn {
            inner: self.inner,
            stage: UserCreateAttached {
                tenant: self.stage.tenant,
                user: self.stage.user,
                membership,
            },
        })
    }
}

impl<DB: UserCreateDialect> UserCreateTxn<DB, UserCreateAttached> {
    /// Lands the tenant, its owner and their attachment together.
    pub async fn commit(self) -> Result<TenantWithOwner, StorageError> {
        self.inner.commit().await.map_err(StorageError::backend)?;
        Ok(TenantWithOwner {
            tenant: self.stage.tenant,
            owner: self.stage.user,
            membership: self.stage.membership,
        })
    }
}

/// A store that can open a transaction creating a tenant with its owner.
pub trait UserCreateTransactional: Send + Sync {
    /// The backend whose dialect the transaction writes in.
    type Db: UserCreateDialect;

    /// Opens the transaction. Dropping it before committing rolls it back.
    fn begin_user_create(
        &self,
    ) -> impl Future<Output = Result<UserCreateTxn<Self::Db, UserCreateBegun>, StorageError>> + Send;
}
