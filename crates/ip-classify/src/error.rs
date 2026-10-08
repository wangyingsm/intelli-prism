//! Every way the arithmetic can refuse.

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClassifyError {
    /// Probabilities are read as a distribution over a vector that holds another count.
    #[error(
        "a distribution is {} probabilities, not {got}",
        ip_core::Difficulty::COUNT
    )]
    WrongWidth {
        /// How many probabilities were handed over.
        got: usize,
    },

    /// A probability outside 0.0..=1.0 cannot be one.
    #[error("a probability is between 0.0 and 1.0, not {value}")]
    OutOfRange {
        /// The value that was refused.
        value: f64,
    },

    /// Numbers in the right range that add to something other than a whole: five
    /// probabilities are a distribution or the source mis-shaped them, and a set that
    /// sums elsewhere is the second.
    #[error("a distribution's probabilities sum to one, not {sum}")]
    DoesNotSum {
        /// What the probabilities summed to.
        sum: f64,
    },

    /// A number that is not finite — no comparison and no arithmetic is safe on it, an
    /// infinity least of all, and the range checks that read as bounds are really checks
    /// that the value is a number at all.
    #[error("a probability, a temperature and a threshold must each be a finite number")]
    NotANumber,

    /// The temperature divides the logits before the softmax. Zero divides by nothing and a
    /// negative one flips the very ordering the gate reads; both mean nothing rather than
    /// something wrong.
    #[error("a temperature is positive, not {value}")]
    TemperatureNotPositive {
        /// The value that was refused.
        value: f64,
    },

    /// The threshold compares probabilities. A value outside 0.0..=1.0 would pass or refuse
    /// every distribution, which a configuration mistype reaches faster than a thought.
    #[error("a confidence threshold is between 0.0 and 1.0, not {value}")]
    ThresholdOutOfRange {
        /// The value that was refused.
        value: f64,
    },
}
