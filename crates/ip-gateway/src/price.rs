//! What each model costs, as a node holds it.

use std::collections::HashMap;
use std::sync::Arc;

use ip_core::{ModelName, Price};
use ip_storage::Priced;

/// The prices a node holds, asked for by model as each answer is recorded.
///
/// Holding none prices nothing, which is what a deployment that has set no price does —
/// and a request nothing prices records no cost rather than a cost of nothing.
#[derive(Debug, Default, Clone)]
pub struct Prices(Arc<HashMap<ModelName, Price>>);

impl Prices {
    /// Holds what storage returned.
    ///
    /// A model named twice keeps the last, which the unique index on the table makes
    /// unreachable from the store and which costs nothing to be sure of here.
    #[must_use]
    pub fn new(prices: Vec<Priced>) -> Self {
        Self(Arc::new(
            prices
                .into_iter()
                .map(|priced| (priced.model, priced.price))
                .collect(),
        ))
    }

    /// What this model costs, and nothing where nobody has priced it.
    #[must_use]
    pub fn of(&self, model: &ModelName) -> Option<Price> {
        self.0.get(model).copied()
    }

    /// How many models are priced.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is priced, which records no cost against anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ip_core::{Rate, Timestamp};

    fn priced(model: &str, input: u64, output: u64) -> Priced {
        Priced {
            model: ModelName::new(model).unwrap(),
            price: Price::new(Rate::new(input).unwrap(), Rate::new(output).unwrap()),
            created_at: Timestamp::now(),
        }
    }

    fn named(model: &str) -> ModelName {
        ModelName::new(model).unwrap()
    }

    #[test]
    fn a_priced_model_is_found_by_its_name() {
        let prices = Prices::new(vec![
            priced("claude-opus-5", 3_000_000, 15_000_000),
            priced("claude-haiku-5-5", 250_000, 1_250_000),
        ]);

        assert_eq!(prices.len(), 2);
        let opus = prices.of(&named("claude-opus-5")).expect("it is priced");
        assert_eq!(opus.input.get(), 3_000_000);
        assert_eq!(opus.output.get(), 15_000_000);
    }

    /// The distinction the whole column rests on: no price is not a price of nothing.
    #[test]
    fn a_model_nobody_priced_has_no_price_rather_than_a_free_one() {
        let prices = Prices::new(vec![priced("claude-opus-5", 3_000_000, 15_000_000)]);

        assert_eq!(prices.of(&named("gpt-5")), None);
        assert_eq!(
            prices
                .of(&named("claude-opus-5"))
                .map(|price| price.input.get()),
            Some(3_000_000)
        );
    }

    #[test]
    fn a_node_holding_no_price_prices_nothing() {
        let prices = Prices::default();

        assert!(prices.is_empty());
        assert_eq!(prices.of(&named("claude-opus-5")), None);
    }

    /// A free model is priced, and priced at nothing, which is not the same as unpriced.
    #[test]
    fn a_model_priced_at_nothing_is_still_priced() {
        let prices = Prices::new(vec![priced("local-llama", 0, 0)]);

        assert_eq!(prices.of(&named("local-llama")), Some(Price::default()));
    }
}
