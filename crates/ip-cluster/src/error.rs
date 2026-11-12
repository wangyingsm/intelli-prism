//! Every way a dispatch to another member can fail.

/// Every way a dispatch to another member can fail.
///
/// A node that *ran* and was refused partway is not here: that is a [`crate::NodeEvent`], because
/// it is an answer rather than a failure to get one. What is here is every way the coordinator
/// never learns what the node came to.
///
/// They are told apart by what a coordinator does next. [`ClusterError::Unreachable`] and
/// [`ClusterError::Unauthorized`] happen before the member has been asked to do anything, so the
/// node can still be run here. The other three happen with a stream already open, which means
/// turns may already have been taken and paid for, so running the node again would pay twice.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClusterError {
    /// The member could not be reached, did not answer, or went quiet mid-stream.
    ///
    /// Also the answer to a status this client cannot read as a stream, which carries the status
    /// in `detail` rather than earning a variant of its own.
    #[error("the cluster member could not be reached: {detail}")]
    Unreachable {
        /// What went wrong on the way.
        detail: String,
    },

    /// The member would not take the cluster secret.
    ///
    /// One deployment's members share one secret, so this is a misconfiguration rather than
    /// something to retry: a member holding a different one will go on refusing.
    #[error("the cluster member would not accept the cluster secret")]
    Unauthorized,

    /// The stream ended without the member saying what the node came to.
    ///
    /// Carries what it had said first, because that is what decides whether the node may be run
    /// again: nothing said means nothing spent.
    #[error("the cluster member stopped after {after} events without answering")]
    CutOff {
        /// How many events arrived before it stopped.
        after: usize,
    },

    /// One event was longer than this client will hold.
    #[error("the cluster member sent an event longer than {max} bytes")]
    Overlong {
        /// The ceiling it passed.
        max: usize,
    },

    /// Something arrived on the stream that is not an event this speaks.
    #[error("the cluster member sent something that is not an event: {detail}")]
    Malformed {
        /// What was wrong with it.
        detail: String,
    },
}
