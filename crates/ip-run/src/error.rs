/// How a run refuses to start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    /// A model named by a template or a request that no upstream could be asked for.
    ///
    /// A template checks the shape of a name and not the name itself, so one that passed when
    /// it was written can still be refused here, where it meets the stricter rule the gateway
    /// spells an upstream's models under.
    #[error("{name:?} is not a model this gateway can ask for: {detail}")]
    NotAModel {
        /// What was named.
        name: String,
        /// Why it will not do.
        detail: String,
    },
}
