use async_trait::async_trait;
use ip_core::{ModelName, Price, Rate, Timestamp};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use crate::audit::{By, NewAudit};
use crate::error::{Entity, StorageError};
use crate::price::{NewPrice, PriceStore, Priced};

#[async_trait]
impl PriceStore for SqliteStore {
    async fn put_price(&self, price: NewPrice, by: &By) -> Result<Priced, StorageError> {
        // One transaction, so a price that lands without its record is not a state this
        // store can be left in, and neither is a record whose price rolled back.
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        // Rolled back by hand rather than by dropping it: a transaction left to its
        // `Drop` keeps the connection's locks until the pool gets round to it, and a
        // scratch schema being torn down behind it waits for ever.
        match setting(&mut transaction, price, by).await {
            Ok(set) => transaction
                .commit()
                .await
                .map_err(StorageError::backend)
                .map(|()| set),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn remove_price(&self, model: &ModelName, by: &By) -> Result<(), StorageError> {
        let mut transaction = self.pool.begin().await.map_err(StorageError::backend)?;
        match removing(&mut transaction, model, by).await {
            Ok(()) => transaction.commit().await.map_err(StorageError::backend),
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn prices(&self) -> Result<Vec<Priced>, StorageError> {
        read_prices(&mut *self.connection().await?).await
    }
}

/// Sets a price and records the change, both on the one connection its caller commits.
///
/// Apart from the method so that every way out of it is a `?` the caller can roll back,
/// rather than a return that leaves a transaction open.
async fn setting(
    connection: &mut SqliteConnection,
    price: NewPrice,
    by: &By,
) -> Result<Priced, StorageError> {
    let before = read_price(connection, &price.model).await?;
    let created_at = Timestamp::now();
    sqlx::query(
        "INSERT INTO prices (model, input, output, created_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT (model) DO UPDATE SET input = excluded.input, \
         output = excluded.output, created_at = excluded.created_at",
    )
    .bind(price.model.as_str())
    .bind(rate(price.price.input))
    .bind(rate(price.price.output))
    .bind(created_at.unix_seconds())
    .execute(&mut *connection)
    .await
    .map_err(StorageError::backend)?;

    let set = Priced {
        model: price.model,
        price: price.price,
        created_at,
    };
    super::audit::record(
        connection,
        NewAudit::changed(by.clone(), before.as_ref(), &set),
    )
    .await?;
    Ok(set)
}

/// Removes a price and records the removal, on the one connection its caller commits.
async fn removing(
    connection: &mut SqliteConnection,
    model: &ModelName,
    by: &By,
) -> Result<(), StorageError> {
    let before = read_price(connection, model).await?;
    let removed = sqlx::query("DELETE FROM prices WHERE model = ?")
        .bind(model.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StorageError::backend)?
        .rows_affected();
    if removed == 0 {
        return Err(StorageError::NotFound {
            entity: Entity::Price,
            id: model.as_str().to_owned(),
        });
    }
    // A row was deleted, so something was there to read a moment ago in the same
    // transaction; an absent before here would mean the two disagree.
    let Some(before) = before else {
        return Err(StorageError::Malformed {
            entity: Entity::Price,
            detail: "a price was removed that nothing could read".to_owned(),
        });
    };
    super::audit::record(connection, NewAudit::removed(by.clone(), &before)).await
}

/// What one model costs, and nothing where nothing prices it. Read inside the transaction
/// that is about to change it, so the before it records is the before that was replaced.
async fn read_price(
    connection: &mut SqliteConnection,
    model: &ModelName,
) -> Result<Option<Priced>, StorageError> {
    let row = sqlx::query("SELECT model, input, output, created_at FROM prices WHERE model = ?")
        .bind(model.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(StorageError::backend)?;
    row.map(priced).transpose()
}

/// A rate as the column holds it, saturating at what the column can hold.
///
/// Unreachable with a `Rate` the type built, whose ceiling is far below this; here so the
/// conversion has no panic in it.
fn rate(rate: Rate) -> i64 {
    i64::try_from(rate.get()).unwrap_or(i64::MAX)
}

/// Every price held, over whichever connection it is given.
pub(super) async fn read_prices(
    connection: &mut SqliteConnection,
) -> Result<Vec<Priced>, StorageError> {
    let rows = sqlx::query(
        "SELECT model, input, output, created_at FROM prices \
         ORDER BY created_at DESC, row_id DESC",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(StorageError::backend)?;
    rows.into_iter().map(priced).collect()
}

/// Rebuilds a price, refusing a row whose stored value its own type will not take.
pub(super) fn priced(row: SqliteRow) -> Result<Priced, StorageError> {
    Ok(Priced {
        model: ModelName::new(row.get::<String, _>("model").as_str())?,
        price: Price::new(quoted(row.get("input"))?, quoted(row.get("output"))?),
        created_at: Timestamp::from_unix_seconds(row.get("created_at"))?,
    })
}

/// A stored rate, refused when it is not one this gateway will price.
pub(super) fn quoted(stored: i64) -> Result<Rate, StorageError> {
    let micros = u64::try_from(stored).map_err(|_| StorageError::Malformed {
        entity: Entity::Price,
        detail: format!("{stored} is not a rate"),
    })?;
    Ok(Rate::new(micros)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::fixture::*;
    use crate::suite::price::{by, priced_at};

    /// Sets a price, then writes over one column with something the code cannot produce.
    async fn written_over(store: &SqliteStore, update: &'static str) {
        store
            .put_price(priced_at(3_000_000, 15_000_000), &by())
            .await
            .unwrap();
        sqlx::query(update).execute(&store.pool).await.unwrap();
    }

    /// Every column whose stored value its own type can refuse, and a value that it
    /// refuses. A rate below nothing is not among them: the column's own CHECK will
    /// not hold one, which is why the conversion that guards it carries no test.
    const REFUSED: [&str; 3] = [
        "UPDATE prices SET input = 9000000000000",
        "UPDATE prices SET output = 9000000000000",
        "UPDATE prices SET model = ''",
    ];

    #[tokio::test]
    async fn a_row_no_type_of_ours_will_take_is_refused_on_the_way_back() {
        for update in REFUSED {
            let store = store().await;
            written_over(&store, update).await;
            assert!(
                matches!(store.prices().await, Err(StorageError::Value(_))),
                "{update} was read back as {:?}",
                store.prices().await
            );
        }
    }
}
