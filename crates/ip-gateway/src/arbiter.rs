//! The margin's second level: the model that is asked what the local judge could not call.

use ip_config::Secret;
use ip_core::{AbsPath, ModelName};

/// The header the gateway's own arbitrating question carries, and which every flow
/// leaves unjudged: a marked request is classified and arbitrated by neither.
pub const ARBITRATING: http::HeaderName = http::HeaderName::from_static("x-ip-arbitrating");

/// The larger model an unsure opening is arbitrated by, and how it is asked: the route
/// the question travels, the model named in it, the credential it presents, and the
/// time it is given.
///
/// The asking rides the gateway's own upstream machinery — the same resolution, the same
/// chooser, the same client. Recursion is thereby closed twice over: the question never
/// enters the flow — it is aimed and sent from within one request's forwarding step —
/// and any flow that does receive a marked question (an `ask_path` resolving back to
/// this gateway or a peer instance) skips both judges, routing without judging.
#[derive(Debug, Clone)]
pub struct Arbiter {
    model: ModelName,
    ask_path: AbsPath,
    timeout: std::time::Duration,
    /// The credential of the arbitrating upstream, presented by the question and never
    /// the caller's own.
    secret: Secret,
}

impl Arbiter {
    /// Builds the question: the route it resolves by, the model it names, the credential
    /// it presents, and the time the answer is given.
    #[must_use]
    pub fn new(
        model: ModelName,
        ask_path: AbsPath,
        timeout: std::time::Duration,
        secret: Secret,
    ) -> Self {
        Self {
            model,
            ask_path,
            timeout,
            secret,
        }
    }

    /// The model the question names.
    #[must_use]
    pub fn model(&self) -> &ModelName {
        &self.model
    }

    /// The path the question is resolved by.
    #[must_use]
    pub fn ask_path(&self) -> &AbsPath {
        &self.ask_path
    }

    /// How long the answer is given before the request it describes is sent without one:
    /// latency the opening request of an unsure conversation pays, once per turn.
    #[must_use]
    pub const fn timeout(&self) -> std::time::Duration {
        self.timeout
    }

    /// The credential the question presents.
    #[must_use]
    pub fn secret(&self) -> &Secret {
        &self.secret
    }
}
