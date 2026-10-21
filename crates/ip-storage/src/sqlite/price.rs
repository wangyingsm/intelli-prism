use async_trait::async_trait;
use ip_core::{ModelName, Price, Rate, Timestamp};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use super::SqliteStore;
use crate::error::{Entity, StorageError};
use crate::price::{NewPrice, PriceStore, Priced};

#[async_trait]
impl PriceStore for SqliteStore {
    async fn put_price(&self, price: NewPrice) -> Result<Priced, StorageError> {
        let mut connection = self.connection().await?;
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

        Ok(Priced {
            model: price.model,
            price: price.price,
            created_at,
        })
    }

    async fn remove_price(&self, model: &ModelName) -> Result<(), StorageError> {
        let mut connection = self.connection().await?;
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
        Ok(())
    }

    async fn prices(&self) -> Result<Vec<Priced>, StorageError> {
        read_prices(&mut *self.connection().await?).await
    }
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
    use crate::suite::price::priced_at;

    /// Sets a price, then writes over one column with something the code cannot produce.
    async fn written_over(store: &SqliteStore, update: &'static str) {
        store
            .put_price(priced_at(3_000_000, 15_000_000))
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
