//! What a request cost, and the rates that produce it.
//!
//! Money is held as a whole number of micros — millionths of whatever currency the
//! deployment prices in — and never as a float. A price of a fraction of a cent per
//! token is ordinary here, and a sum of a million of them has to come out the same
//! on two machines; binary floating point gives neither.
//!
//! The deployment's currency is its own and is not carried on the figure. The gateway
//! converts nothing, so a number here means *micros of whatever you priced in*, and
//! mixing two currencies in one ledger would produce a total that means nothing.

use std::fmt;
use std::iter::Sum;
use std::ops::Add;

use crate::error::CoreError;
use crate::usage::{TokenCount, Tokens};

/// A rate is quoted per this many tokens, as every vendor quotes one.
const PER: u128 = 1_000_000;

/// The dearest rate, charged on the most tokens one request can carry, fits in micros.
///
/// A compile time proof rather than a comment: raise [`Rate::DEAREST`] past what
/// [`Money`] can hold and the build stops here, instead of a ledger quietly reporting
/// a fortune as whatever it wrapped to.
const _: () = assert!(
    Rate::DEAREST as u128 * u32::MAX as u128 / PER <= u64::MAX as u128,
    "the dearest rate on the most tokens must fit in the micros Money holds"
);

/// What something cost, in micros of the deployment's currency.
///
/// Never negative: a request spends or it does not.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Money(u64);

impl Money {
    /// Nothing spent, which is what an answer out of the cache costs.
    pub const ZERO: Self = Self(0);

    /// Wraps a whole number of micros.
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// The figure, in micros.
    #[must_use]
    pub const fn micros(self) -> u64 {
        self.0
    }

    /// This much, over again, as many times as a request was raced.
    ///
    /// Saturating, like every other arithmetic here: a ledger that wrapped would
    /// report a fortune as a farthing.
    #[must_use]
    pub const fn times(self, count: u32) -> Self {
        Self(self.0.saturating_mul(count as u64))
    }
}

impl Add for Money {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl Sum for Money {
    fn sum<I: Iterator<Item = Self>>(spent: I) -> Self {
        spent.fold(Self::ZERO, Add::add)
    }
}

impl fmt::Display for Money {
    /// Whole units and six places, which is what a micro is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:06}", self.0 / 1_000_000, self.0 % 1_000_000)
    }
}

/// What a million tokens cost, in micros.
///
/// Quoted per million because that is how every vendor quotes it, and held as micros
/// per million so that the ordinary prices are whole numbers: three dollars a million
/// is 3,000,000, and a quarter of a dollar a million is 250,000.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(try_from = "u64")]
pub struct Rate(u64);

impl Rate {
    /// Free, which is what a model somebody hosts themselves may well be.
    pub const FREE: Self = Self(0);

    /// The most a million tokens may be quoted at: a million units of the currency.
    ///
    /// Far past any price a vendor has charged, and here so that a figure typed with
    /// the decimal point in the wrong place is refused rather than silently billed.
    ///
    /// It is also what keeps the arithmetic exact: the ceiling below proves that the
    /// dearest rate times the most tokens a request can carry still lands inside the
    /// micros [`Money`] holds, so no cost is ever a saturated approximation.
    pub const DEAREST: u64 = 1_000_000_000_000;

    /// Wraps a quote after checking it is one.
    ///
    /// # Errors
    ///
    /// [`CoreError::RateTooDear`] when the quote is past [`Rate::DEAREST`].
    pub fn new(micros_per_million: u64) -> Result<Self, CoreError> {
        if micros_per_million > Self::DEAREST {
            return Err(CoreError::RateTooDear {
                asked: micros_per_million,
                most: Self::DEAREST,
            });
        }
        Ok(Self(micros_per_million))
    }

    /// The quote, in micros per million tokens.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// What this many tokens cost at this rate.
    ///
    /// Rounded to the nearest micro rather than truncated: a gateway answering
    /// millions of requests would otherwise lose half a micro on each of them, always
    /// in its own favour, which is a bill that drifts away from the vendor's.
    #[must_use]
    pub fn of(self, tokens: TokenCount) -> Money {
        let spent = u128::from(self.0) * u128::from(tokens.get());
        let rounded = (spent + PER / 2) / PER;
        Money(u64::try_from(rounded).unwrap_or(u64::MAX))
    }
}

impl TryFrom<u64> for Rate {
    type Error = CoreError;

    /// Every way a rate arrives from outside goes through the ceiling, deserialization
    /// included: a transparent newtype that derived its way past [`Rate::new`] would
    /// take a price the api refuses from a payload nobody checked.
    fn try_from(micros_per_million: u64) -> Result<Self, Self::Error> {
        Self::new(micros_per_million)
    }
}

/// What one model costs in each direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Price {
    /// What a million tokens sent to the model cost.
    pub input: Rate,
    /// What a million tokens it answers with cost.
    pub output: Rate,
}

impl Price {
    /// A price in both directions.
    #[must_use]
    pub const fn new(input: Rate, output: Rate) -> Self {
        Self { input, output }
    }

    /// What one answer cost: what was sent, and what came back.
    #[must_use]
    pub fn of(self, tokens: Tokens) -> Money {
        self.input.of(tokens.input) + self.output.of(tokens.output)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} in, {} out, per million",
            Money(self.input.get()),
            Money(self.output.get())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic the whole milestone rests on, at a rate an operator would type.
    #[test]
    fn a_hundred_tokens_at_a_quarter_a_million_cost_twenty_five_micros() {
        let rate = Rate::new(250_000).unwrap();

        assert_eq!(rate.of(TokenCount::new(100)), Money::from_micros(25));
        assert_eq!(
            rate.of(TokenCount::new(1_000_000)),
            Money::from_micros(250_000),
            "a million tokens cost exactly what the rate quotes"
        );
    }

