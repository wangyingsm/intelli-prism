//! Authentication: passphrase verifiers and request signatures.

pub mod error;
pub mod passphrase;

pub use error::AuthError;
pub use passphrase::{HashingCost, Passphrase, PassphraseHasher};
