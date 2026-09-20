//! Authentication: passphrase verifiers, request signatures and web sessions.

pub mod error;
pub mod passphrase;
pub mod request;
pub mod session;

pub use error::AuthError;
pub use passphrase::{HashingCost, Passphrase, PassphraseHasher};
pub use request::{Authority, Identity, RequestVerifier, SignedRequest};
pub use session::{Session, SessionId, SessionToken, SessionTokens};
