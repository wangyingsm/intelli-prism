use ip_core::{ModelName, Price, Rate};

use crate::audit::By;
use crate::error::{Entity, StorageError};
use crate::price::NewPrice;
use crate::store::Backend;

/// The account every suite test prices as, through the endpoint that sets a price.
pub(crate) fn by() -> By {
    By::new(crate::suite::fixture::user_id(), "/_ip/prices")
}

/// A price for the model every other test prices, in micros per million tokens.
pub(crate) fn priced_at(input: u64, output: u64) -> NewPrice {
    NewPrice {
        model: model(),
        price: Price::new(Rate::new(input).unwrap(), Rate::new(output).unwrap()),
    }
}

/// The model the suite prices.
pub(crate) fn model() -> ModelName {
    ModelName::new("claude-opus-5").unwrap()
}

pub(crate) async fn a_price_round_trips(store: &impl Backend) {
    let set = store
        .put_price(priced_at(3_000_000, 15_000_000), &by())
        .await
        .unwrap();
    assert_eq!(set.model, model());
    assert_eq!(set.price.input.get(), 3_000_000);
    assert_eq!(set.price.output.get(), 15_000_000);

    let held = store.prices().await.unwrap();
    assert_eq!(held, vec![set]);
}

pub(crate) async fn pricing_the_same_model_again_replaces_what_it_costs(store: &impl Backend) {
    store
        .put_price(priced_at(3_000_000, 15_000_000), &by())
        .await
        .unwrap();
    store
        .put_price(priced_at(1_000_000, 5_000_000), &by())
        .await
        .unwrap();

    let held = store.prices().await.unwrap();

    assert_eq!(held.len(), 1, "one model, one price");
    assert_eq!(held[0].price.input.get(), 1_000_000);
    assert_eq!(held[0].price.output.get(), 5_000_000);
}

pub(crate) async fn a_model_costing_nothing_is_priced_at_nothing(store: &impl Backend) {
    let set = store.put_price(priced_at(0, 0), &by()).await.unwrap();

    assert_eq!(set.price, Price::default());
    assert_eq!(store.prices().await.unwrap(), vec![set]);
}

pub(crate) async fn every_model_holds_its_own_price(store: &impl Backend) {
    store
        .put_price(priced_at(3_000_000, 15_000_000), &by())
        .await
        .unwrap();
    store
        .put_price(
            NewPrice {
                model: ModelName::new("claude-haiku-5-5").unwrap(),
                price: Price::new(Rate::new(250_000).unwrap(), Rate::new(1_250_000).unwrap()),
            },
            &by(),
        )
        .await
        .unwrap();

    let held = store.prices().await.unwrap();

    assert_eq!(held.len(), 2);
    let cheap = held
        .iter()
        .find(|priced| priced.model.as_str() == "claude-haiku-5-5")
        .expect("both are held");
    assert_eq!(cheap.price.input.get(), 250_000);
}

pub(crate) async fn removing_a_price_leaves_the_others(store: &impl Backend) {
    store
        .put_price(priced_at(3_000_000, 15_000_000), &by())
        .await
        .unwrap();
    let other = ModelName::new("claude-haiku-5-5").unwrap();
    store
        .put_price(
            NewPrice {
                model: other.clone(),
                price: Price::new(Rate::new(250_000).unwrap(), Rate::new(1_250_000).unwrap()),
            },
            &by(),
        )
        .await
        .unwrap();

    store.remove_price(&model(), &by()).await.unwrap();

    let held = store.prices().await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].model, other);
}

pub(crate) async fn removing_a_price_that_is_absent_reports_it_missing(store: &impl Backend) {
    let missing = store.remove_price(&model(), &by()).await.unwrap_err();

    assert!(
        matches!(&missing, StorageError::NotFound { entity, id }
            if *entity == Entity::Price && id == "claude-opus-5"),
        "{missing}"
    );
}

/// Every node holds the prices in memory, so a change here has to tell them to rebuild
/// exactly as a change to a limit or a route does.
pub(crate) async fn every_change_to_a_price_moves_the_revision_on(store: &impl Backend) {
    let first = store.rule_revision().await.unwrap();

    store
        .put_price(priced_at(3_000_000, 15_000_000), &by())
        .await
        .unwrap();
    let after_set = store.rule_revision().await.unwrap();
    assert!(after_set > first, "setting a price moves it on");

    store
        .put_price(priced_at(1_000_000, 5_000_000), &by())
        .await
        .unwrap();
    let after_change = store.rule_revision().await.unwrap();
    assert!(after_change > after_set, "changing one moves it on");

    store.remove_price(&model(), &by()).await.unwrap();
    let after_removal = store.rule_revision().await.unwrap();
    assert!(after_removal > after_change, "removing one moves it on");
}

/// Prices belong to the deployment rather than to a tenant: there is no tenant on the
/// row and nothing scopes one. Held as a test so that a later tenant column cannot be
/// added without somebody reading this and deciding it on purpose.
pub(crate) async fn a_price_belongs_to_no_tenant(store: &impl Backend) {
    let set = store
        .put_price(priced_at(3_000_000, 15_000_000), &by())
        .await
        .unwrap();

    let held = store.prices().await.unwrap();

    assert_eq!(held, vec![set], "every node sees the one set of prices");
}
