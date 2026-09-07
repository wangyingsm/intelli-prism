//! Authentication: passphrase verifiers and request signatures.

pub mod error;
pub mod passphrase;
pub mod request;

pub use error::AuthError;
pub use passphrase::{HashingCost, Passphrase, PassphraseHasher};
pub use request::{Authority, Identity, RequestVerifier, SignedRequest};
