//! The cache that answers a question nobody asked in those words before.
//!
//! The response cache is exact: a key is a digest, a hit is the same bytes, and being
//! wrong is not one of the available outcomes. This one can be wrong, and everything
//! here is about bounding how.
//!
//! # Three gates, and what each is for
//!
//! - **One question.** Only a body holding a single unanswered turn is looked up or
//!   kept, which the flow decides with [`ip_protocol::Exchange`]. In an exchange the
//!   context is most of the meaning, and matching on the question alone would answer
//!   it out of somebody else's conversation.
//! - **Near enough.** A threshold the operator sets, with no default, inclusive at the
//!   boundary — the same shape as the classifier's confidence, and for the same reason:
//!   what a near miss is worth is a judgement about their traffic, not ours.
//! - **One tenant.** Every tenant's vectors live in a neighbourhood of that tenant's
//!   own, and the stored answer is keyed under the tenant besides. Not one index with
//!   a filter on the way out, which is one forgotten condition away from serving
//!   somebody else's answer: one index each, where there is nothing to forget.
//!
//! # Where what is kept lives
//!
//! The answers go in the shared cache under [`CacheLevel::Semantic`], so they are
//! bounded and evicted like everything else there. The vectors do not: they are in
//! memory, in this process, because a shared cache has no way to ask *what is near
//! this* and scanning it over the wire for every request would cost more than the
//! upstream call. Two consequences, both real and neither hidden: a node's hit rate is
//! built from the traffic that node has seen, and a restart starts over.
//!
//! The vector's expiry and the stored answer's ttl are the same span, set from the
//! same place, so the two go together. Where the cache evicts early under its own
//! pressure a vector can outlive its answer, and the cost of that is one cache read
//! that finds nothing before the request goes upstream as it would have anyway.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ip_cache::{Cache, CacheError, CacheKey, CacheLevel, Ttl};
use ip_core::{RouteKey, TenantId, Timestamp};
use ip_embed::{Capacity, Embedding, Nearness, Neighbourhood, Width};
use sha2::{Digest, Sha256};

use crate::cache::CachedResponse;

/// What the semantic level is configured with, beside the cache it keeps answers in.
///
/// A composite rather than five arguments, and the shape the `[semantic]` stanza
/// resolves to: everything here is a decision somebody made in a file or a figure the
/// artefact carries, and none of it is invented in code.
#[derive(Debug, Clone)]
pub struct SemanticSettings {
    /// How near a question must be to be answered by another's answer.
    pub nearness: Nearness,
    /// How long an answer stands, and how long the vector that finds it is kept.
    pub ttl: Ttl,
    /// The width of vector the configured model makes.
    pub width: Width,
    /// How many vectors one tenant holds before its least recently used go.
    pub capacity: Capacity,
    /// What identifies the model these vectors are in the geometry of — the digest of
    /// its weights, from the artefact's manifest.
    ///
    /// It goes into every key, so that replacing the model cannot serve an answer
    /// found by the old one's geometry to a question asked of the new one's. Two
    /// models' vectors are not near or far from each other; they are incomparable,
    /// and the only safe thing is for them to share no keys at all.
    pub model: String,
}

/// Whose questions, asked where: one neighbourhood's worth.
///
/// The route is half of it and not decoration. One index per tenant would let a
/// question asked on one route find a vector filed on another and read that route's
/// answer — a different upstream, a different model, somebody else's reply. Keeping
/// the partitions apart is what makes that impossible rather than guarded against.
type Partition = (TenantId, RouteKey);

/// The cache that answers by meaning.
pub struct SemanticCache {
    answers: Arc<dyn Cache>,
    /// One neighbourhood per tenant and route. The partition is the isolation, not a
    /// filter applied to a shared index on the way out.
    known: Mutex<HashMap<Partition, Neighbourhood<CacheKey>>>,
    settings: SemanticSettings,
}

