//! What turns a question into the vector the semantic cache knows it by.
//!
//! A trait, so this crate never depends on an inference runtime: the binary builds the
//! implementation when it carries the feature, and the flow asks an `Option<Arc<dyn
//! Embedder>>` exactly as it asks the classifier. Nothing here fails a request and
//! nothing here delays one past the clock it is called behind.

use ip_embed::Embedding;

/// How long the embedder gets before its silence is taken as a miss.
///
/// This is a ceiling, not the usual cost: a small encoder answers in tens of
/// milliseconds, and the deadline only bites on a loaded box. It matters more than the
/// classifier's does, though, for a reason worth stating. The classifier runs to avoid
/// an arbiter call and costs nothing when it is wrong. This runs in front of the
/// upstream call it is trying to avoid, so **every request that misses pays it**, and
/// the level only earns its keep where enough of them hit. An operator watching
/// `x-ip-nearness` against their own traffic is watching exactly that trade.
pub const EMBED_DEADLINE: std::time::Duration = std::time::Duration::from_millis(150);

/// What one text means, as far as the configured model can say: a vector, or nothing
/// when it will not commit to one for any reason at all.
pub trait Embedder: std::fmt::Debug + Send + Sync {
    /// The vector for `text`, or nothing. Never an error, never a panic, never a wait
    /// the caller did not survive: implementations own their failure and answer
    /// `None`, which costs a cache miss and never a request.
    fn embed(&self, text: &str) -> Option<Embedding>;
}
