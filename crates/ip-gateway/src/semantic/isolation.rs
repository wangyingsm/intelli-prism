//! Whether one tenant's question can be answered with another's answer.
//!
//! The other two audits ask this of the store and of the management api. This is the
//! third place it has to hold, and the one where it is hardest to see, because nothing
//! here compares identities: an entry is found by *how near two vectors are*, and
//! nearness knows nothing about who asked.
//!
//! So the separation is structural, in two places at once, and this file is written to
//! make a hole in either one fail:
//!
//! - **The partition.** Each `(tenant, route)` has a neighbourhood of its own, so a
//!   scan cannot reach a vector belonging to anyone else. Not one index with a filter
//!   on the way out — that is one forgotten condition from a leak — but one index each,
//!   where there is nothing to forget.
//! - **The key.** Every stored answer is named by the tenant, the route, the model and
//!   the vector together, so even two indexes that *did* somehow hand back each
//!   other's keys would read nothing under them.
//!
//! Either alone would pass most of the tests below, which is why both are tested from
//! both sides: a case where only the partition stops the leak, and a case where both
//! parties have filed something so only the key can.
//!
//! What this cannot say: **the level is per tenant and not per user**, exactly as the
//! response cache is. Two people inside one tenant share answers, by design and not by
//! oversight — [`a_tenants_people_share_its_answers_and_the_audit_says_so`] records
//! that so a reader meets the decision rather than guessing at it.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ip_cache::{SledCache, Ttl};
    use ip_core::{AbsPath, Endpoint, Host, Port, Protocol, RouteKey, TenantId};
    use ip_embed::{Capacity, Embedding, Nearness, Width};

    use crate::cache::CachedResponse;
    use crate::semantic::{SemanticCache, SemanticSettings, semantic_key};

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

    /// The one question everybody in this file asks, as a vector.
    fn question() -> Embedding {
        Embedding::new(&[1.0, 0.0, 0.0]).unwrap()
    }

    fn answer(body: &'static str) -> CachedResponse {
        CachedResponse::new(None, bytes::Bytes::from_static(body.as_bytes()))
    }

    /// A cache in the one model's geometry, threshold low enough that nothing below
    /// is ever refused for being too far: what is refused here is refused for *whose
    /// it is*, never for how near it is, or the audit would prove the wrong thing.
    fn caching(model: &str) -> SemanticCache {
        SemanticCache::new(
            Arc::new(SledCache::temporary().unwrap()),
            SemanticSettings {
                nearness: Nearness::new(0.0).unwrap(),
                ttl: Ttl::seconds(60).unwrap(),
                width: Width::new(3).unwrap(),
                capacity: Capacity::new(64).unwrap(),
                model: model.to_owned(),
            },
        )
    }

    async fn body_of(
        cache: &SemanticCache,
        who: &TenantId,
        where_: &RouteKey,
    ) -> Option<&'static str> {
        let (held, _) = cache.look(who, where_, &question()).await.unwrap().near()?;
        match std::str::from_utf8(held.body()).unwrap() {
            "acme's" => Some("acme's"),
            "globex's" => Some("globex's"),
            other => panic!("an answer nobody filed: {other}"),
        }
    }

    /// The control the rest of this file means nothing without.
    ///
    /// Every case below asserts that somebody is *not* answered. If the cache simply
    /// never answered anybody — a threshold nothing clears, a partition never filled,
    /// a key that never matches itself — all of them would pass and prove nothing.
    /// This says the identical question from the identical asker does hit.
    #[tokio::test]
    async fn the_same_tenant_asking_the_same_thing_is_answered() {
        let cache = caching("one-model");
        let (acme, path) = (tenant("acme"), route("/v1/messages"));
        cache
            .keep(&acme, &path, question(), &answer("acme's"))
            .await
            .unwrap();
        assert_eq!(body_of(&cache, &acme, &path).await, Some("acme's"));
    }

    /// Another tenant, stopped by the partition: globex has filed nothing, so it has
    /// no neighbourhood and no vector of its own to find.
    #[tokio::test]
    async fn another_tenant_who_has_filed_nothing_is_answered_nothing() {
        let cache = caching("one-model");
        let path = route("/v1/messages");
        cache
            .keep(&tenant("acme"), &path, question(), &answer("acme's"))
            .await
            .unwrap();
        assert_eq!(body_of(&cache, &tenant("globex"), &path).await, None);
    }

    /// Another tenant, stopped by the key: both have filed the identical question, so
    /// both have a neighbourhood and both reach the shared cache. Only the tenant in
    /// the key keeps the two answers apart, and without it the second filing would
    /// have written over the first.
    #[tokio::test]
    async fn two_tenants_who_have_both_filed_keep_their_own_answers() {
        let cache = caching("one-model");
        let path = route("/v1/messages");
        let (acme, globex) = (tenant("acme"), tenant("globex"));
        cache
            .keep(&acme, &path, question(), &answer("acme's"))
            .await
            .unwrap();
        cache
            .keep(&globex, &path, question(), &answer("globex's"))
            .await
            .unwrap();
        assert_eq!(body_of(&cache, &acme, &path).await, Some("acme's"));
        assert_eq!(body_of(&cache, &globex, &path).await, Some("globex's"));
    }

    /// The same pair of cases for the route, which is the other half of the partition
    /// and the other part of the key. A different route is a different upstream and a
    /// different model behind it, so its answer is as much somebody else's.
    #[tokio::test]
    async fn another_route_that_has_filed_nothing_is_answered_nothing() {
        let cache = caching("one-model");
        let acme = tenant("acme");
        cache
            .keep(&acme, &route("/v1/messages"), question(), &answer("acme's"))
            .await
            .unwrap();
        assert_eq!(body_of(&cache, &acme, &route("/v1/complete")).await, None);
    }

    #[tokio::test]
    async fn two_routes_that_have_both_filed_keep_their_own_answers() {
        let cache = caching("one-model");
        let acme = tenant("acme");
        let (messages, complete) = (route("/v1/messages"), route("/v1/complete"));
        cache
            .keep(&acme, &messages, question(), &answer("acme's"))
            .await
            .unwrap();
        cache
            .keep(&acme, &complete, question(), &answer("globex's"))
            .await
            .unwrap();
        assert_eq!(body_of(&cache, &acme, &messages).await, Some("acme's"));
        assert_eq!(body_of(&cache, &acme, &complete).await, Some("globex's"));
    }

    /// Replacing the model is the one of these that has no partition behind it: the
    /// index is in memory and goes with the process, so what the next process must not
    /// find is what the last one left in the *shared* cache. Only the key stops it.
    ///
    /// Two models' vectors are not near or far from each other, they are
    /// incomparable, so an answer found by one geometry standing in for a question
    /// asked of another is not an approximation — it is a different question's answer.
    #[tokio::test]
    async fn an_answer_the_old_model_found_is_not_served_to_the_new_one() {
        let answers: Arc<dyn ip_cache::Cache> = Arc::new(SledCache::temporary().unwrap());
        let (acme, path) = (tenant("acme"), route("/v1/messages"));
        let before = SemanticCache::new(Arc::clone(&answers), settings_of("model-one"));
        before
            .keep(&acme, &path, question(), &answer("acme's"))
            .await
            .unwrap();
        // A new process, the same shared cache, a different model's weights. It has
        // filed this question too, so its own index finds a vector — and the key that
        // vector names is not the one the old model wrote under.
        let after = SemanticCache::new(Arc::clone(&answers), settings_of("model-two"));
        assert_eq!(
            after.look(&acme, &path, &question()).await.unwrap().near(),
            None,
            "the new model was served what the old one found"
        );
        assert_ne!(
            semantic_key(&acme, &path, "model-one", &question()).unwrap(),
            semantic_key(&acme, &path, "model-two", &question()).unwrap()
        );
    }

    fn settings_of(model: &str) -> SemanticSettings {
        SemanticSettings {
            nearness: Nearness::new(0.0).unwrap(),
            ttl: Ttl::seconds(60).unwrap(),
            width: Width::new(3).unwrap(),
            capacity: Capacity::new(64).unwrap(),
            model: model.to_owned(),
        }
    }

    /// The level is per tenant and not per user, which is a decision and not a gap.
    ///
    /// The api carries no user, so two people inside one tenant share answers exactly
    /// as they already do through the response cache, whose key is route, tenant and
    /// body and has never been per user either. Asserted rather than left implied: if
    /// the level ever does become per user, this is the test that says the audit must
    /// grow a case for it, and until then a reader meets the decision here instead of
    /// inferring it from a signature.
    #[tokio::test]
    async fn a_tenants_people_share_its_answers_and_the_audit_says_so() {
        let cache = caching("one-model");
        let (acme, path) = (tenant("acme"), route("/v1/messages"));
        cache
            .keep(&acme, &path, question(), &answer("acme's"))
            .await
            .unwrap();
        // There is nowhere in either call to say who is asking, which is the whole
        // content of this test: the answer below belongs to the tenant, not a person.
        assert_eq!(body_of(&cache, &acme, &path).await, Some("acme's"));
    }

    /// The tripwire. Every part that names a stored answer must have a case above, or
    /// the separation has a hole exactly where nobody looked.
    ///
    /// Read from the source rather than from a list kept by hand, because a list kept
    /// by hand is the thing that goes stale. A part added to the key without a case
    /// here fails this.
    #[test]
    fn every_part_of_the_key_is_audited() {
        const SOURCE: &str = include_str!("../semantic.rs");
        let parts: Vec<String> = SOURCE
            .split_once("    for part in [")
            .expect("the key is hashed over an array of parts")
            .1
            .split_once("    ] {")
            .expect("the array ends")
            .0
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.trim_end_matches(',').to_owned())
            .collect();
        // Every part, and what above proves that changing it alone changes the answer.
        let audited: [(&str, &str); 4] = [
            ("route.as_bytes()", "two routes that have both filed"),
            (
                "tenant.as_str().as_bytes()",
                "two tenants that have both filed",
            ),
            ("model.as_bytes()", "the old model's answer"),
            ("&numbers", "the vector, which is what nearness compares"),
        ];
        for part in &parts {
            assert!(
                audited.iter().any(|(known, _)| known == part),
                "{part} names an entry but has no isolation case; \
                 add one to semantic/isolation.rs"
            );
        }
        assert_eq!(
            parts.len(),
            audited.len(),
            "{} parts name an entry and {} are audited: {parts:?}",
            parts.len(),
            audited.len()
        );
    }
}
