//! Every way the arithmetic can refuse.

/// Every way reading a distribution can fail.
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

    /// Opening the model: the runtime library, the artefact directory, the graph,
    /// the tokenizer, and the forward pass itself. Every one of them belongs to
    /// `ip-onnx`, which both this crate and the embedder open their models through.
    #[cfg(feature = "runtime")]
    #[error(transparent)]
    Onnx(#[from] ip_onnx::OnnxError),

    /// The graph opened and answered, but not with the five numbers the gate is built
    /// on: a logits vector shorter than the classes, which only a mis-shaped export
    /// produces — the failure a numeric export verification promises against, and
    /// double-checks with what it reads back.
    ///
    /// This one stays here rather than moving to `ip-onnx`: what shape an answer must
    /// be is the one thing about a model that only the crate asking it can know.
    #[error(
        "the classifier answered with {got} logits, not {}",
        ip_core::Difficulty::COUNT
    )]
    WrongOutput {
        /// How many logits came back.
        got: usize,
    },

    /// The threshold compares probabilities. A value outside 0.0..=1.0 would pass or refuse
    /// every distribution, which a configuration mistype reaches faster than a thought.
    #[error("a confidence threshold is between 0.0 and 1.0, not {value}")]
    ThresholdOutOfRange {
        /// The value that was refused.
        value: f64,
    },
}
