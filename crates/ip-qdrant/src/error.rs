//! Every way a call to a collection can fail.

/// Every way a call to a collection can fail.
///
/// Five, because a caller does something different about each: a collection that is not there is
/// an operator naming one that does not exist and is answered at registration; a refusal is the
/// store declining a call it understood, in its own words; an unauthenticated client is a wrong
/// or missing key; a store that cannot be reached is worth retrying; and a malformed answer is a
/// store that is not the one this speaks to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QdrantError {
    /// The collection could not be reached, or did not answer in time.
    ///
    /// Also the answer to a status this client cannot read as a result, which carries the status
    /// in `detail` rather than earning a variant of its own.
    #[error("the collection could not be reached: {detail}")]
    Unreachable {
        /// What went wrong on the way.
        detail: String,
    },

    /// There is no collection of that name.
    #[error("there is no collection named {collection}")]
    Absent {
        /// The name that was asked for.
        collection: String,
    },

    /// The store declined the call, and said why.
    #[error("the collection refused the call: {message} ({status})")]
    Refused {
        /// The status it answered with.
        status: u16,
        /// What it said about it.
        message: String,
    },

    /// The key was missing, or the store would not take it.
    #[error("the collection would not accept the credential")]
    Unauthorized,

    /// The answer was not the shape the call promised.
    #[error("the collection answered with something that is not an answer: {detail}")]
    Malformed {
        /// What was wrong with it.
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_says_which_it_is_without_the_caller_reading_a_string() {
        assert_eq!(
            QdrantError::Absent {
                collection: "handbook".to_owned()
            }
            .to_string(),
            "there is no collection named handbook"
        );
        assert_eq!(
            QdrantError::Refused {
                status: 400,
                message: "Wrong input: Vector dimension error".to_owned(),
            }
            .to_string(),
            "the collection refused the call: Wrong input: Vector dimension error (400)"
        );
    }
}
