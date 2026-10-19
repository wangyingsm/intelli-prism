//! Every way opening an exported model can fail.

use std::path::PathBuf;

/// What went wrong between a directory on disk and a model ready to run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OnnxError {
    /// The ONNX Runtime library could not be found or loaded. An ordinary startup
    /// failure of a build that carries a model on a machine that has no runtime
    /// installed — reported, never aborted. The message names what was tried.
    #[error("the ONNX Runtime library could not be loaded: {0}")]
    RuntimeMissing(String),

    /// Two callers asked for two different runtime libraries.
    ///
    /// Only one can be had: the library is loaded once for the process, and the first
    /// load wins. Before this crate owned the loading, the second path was simply
    /// ignored — a configuration naming it was accepted, had no effect, and said
    /// nothing. Refusing is the honest answer, because the alternative is a model
    /// running against a library its operator did not choose.
    #[error("the ONNX Runtime at {loaded:?} is already loaded, so {asked:?} cannot also be")]
    RuntimeDisagrees {
        /// What is loaded, and what every model in this process will run against.
        loaded: PathBuf,
        /// What was asked for afterwards.
        asked: PathBuf,
    },

    /// A required file of the artefact directory is not there — a misspelled path or
    /// a half-copied export. The message names the file and spells the layout.
    #[error("the artefact is missing {0}")]
    ArtifactMissing(String),

    /// A file of the artefact exists and is not readable as what it claims: tokenizer
    /// json that isn't, a manifest without the figure it is read for. The operator
    /// needs to know which file to go and look at.
    #[error("the artefact file {0} is not readable as what it claims")]
    ArtifactUnreadable(String),

    /// The manifest declares a schema this build does not read. Refused rather than
    /// read for the fields it happens to recognise: a manifest written to a later
    /// shape may mean something different by the same key, and guessing at it is how
    /// a model ends up calibrated for an artefact nobody shipped.
    #[error("the artefact declares manifest schema {found}, and this build reads {carried}")]
    ArtifactSchema {
        /// What the manifest said.
        found: u64,
        /// What this build knows, listed.
        carried: String,
    },

    /// The weights are not the weights the manifest was written for. A truncated
    /// download, a half-replaced file, an artefact swapped under a running
    /// deployment: each would run against figures fitted to something else, which is
    /// silent, so this is loud instead.
    #[error("the weights do not match the manifest: it names {named}, the file is {found}")]
    ArtifactChecksum {
        /// The digest the manifest claims.
        named: String,
        /// The digest the file actually has.
        found: String,
    },

    /// The graph would not open: corrupted, foreign, or built for another runtime.
    #[error("the graph would not open: {0}")]
    GraphRefused(String),

    /// The forward pass failed at run time, or its output would not extract. Ordinary
    /// operation includes this possibility, which is why it is a value: serving never
    /// fails because a model did.
    #[error("the model failed: {0}")]
    Inference(String),

    /// The text would not tokenize, which real user text sometimes does not.
    #[error("the text could not be tokenized: {0}")]
    Tokenize(String),
}
