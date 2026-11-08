//! Every way a call to a plane can fail.

/// Every way a call to a plane can fail.
///
/// The four are what a caller does something different about: a plane that cannot be reached is a
/// 503 and worth retrying, a refusal is the store declining a call it understood, an
/// unauthenticated client is a deployment misconfiguration, and a malformed answer is a store
/// that is not the one this speaks to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DrsgError {
    /// The plane could not be reached, or did not answer in time.
    ///
    /// Also the answer to a status this client cannot read as a result, which carries the status
    /// in `detail` rather than earning a variant of its own: a caller treats "no result" the same
    /// way whether the socket closed or a proxy answered 502.
    #[error("the graph store could not be reached: {detail}")]
    Unreachable {
        /// What went wrong on the way.
        detail: String,
    },

    /// The plane answered with a JSON-RPC error object rather than a result.
    #[error("the graph store refused the call: {message} ({code})")]
    Refused {
        /// The code the store chose.
        code: i64,
        /// What it said about it.
        message: String,
    },

    /// The credential was missing, or the store would not take it.
    #[error("the graph store would not accept the credential")]
    Unauthorized,

    /// The answer was not JSON-RPC, or was not an answer to the call that asked.
    #[error("the graph store answered with something that is not an answer: {detail}")]
    Malformed {
        /// What was wrong with it.
        detail: String,
    },
}
