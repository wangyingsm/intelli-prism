mod identity;

use std::str::FromStr;

use ip_core::{TenantId, UserId};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};

use crate::error::{Entity, StorageError};
use crate::model::{TenantRowId, UserRowId};

const MAX_CONNECTIONS: u32 = 16;

/// The cluster backend: a shared postgres server.
#[derive(Debug, Clone)]
pub struct PostgresStore {
    pool: PgPool,
    #[cfg(test)]
    scratch: Option<std::sync::Arc<scratch::Schema>>,
}

impl PostgresStore {
    /// Connects to the database at `url`, migrating it when it is behind.
    pub async fn open(url: &str) -> Result<Self, StorageError> {
        let options = PgConnectOptions::from_str(url).map_err(StorageError::backend)?;
        Self::connect(options).await
    }

    async fn connect(options: PgConnectOptions) -> Result<Self, StorageError> {
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect_with(options)
            .await
            .map_err(StorageError::backend)?;
        sqlx::migrate!("./migrations/postgres")
            .run(&pool)
            .await
            .map_err(StorageError::backend)?;
        Ok(Self {
            pool,
            #[cfg(test)]
            scratch: None,
        })
    }

    /// Closes every pooled connection.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    async fn tenant_row_id(&self, id: &TenantId) -> Result<TenantRowId, StorageError> {
        sqlx::query("SELECT row_id FROM tenants WHERE id = $1")
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
        sqlx::query("SELECT row_id FROM users WHERE id = $1")
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

/// Fresh schemas for tests, on the database `DATABASE_URL` names.
#[cfg(test)]
pub(crate) mod scratch {
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use sqlx::postgres::{PgConnectOptions, PgConnection};
    use sqlx::{AssertSqlSafe, Connection};

    use super::PostgresStore;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A schema made for one test, dropped with the last clone of its store.
    #[derive(Debug)]
    pub(crate) struct Schema {
        name: String,
        server: PgConnectOptions,
    }

    impl Schema {
        /// The schema's name.
        pub(crate) fn name(&self) -> &str {
            &self.name
        }
    }

    impl Drop for Schema {
        fn drop(&mut self) {
            let sql = format!("DROP SCHEMA {} CASCADE", self.name);
            let server = self.server.clone();
            // A drop inside the test's runtime cannot wait on that runtime, so it waits on its own.
            let dropped = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime to drop a scratch schema")
                    .block_on(async move {
                        let mut connection = PgConnection::connect_with(&server).await?;
                        sqlx::query(AssertSqlSafe(sql))
                            .execute(&mut connection)
                            .await?;
                        connection.close().await
                    })
            })
            .join();
            if !matches!(dropped, Ok(Ok(()))) {
                eprintln!("could not drop scratch schema {}", self.name);
            }
        }
    }

    /// A store on a new schema of the database `DATABASE_URL` names, or none when it is unset.
    pub(crate) async fn store() -> Option<PostgresStore> {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL is unset, so this postgres test checks nothing");
            return None;
        };
        let server =
            PgConnectOptions::from_str(&url).expect("DATABASE_URL names a postgres database");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let name = format!(
            "ip_test_{}_{}_{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let mut connection = PgConnection::connect_with(&server)
            .await
            .expect("DATABASE_URL is reachable");
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {name}")))
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        let options = server.clone().options([("search_path", name.as_str())]);
        let mut store = PostgresStore::connect(options).await.unwrap();
        store.scratch = Some(Arc::new(Schema { name, server }));
        Some(store)
    }
}

/// The shared identity tests, each on a scratch schema of the database `DATABASE_URL` names.
#[cfg(test)]
mod suite {
    crate::suite::backend_suite!(identity, crate::postgres::scratch::store);
}

#[cfg(test)]
mod tests {
    use sqlx::Connection;
    use sqlx::postgres::PgConnection;

    use super::*;

    #[tokio::test]
    async fn a_fresh_schema_is_migrated_and_empty() {
        let Some(store) = scratch::store().await else {
            return;
        };
        let tenants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenants")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(tenants, 0);
    }

    #[tokio::test]
    async fn a_scratch_schema_goes_with_its_store() {
        let Some(store) = scratch::store().await else {
            return;
        };
        let name = store.scratch.as_ref().unwrap().name().to_owned();
        store.close().await;
        drop(store);
        let url = std::env::var("DATABASE_URL").unwrap();
        let mut connection = PgConnection::connect(&url).await.unwrap();
        let left: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = $1",
        )
        .bind(&name)
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(left, 0);
    }
}
