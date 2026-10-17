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

    /// The ONNX Runtime library could not be found or loaded. An ordinary startup
    /// failure of a build with the `runtime` feature on a machine that has no runtime
    /// installed — reported, never aborted: a classifier that cannot load leaves the
    /// process standing. The message names what was tried.
    #[error("the ONNX Runtime library could not be loaded: {0}")]
    RuntimeMissing(String),

    /// A required file of the artefact directory is not there — a misspelled path or
    /// a half-copied export. The message names the file and spells the layout.
    #[error("the classifier artefact is missing {0}")]
    ArtifactMissing(String),

    /// A file of the artefact exists and is not readable as what it claims: tokenizer
    /// json that isn't, a gate manifest without its temperature, a temperature the
    /// arithmetic refuses. The artifact on disk is wrong; the operator needs to know
    /// which file to go look at.
    #[error("the classifier artefact file {0} is not readable as what it claims")]
    ArtifactUnreadable(String),

    /// The manifest declares a schema this build does not read. Refused rather than
    /// read for the fields it happens to recognise: a manifest written to a later
    /// shape may mean something different by the same key, and guessing at it is how
    /// a gate ends up calibrated for an artefact nobody shipped.
    #[error(
        "the classifier artefact declares manifest schema {found}, and this build reads {carried}"
    )]
    ArtifactSchema {
        /// What the manifest said.
        found: u64,
        /// What this build knows, listed.
        carried: String,
    },

    /// The weights are not the weights the manifest was written for. A truncated
    /// download, a half-replaced file, an artefact swapped under a running deployment:
    /// each would route by a gate fitted to something else, which is silent, so this
    /// is loud instead.
    #[error(
        "the classifier weights do not match the manifest: it names {named}, the file is {found}"
    )]
    ArtifactChecksum {
        /// The digest the manifest claims.
        named: String,
        /// The digest the file actually has.
        found: String,
    },

    /// The graph would not open: corrupted, foreign, or built for another runtime.
    #[error("the classifier graph would not open: {0}")]
    GraphRefused(String),

    /// The graph opened and answered, but not with the five numbers the gate is built
    /// on: a logits vector shorter than the classes, which only a mis-shaped export
    /// produces — the failure a numeric export verification promises against, and
    /// double-checks with what it reads back.
    #[error(
        "the classifier answered with {got} logits, not {}",
        ip_core::Difficulty::COUNT
    )]
    WrongOutput {
        /// How many logits came back.
        got: usize,
    },

    /// The forward pass failed at run time, or its output would not extract. Ordinary
    /// operation includes this possibility, which is why it is a value: serving never
    /// fails because classifying did.
    #[error("classification failed: {0}")]
    Inference(String),

    /// The text would not tokenize — input the model's tokenizer rejects, which real
    /// user text sometimes is and which classifies as nothing rather than as an error
    /// above the request.
    #[error("the text could not be tokenized: {0}")]
    Tokenize(String),

    /// The threshold compares probabilities. A value outside 0.0..=1.0 would pass or refuse
    /// every distribution, which a configuration mistype reaches faster than a thought.
    #[error("a confidence threshold is between 0.0 and 1.0, not {value}")]
    ThresholdOutOfRange {
        /// The value that was refused.
        value: f64,
    },
}