    /// Half a micro goes up, not away: truncating would lose it on every request, and
    /// always in the gateway's favour.
    #[test]
    fn what_falls_between_two_micros_rounds_to_the_nearer() {
        // 3 tokens at 500_000 per million is 1.5 micros.
        assert_eq!(
            Rate::new(500_000).unwrap().of(TokenCount::new(3)),
            Money::from_micros(2)
        );
        // 1 token at 500_000 per million is 0.5 micros.
        assert_eq!(
            Rate::new(500_000).unwrap().of(TokenCount::new(1)),
            Money::from_micros(1)
        );
        // 1 token at 400_000 per million is 0.4 micros, which is nearer nothing.
        assert_eq!(
            Rate::new(400_000).unwrap().of(TokenCount::new(1)),
            Money::ZERO
        );
    }

    #[test]
    fn nothing_spent_costs_nothing_and_a_free_model_costs_nothing_either() {
        assert_eq!(
            Rate::new(15_000_000).unwrap().of(TokenCount::ZERO),
            Money::ZERO
        );
        assert_eq!(Rate::FREE.of(TokenCount::new(4_000_000_000)), Money::ZERO);
    }

    /// The worst case is exact, and the arithmetic that gets there is not.
    ///
    /// The dearest rate on the most tokens a request can carry multiplies out to
    /// 4.3 × 10²¹, which no `u64` holds — the product is taken in 128 bits and only
    /// the result, 4.3 × 10¹⁵ micros, comes back down. That is what the ceiling on
    /// `DEAREST` is for, and the compile time assertion beside it proves it stays
    /// true. A sum of such figures is a different matter, and saturates.
    #[test]
    fn the_dearest_rate_on_the_most_tokens_is_still_exact() {
        let dearest = Rate::new(Rate::DEAREST).unwrap();

        let spent = dearest.of(TokenCount::new(u32::MAX));

        assert_eq!(spent, Money::from_micros(4_294_967_295_000_000));
        assert_eq!(
            Money::from_micros(u64::MAX) + Money::from_micros(1),
            Money::from_micros(u64::MAX),
            "a sum past the end stops there rather than starting again"
        );
        assert_eq!(
            Money::from_micros(u64::MAX).times(2),
            Money::from_micros(u64::MAX)
        );
    }

    /// The ceiling holds against a payload as well as against a caller: a rate is read
    /// from json wherever the api and the store meet, and a transparent newtype whose
    /// `Deserialize` derived its way past the check would let a refused price in.
    #[test]
    fn a_rate_read_from_json_goes_through_the_ceiling_too() {
        let read: Rate = serde_json::from_str("3000000").unwrap();
        assert_eq!(read.get(), 3_000_000);

        let refused = serde_json::from_str::<Rate>("9000000000000").unwrap_err();
        assert!(
            refused.to_string().contains("past the"),
            "the refusal is the type's own: {refused}"
        );
    }

    #[test]
    fn a_quote_with_the_point_in_the_wrong_place_is_refused() {
        let refusal = Rate::new(Rate::DEAREST + 1).unwrap_err();

        assert!(matches!(refusal, CoreError::RateTooDear { asked, most }
            if asked == Rate::DEAREST + 1 && most == Rate::DEAREST));
        assert_eq!(Rate::new(Rate::DEAREST).unwrap().get(), Rate::DEAREST);
        assert_eq!(Rate::new(0).unwrap(), Rate::FREE);
    }

    /// What a request costs is what was sent plus what came back, each at its own rate.
    #[test]
    fn a_price_charges_each_direction_at_its_own_rate() {
        let price = Price::new(
            Rate::new(3_000_000).unwrap(),
            Rate::new(15_000_000).unwrap(),
        );
        let tokens = Tokens::new(TokenCount::new(1_000), TokenCount::new(500));

        // 1000 at 3 a million is 3000 micros; 500 at 15 a million is 7500.
        assert_eq!(price.of(tokens), Money::from_micros(10_500));
        assert_eq!(price.of(Tokens::ZERO), Money::ZERO);
    }

    #[test]
    fn what_was_spent_adds_up() {
        let spent: Money = [
            Money::from_micros(25),
            Money::from_micros(10_500),
            Money::ZERO,
        ]
        .into_iter()
        .sum();

        assert_eq!(spent, Money::from_micros(10_525));
        assert_eq!(Money::from_micros(25).times(3), Money::from_micros(75));
        assert_eq!(Money::from_micros(25).times(0), Money::ZERO);
    }

    /// The figure as a human reads it: whole units and the six places a micro needs.
    #[test]
    fn money_prints_as_units_and_micros() {
        assert_eq!(Money::from_micros(10_500).to_string(), "0.010500");
        assert_eq!(Money::from_micros(3_000_000).to_string(), "3.000000");
        assert_eq!(Money::ZERO.to_string(), "0.000000");
        assert_eq!(
            Price::new(
                Rate::new(3_000_000).unwrap(),
                Rate::new(15_000_000).unwrap()
            )
            .to_string(),
            "3.000000 in, 15.000000 out, per million"
        );
    }
}
