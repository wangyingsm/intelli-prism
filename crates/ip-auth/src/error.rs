use ip_core::CoreError;

/// Every way authentication can fail before an identity is established.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The passphrase is shorter than the policy allows.
    #[error("passphrase is {len} bytes, under the {min} byte minimum")]
    PassphraseTooShort {
        /// How long the passphrase was.
        len: usize,
        /// The shortest passphrase accepted.
        min: usize,
    },

    /// The passphrase is longer than the hasher will read.
    #[error("passphrase is {len} bytes, over the {max} byte limit")]
    PassphraseTooLong {
        /// How long the passphrase was.
        len: usize,
        /// The longest passphrase accepted.
        max: usize,
    },

    /// The requested hashing cost is not one argon2 accepts.
    #[error("hashing parameters are not accepted: {0}")]
    Params(argon2::password_hash::Error),

    /// Hashing or reading a verifier failed.
    #[error("passphrase hashing failed: {0}")]
    Hashing(argon2::password_hash::Error),

    /// A value failed its own validation.
    #[error(transparent)]
    Value(#[from] CoreError),
}
