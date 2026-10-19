//! Every way a vector, or a threshold over one, can refuse to be built.

/// What was wrong with the numbers.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EmbedError {
    /// A width of nothing is not a model's output, it is an empty read.
    #[error("a vector of no numbers has no direction")]
    NoWidth,

    /// The width comes from a manifest an operator points at, so a mistyped one
    /// allocates before anything checks whether a model of that shape exists.
    #[error("{got} numbers is wider than any embedding this carries, which is {most}")]
    TooWide {
        /// What was asked for.
        got: usize,
        /// The widest this build will hold.
        most: usize,
    },

    /// An infinity or a NaN among the numbers. No comparison can decide it, and one of
    /// them turns every distance measured against the vector into nothing.
    #[error("a vector holding {value} cannot be compared with anything")]
    NotANumber {
        /// The number that was refused, so the message names what arrived.
        value: f32,
    },

    /// Every number so near zero that normalising would divide the rounding rather than
    /// the direction. A model that answers this way has answered nothing.
    #[error("a vector of length {length:e} has no direction left in it, only rounding")]
    NoDirection {
        /// The length that was measured.
        length: f64,
    },

    /// Two vectors of different widths are two different models' answers, and the
    /// cosine of them is not a small error but a meaningless number.
    #[error("a vector of {got} numbers cannot be compared with one of {expected}")]
    WidthMismatch {
        /// The width the first vector has.
        expected: usize,
        /// The width the second one has.
        got: usize,
    },

    /// A threshold outside the unit admits everything or nothing, and both are mistypes
    /// rather than decisions.
    #[error("{value} is not a nearness: a threshold lies within 0.0..=1.0")]
    NotANearness {
        /// The threshold that was refused.
        value: f32,
    },

    /// A cache bounded at nothing is one that never hits, written as though it were a
    /// setting. Switching the level off is said by leaving it unconfigured.
    #[error("a neighbourhood that holds nothing can never answer")]
    HoldsNothing,

    /// A lookup is linear in what is held, so a bound past what can be scanned is a
    /// cache that costs more than the call it saves.
    #[error("holding {got} vectors is more than one neighbourhood scans, which is {most}")]
    HoldsTooMuch {
        /// What was asked for.
        got: usize,
        /// The most that will be held.
        most: usize,
    },

    /// The ONNX Runtime library could not be found or loaded. An ordinary startup
    /// failure of a build with the `runtime` feature on a machine that has no runtime
    /// installed — reported, never aborted. The message names what was tried.
    #[error("the ONNX Runtime library could not be loaded: {0}")]
    RuntimeMissing(String),

    /// A required file of the artefact directory is not there — a misspelled path or
    /// a half-copied export. The message names the file and spells the layout.
    #[error("the embedding artefact is missing {0}")]
    ArtifactMissing(String),

    /// A file of the artefact exists and is not readable as what it claims: tokenizer
    /// json that isn't, a manifest without its width. The operator needs to know
    /// which file to go and look at.
    #[error("the embedding artefact file {0} is not readable as what it claims")]
    ArtifactUnreadable(String),

    /// The manifest declares a schema this build does not read. Refused rather than
    /// read for the fields it happens to recognise: a later shape may mean something
    /// different by the same key.
    #[error(
        "the embedding artefact declares manifest schema {found}, and this build reads {carried}"
    )]
    ArtifactSchema {
        /// What the manifest said.
        found: u64,
        /// What this build knows, listed.
        carried: String,
    },

    /// A way of turning token vectors into one that this build does not carry. Which
    /// one is right is a property of how the model was trained, so guessing would give
    /// a confident vector that means nothing much, and silently.
    #[error("the embedding artefact pools its tokens by {named}, which this build does not carry")]
    UnknownPooling {
        /// What the manifest named.
        named: String,
    },

    /// The weights are not the weights the manifest was written for: a truncated
    /// download, a half-replaced file, an artefact swapped under a running deployment.
    #[error(
        "the embedding weights do not match the manifest: it names {named}, the file is {found}"
    )]
    ArtifactChecksum {
        /// The digest the manifest claims.
        named: String,
        /// The digest the file actually has.
        found: String,
    },

    /// The graph would not open: corrupted, foreign, or built for another runtime.
    #[error("the embedding graph would not open: {0}")]
    GraphRefused(String),

    /// The graph answered in a shape this cannot read as one vector of the manifest's
    /// width. Refused rather than reinterpreted: token vectors laid over a pooled one
    /// read as a perfectly plausible vector of the wrong thing.
    #[error("the embedding graph answered {got}, which is not one vector of {expected}")]
    WrongOutput {
        /// The width the manifest promised.
        expected: usize,
        /// The shape that came back.
        got: String,
    },

    /// The forward pass failed at run time, or its output would not extract. Ordinary
    /// operation includes this possibility, which is why it is a value: serving never
    /// fails because embedding did.
    #[error("embedding failed: {0}")]
    Inference(String),

    /// The text would not tokenize, which real user text sometimes does not.
    #[error("the text could not be tokenized: {0}")]
    Tokenize(String),
}
