//! The local judge of a request's difficulty: text in, a class out, or nothing.
//!
//! The gateway holds one of these behind `Option`, and a gateway without one behaves
//! exactly as it did before the type existed. Everything the flow needs is `&self` and
//! sync-free; the trained encoder behind it (`ip-classify`'s `runtime` feature) is one
//! implementation, and the flow's tests use another: the flow must not need an artefact.
//!
//! A failure to classify is not a failure to serve. The flow treats `None` — refused,
//! silent, broken — as "dispatch as an unclassified request does today"; nothing here
//! refuses a request, and nothing here delays one past the clock it is called behind.

use ip_core::Difficulty;

/// How long the local judge gets to answer before its silence is taken as an answer: a
/// classification that cannot keep this clock should not slow the request it describes.
/// Set from the measured cost of one fp32 forward pass run through the flow's shape —
/// 20 ms warm at batch one, the runtime's first-call cost absorbed by the judge's
/// warm-up — with headroom for a loaded box. M14's int8 build is expected to let it
/// come down.
pub const CLASSIFY_DEADLINE: std::time::Duration = std::time::Duration::from_millis(150);

/// What one text is, as far as this judge can say: a class, when the judge will commit
/// to one; `None` when it will not, for any reason at all.
pub trait Classifier: std::fmt::Debug + Send + Sync {
    /// The class for `text`, or nothing. Never an error, never a panic, never a wait
    /// the caller didn't survive: implementations own their failure and answer `None`.
    fn classify(&self, text: &str) -> Option<Difficulty>;
}