impl std::fmt::Debug for SemanticCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticCache")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl SemanticCache {
    /// Answers out of `answers`, on the terms `settings` sets.
    #[must_use]
    pub fn new(answers: Arc<dyn Cache>, settings: SemanticSettings) -> Self {
        Self {
            answers,
            known: Mutex::new(HashMap::new()),
            settings,
        }
    }

    /// How near a question must be to be answered. The flow puts it on the response.
    #[must_use]
    pub fn nearness(&self) -> Nearness {
        self.settings.nearness
    }

    /// The width of vector this expects, so a caller can refuse a model that disagrees
    /// before a single request is served by it.
    #[must_use]
    pub fn width(&self) -> Width {
        self.settings.width
    }

    /// The nearest question this tenant has already had answered, if one is near
    /// enough and its answer is still held.
    ///
    /// Two steps, and the second can come up empty: the vector says which answer, and
    /// the shared cache says whether that answer is still there. A vector that outlived
    /// its answer reads as a miss, which is what it is.
    pub async fn look(
        &self,
        tenant: &TenantId,
        route: &RouteKey,
        query: &Embedding,
    ) -> Result<Option<(CachedResponse, Nearness)>, CacheError> {
        let Some((key, nearness)) = self.nearest(tenant, route, query) else {
            return Ok(None);
        };
        let Some(held) = self
            .answers
            .get(&key)
            .await?
            .and_then(|entry| CachedResponse::decode(&entry))
        else {
            return Ok(None);
        };
        Ok(Some((held, nearness)))
    }

    /// Keeps `answer` as what this question means, so a later one meaning the same
    /// thing is answered from here.
    pub async fn keep(
        &self,
        tenant: &TenantId,
        route: &RouteKey,
        query: Embedding,
        answer: &CachedResponse,
    ) -> Result<(), CacheError> {
        let key = self.key_for(tenant, route, &query)?;
        self.answers
            .put(&key, &answer.encode(), Some(self.settings.ttl))
            .await?;
        // Filed only once the answer is actually there, so a vector never points at
        // something that was never written.
        self.file(tenant, route, query, key);
        Ok(())
    }

    /// The nearest vector this partition holds, under the lock and nowhere near i/o.
    fn nearest(
        &self,
        tenant: &TenantId,
        route: &RouteKey,
        query: &Embedding,
    ) -> Option<(CacheKey, Nearness)> {
        let now = Timestamp::now();
        let mut known = self.locked();
        let found = known.get_mut(&(tenant.clone(), route.clone()))?;
        match found.nearest(query, self.settings.nearness, now) {
            Ok(found) => found,
            // A query of another width is a model changed under a running index, which
            // is a fault to report rather than a question to answer badly.
            Err(error) => {
                tracing::warn!(%error, "a question could not be compared with what is known");
                None
            }
        }
    }

    /// Files a vector in its own partition, making one if this is its first.
    fn file(&self, tenant: &TenantId, route: &RouteKey, query: Embedding, key: CacheKey) {
        let now = Timestamp::now();
        let seconds = i64::try_from(self.settings.ttl.whole_seconds()).unwrap_or(i64::MAX);
        let Ok(until) = Timestamp::from_unix_seconds(now.unix_seconds().saturating_add(seconds))
        else {
            tracing::warn!("a semantic entry's expiry is past any date, so it is not kept");
            return;
        };
        let mut known = self.locked();
        let holding = known
            .entry((tenant.clone(), route.clone()))
            .or_insert_with(|| Neighbourhood::new(self.settings.width, self.settings.capacity));
        if let Err(error) = holding.insert(query, key, until, now) {
            tracing::warn!(%error, "a question could not be filed against what is known");
        }
    }

    /// The map, recovering from a poisoning rather than refusing for the process's
    /// life: one panic must not switch the level off until a restart.
    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<Partition, Neighbourhood<CacheKey>>> {
        self.known.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("the semantic cache recovered from a poisoned index lock");
            poisoned.into_inner()
        })
    }

    /// Names the answer to one question.
    fn key_for(
        &self,
        tenant: &TenantId,
        route: &RouteKey,
        query: &Embedding,
    ) -> Result<CacheKey, CacheError> {
        semantic_key(tenant, route, &self.settings.model, query)
    }
}

