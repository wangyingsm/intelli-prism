use ip_cache::CacheError;
use ip_core::{CoreError, Nonce, TenantId, UserId};
use ip_storage::StorageError;

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

    /// No tenant is registered under the id the request named.
    #[error("no such tenant: {tenant}")]
    UnknownTenant {
        /// The tenant the request named.
        tenant: TenantId,
    },

    /// No user is registered under the id the request named.
    #[error("no such user: {user}")]
    UnknownUser {
        /// The user the request named.
        user: UserId,
    },

    /// The user exists but is not attached to the tenant it signed for.
    #[error("{user} is not a member of {tenant}")]
    NotAMember {
        /// The user the request named.
        user: UserId,
        /// The tenant the request named.
        tenant: TenantId,
    },

    /// The signature is not the one the stored keys produce.
    #[error("signature does not match")]
    BadSignature,

    /// The system administrator may only reach the api from the machine it runs on.
    #[error("the system administrator may not call the api from {origin}")]
    AdminOffLocalhost {
        /// Where the call came from.
        origin: std::net::IpAddr,
    },

    /// The nonce was spent by an earlier request, so this one is a replay.
    #[error("nonce {nonce} was already spent")]
    SpentNonce {
        /// The nonce the request carried.
        nonce: Nonce,
    },

    /// Storage could not answer.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// The user or the passphrase was wrong, and which one is never said.
    #[error("login refused")]
    LoginRefused,

    /// A session token is not one this server issued, or it has run out.
    #[error("session token rejected")]
    SessionRejected,

    /// A logged in user gave the wrong passphrase when asked for it again.
    #[error("passphrase does not match")]
    RecheckRefused,

    /// Too many wrong passphrases were given lately, so none is checked until they age out.
    #[error("too many wrong passphrases; asking again is locked for now")]
    RecheckLocked,

    /// A session token could not be built.
    #[error("session token could not be issued")]
    SessionIssue(#[source] serde_json::Error),

    /// The system's random source failed.
    #[error("could not draw random bytes")]
    Random(#[source] getrandom::Error),

    /// The cache could not answer.
    #[error(transparent)]
    Cache(#[from] CacheError),
}
