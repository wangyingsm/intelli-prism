//! What must not leave a request.
//!
//! A [`Secret`] is a declaration: a [`Kind`] naming it, a [`Match`] saying how it is
//! recognised — plain text, a prefix, a suffix or an expression — and optionally the
//! [`Check`] that confirms one. [`Detectors`] compiles declarations and finds them in a
//! text, saying where each one sits.
//!
//! Nothing here reads a body, touches a network or keeps anything: it is the shapes and
//! the arithmetic alone, so the hard part — which strings are really secrets and which
//! are a code sample full of digits — can be argued with in a unit test rather than
//! through a gateway.
//!
//! **An operator's secrets do not look like anyone else's**, so the set is configured
//! rather than built in. [`Secret::defaults`] is a starting point carrying the shapes
//! providers issue and the numbers whose issuers published a check; it may be taken
//! whole, taken in part, or ignored in favour of an operator's own. Where a check
//! exists it is named, and that is what makes a kind safe to switch on without first
//! measuring what it costs in false positives.

#![forbid(unsafe_code)]

pub(crate) mod check;
pub mod detect;
pub mod error;
pub mod kind;
pub mod redact;
pub mod secret;
pub mod stream;

pub use detect::{Detectors, Finding};
pub use error::FilterError;
pub use kind::Kind;
pub use redact::{Redaction, redact};
pub use secret::{Check, Least, Match, Secret};
pub use stream::{Trickle, WINDOW};
