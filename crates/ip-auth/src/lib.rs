//! Authentication: passphrase verifiers, request signatures and web sessions.

pub mod error;
pub mod login;
pub mod passphrase;
pub mod request;
pub mod session;

pub use error::AuthError;
pub use login::{Logins, MAX_RECHECK_ATTEMPTS, RecheckLimit};
pub use passphrase::{HashingCost, Passphrase, PassphraseHasher};
pub use request::{Authority, Identity, RequestVerifier, SignedRequest, role_of};
pub use session::{Session, SessionId, SessionToken, SessionTokens};