/// Names one cached answer, so a hit can only ever be the same tenant, on the same
/// route, in the same model's geometry.
///
/// The vector itself is hashed rather than the text, because the vector is what the
/// index hands back and what the entry is the answer to. Two runs of one model over
/// one text give the same vector and so the same key; were a model ever to answer a
/// bit differently, the cost is a second entry for one question and never a wrong
/// answer, because what finds an entry is nearness and never this.
pub fn semantic_key(
    tenant: &TenantId,
    route: &RouteKey,
    model: &str,
    query: &Embedding,
) -> Result<CacheKey, CacheError> {
    let route = route.to_string();
    let numbers: Vec<u8> = query
        .numbers()
        .iter()
        .flat_map(|number| number.to_le_bytes())
        .collect();
    let mut hasher = Sha256::new();
    for part in [
        route.as_bytes(),
        tenant.as_str().as_bytes(),
        model.as_bytes(),
        &numbers,
    ] {
        // The lengths are hashed too, so no two different parts can concatenate to the
        // same bytes.
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    CacheKey::new(CacheLevel::Semantic, &hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use ip_cache::SledCache;
    use ip_core::{AbsPath, Endpoint, Host, Port, Protocol};

    use super::*;

    fn route(path: &str) -> RouteKey {
        RouteKey::new(Endpoint::new(
            Protocol::Https,
            Host::new("api.example.com").unwrap(),
            Port::new(443).unwrap(),
            AbsPath::new(path).unwrap(),
        ))
    }

    fn tenant(name: &str) -> TenantId {
        TenantId::new(name).unwrap()
    }

    fn vector(numbers: &[f32]) -> Embedding {
        Embedding::new(numbers).expect("a vector with a direction")
    }

    fn settings() -> SemanticSettings {
        SemanticSettings {
            nearness: Nearness::new(0.95).unwrap(),
            ttl: Ttl::seconds(60).unwrap(),
            width: Width::new(3).unwrap(),
            capacity: Capacity::new(64).unwrap(),
            model: "model-one".to_owned(),
        }
    }

    fn caching(settings: SemanticSettings) -> SemanticCache {
        SemanticCache::new(Arc::new(SledCache::temporary().unwrap()), settings)
    }

    fn answer(body: &'static str) -> CachedResponse {
        CachedResponse::new(None, bytes::Bytes::from_static(body.as_bytes()))
    }

    /// A vector a stated amount off the first axis, for questions that are near
    /// without being identical.
    fn off_by(wobble: f32) -> Embedding {
        vector(&[1.0, 0.0, wobble])
    }

    #[tokio::test]
    async fn a_question_meaning_what_an_earlier_one_meant_is_answered_from_here() {
        let cache = caching(settings());
        let (acme, path) = (tenant("acme"), route("/v1/messages"));
        cache
            .keep(&acme, &path, off_by(0.0), &answer("four pounds"))
            .await
            .unwrap();
        // 1/sqrt(1 + 0.1^2) is 0.995, which clears the 0.95 threshold.
        let (found, nearness) = cache
            .look(&acme, &path, &off_by(0.1))
            .await
            .unwrap()
            .expect("near enough");
        assert_eq!(found.body(), &bytes::Bytes::from_static(b"four pounds"));
        assert!(nearness.reaches(Nearness::new(0.99).unwrap()), "{nearness}");
    }

    #[tokio::test]
    async fn a_question_that_means_something_else_is_not_answered_from_here() {
        let cache = caching(settings());
        let (acme, path) = (tenant("acme"), route("/v1/messages"));
        cache
            .keep(&acme, &path, off_by(0.0), &answer("four pounds"))
            .await
            .unwrap();
        // 1/sqrt(1 + 0.5^2) is 0.894, which does not.
        assert_eq!(cache.look(&acme, &path, &off_by(0.5)).await.unwrap(), None);
    }

    /// The gate the whole level is judged on. Two tenants asking the identical
    /// question must get their own answers and never each other's.
    #[tokio::test]
    async fn one_tenants_answer_is_never_another_tenants() {
        let cache = caching(settings());
        let path = route("/v1/messages");
        let (acme, globex) = (tenant("acme"), tenant("globex"));
        cache
            .keep(&acme, &path, off_by(0.0), &answer("acme's books"))
            .await
            .unwrap();
        assert_eq!(
            cache.look(&globex, &path, &off_by(0.0)).await.unwrap(),
            None,
            "globex was served acme's answer to the identical question"
        );
        // The positive control: without it this proves only that nothing ever hits.
        let (mine, _) = cache
            .look(&acme, &path, &off_by(0.0))
            .await
            .unwrap()
            .expect("acme's own answer is still there");
        assert_eq!(mine.body(), &bytes::Bytes::from_static(b"acme's books"));
    }

    /// The leak the partition alone does not stop, and the reason the tenant is in
    /// the key as well.
    ///
    /// The test above passes because globex has no neighbourhood of its own yet — it
    /// would pass with the tenant taken out of the key entirely. Here both tenants
    /// ask the identical question and both file an answer, so both have a
    /// neighbourhood and both reach the shared cache. Sharing a key, the second keep
    /// would overwrite the first and acme would be served globex's books.
    #[tokio::test]
    async fn two_tenants_asking_the_identical_question_each_keep_their_own_answer() {
        let cache = caching(settings());
        let path = route("/v1/messages");
        let (acme, globex) = (tenant("acme"), tenant("globex"));
        cache
            .keep(&acme, &path, off_by(0.0), &answer("acme's books"))
            .await
            .unwrap();
        cache
            .keep(&globex, &path, off_by(0.0), &answer("globex's books"))
            .await
            .unwrap();
        for (who, expected) in [(&acme, "acme's books"), (&globex, "globex's books")] {
            let (found, _) = cache
                .look(who, &path, &off_by(0.0))
                .await
                .unwrap()
                .expect("each tenant's own answer");
            assert_eq!(
                found.body(),
                &bytes::Bytes::from(expected),
                "{who} was served the wrong tenant's answer"
            );
        }
    }

    /// And the same again for the route, for the same reason: with both routes filed
    /// each has a neighbourhood, so only the key keeps the answers apart.
    #[tokio::test]
    async fn the_same_question_on_two_routes_keeps_two_answers() {
        let cache = caching(settings());
        let acme = tenant("acme");
        let (messages, complete) = (route("/v1/messages"), route("/v1/complete"));
        cache
            .keep(&acme, &messages, off_by(0.0), &answer("from messages"))
            .await
            .unwrap();
        cache
            .keep(&acme, &complete, off_by(0.0), &answer("from complete"))
            .await
            .unwrap();
        for (path, expected) in [(&messages, "from messages"), (&complete, "from complete")] {
            let (found, _) = cache
                .look(&acme, path, &off_by(0.0))
                .await
                .unwrap()
                .expect("each route's own answer");
            assert_eq!(found.body(), &bytes::Bytes::from(expected));
        }
    }

    #[tokio::test]
    async fn an_answer_given_on_one_route_is_not_given_on_another() {
        let cache = caching(settings());
        let acme = tenant("acme");
        cache
            .keep(
                &acme,
                &route("/v1/messages"),
                off_by(0.0),
                &answer("one route"),
            )
            .await
            .unwrap();
        // The vector is this tenant's and is found; the key it names is another
        // route's, so the shared cache holds nothing under it.
        assert_eq!(
            cache
                .look(&acme, &route("/v1/complete"), &off_by(0.0))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn nothing_is_answered_before_anything_is_kept() {
        let cache = caching(settings());
        assert_eq!(
            cache
                .look(&tenant("acme"), &route("/v1/messages"), &off_by(0.0))
                .await
                .unwrap(),
            None,
            "a tenant with no neighbourhood of its own answers nothing"
        );
    }

    /// A vector whose answer the cache no longer holds is a miss, not a panic and not
    /// an empty body handed back as though it were an answer.
    #[tokio::test]
    async fn a_vector_that_outlived_its_answer_reads_as_a_miss() {
        let answers = Arc::new(SledCache::temporary().unwrap());
        let cache = SemanticCache::new(answers.clone(), settings());
        let (acme, path) = (tenant("acme"), route("/v1/messages"));
        cache
            .keep(&acme, &path, off_by(0.0), &answer("four pounds"))
            .await
            .unwrap();
        let key = semantic_key(&acme, &path, "model-one", &off_by(0.0)).unwrap();
        assert!(answers.remove(&key).await.unwrap(), "it was there");
        assert_eq!(cache.look(&acme, &path, &off_by(0.0)).await.unwrap(), None);
    }

    /// Replacing the model must not let an answer found by the old geometry be served
    /// to a question asked of the new one's.
    #[test]
    fn two_models_share_no_keys() {
        let (acme, path) = (tenant("acme"), route("/v1/messages"));
        let one = semantic_key(&acme, &path, "model-one", &off_by(0.0)).unwrap();
        let two = semantic_key(&acme, &path, "model-two", &off_by(0.0)).unwrap();
        assert_ne!(one, two);
    }

    #[test]
    fn a_key_names_the_tenant_the_route_and_the_vector() {
        let path = route("/v1/messages");
        let named = |tenant: &str, route: &RouteKey, vector: &Embedding| {
            semantic_key(&self::tenant(tenant), route, "model-one", vector).unwrap()
        };
        let base = named("acme", &path, &off_by(0.0));
        assert_eq!(base, named("acme", &path, &off_by(0.0)), "the same, twice");
        assert_ne!(base, named("globex", &path, &off_by(0.0)));
        assert_ne!(base, named("acme", &route("/v1/complete"), &off_by(0.0)));
        assert_ne!(base, named("acme", &path, &off_by(0.1)));
    }

    /// The length prefixes are what stop two different sets of parts hashing alike.
    ///
    /// The two below concatenate to the identical bytes — `ac` + `memodel` and
    /// `acme` + `model` are both `acmemodel` — so without the lengths they are one
    /// key, and one tenant's answers are filed under another's.
    #[test]
    fn moving_bytes_between_the_parts_names_another_entry() {
        let path = route("/v1/messages");
        assert_ne!(
            semantic_key(&tenant("ac"), &path, "memodel", &off_by(0.0)).unwrap(),
            semantic_key(&tenant("acme"), &path, "model", &off_by(0.0)).unwrap()
        );
    }

    #[test]
    fn a_cache_says_what_it_was_configured_with() {
        let cache = caching(settings());
        assert_eq!(cache.nearness(), Nearness::new(0.95).unwrap());
        assert_eq!(cache.width(), Width::new(3).unwrap());
        // Debug must not print what is cached, only how it is configured.
        let shown = format!("{cache:?}");
        assert!(shown.contains("SemanticCache"), "{shown}");
    }

    /// A question of another model's width is a fault to report, not a question to
    /// answer badly — and reporting it must not take the level down.
    #[tokio::test]
    async fn a_question_of_the_wrong_width_answers_nothing_and_does_not_panic() {
        let cache = caching(settings());
        let (acme, path) = (tenant("acme"), route("/v1/messages"));
        cache
            .keep(&acme, &path, off_by(0.0), &answer("four pounds"))
            .await
            .unwrap();
        assert_eq!(
            cache
                .look(&acme, &path, &vector(&[1.0, 0.0]))
                .await
                .unwrap(),
            None
        );
        // And the tenant's own question still works afterwards.
        assert!(
            cache
                .look(&acme, &path, &off_by(0.0))
                .await
                .unwrap()
                .is_some()
        );
    }
}
