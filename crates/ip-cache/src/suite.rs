//! Tests every cache backend must pass, written once and run against each backend.

use std::time::Duration;

use crate::cache::Cache;
use crate::key::{CacheKey, CacheLevel};
use crate::ttl::Ttl;

/// A key in the system level, which is where the shared tests work.
pub(crate) fn key(id: &str) -> CacheKey {
    CacheKey::new(CacheLevel::System, id).unwrap()
}

/// Long enough that nothing expires while a test runs.
pub(crate) fn a_while() -> Ttl {
    Ttl::seconds(60).unwrap()
}

/// Short enough that a test can wait it out.
pub(crate) fn a_moment() -> Ttl {
    Ttl::new(Duration::from_millis(50)).unwrap()
}

pub(crate) async fn a_value_round_trips(cache: &impl Cache) {
    cache
        .put(&key("round-trip"), b"body", Some(a_while()))
        .await
        .unwrap();
    assert_eq!(
        cache.get(&key("round-trip")).await.unwrap(),
        Some(b"body".to_vec())
    );
}

pub(crate) async fn a_key_nothing_wrote_reads_as_nothing(cache: &impl Cache) {
    assert_eq!(cache.get(&key("never-written")).await.unwrap(), None);
}

pub(crate) async fn writing_again_replaces_the_value(cache: &impl Cache) {
    cache
        .put(&key("replaced"), b"first", Some(a_while()))
        .await
        .unwrap();
    cache
        .put(&key("replaced"), b"second", Some(a_while()))
        .await
        .unwrap();
    assert_eq!(
        cache.get(&key("replaced")).await.unwrap(),
        Some(b"second".to_vec())
    );
}

pub(crate) async fn removing_reports_whether_it_was_there(cache: &impl Cache) {
    cache
        .put(&key("removed"), b"body", Some(a_while()))
        .await
        .unwrap();
    assert!(cache.remove(&key("removed")).await.unwrap());
    assert!(!cache.remove(&key("removed")).await.unwrap());
    assert_eq!(cache.get(&key("removed")).await.unwrap(), None);
}

pub(crate) async fn an_entry_goes_once_its_ttl_passes(cache: &impl Cache) {
    cache
        .put(&key("brief"), b"body", Some(a_moment()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(cache.get(&key("brief")).await.unwrap(), None);
}

pub(crate) async fn an_entry_without_a_ttl_stays(cache: &impl Cache) {
    cache.put(&key("kept"), b"body", None).await.unwrap();
    cache
        .put(&key("fleeting"), b"body", Some(a_moment()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        cache.get(&key("kept")).await.unwrap(),
        Some(b"body".to_vec())
    );
    assert_eq!(cache.get(&key("fleeting")).await.unwrap(), None);
}

pub(crate) async fn a_claim_without_a_ttl_is_held(cache: &impl Cache) {
    assert!(cache.claim(&key("held"), None).await.unwrap());
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(!cache.claim(&key("held"), None).await.unwrap());
}

pub(crate) async fn a_claim_is_taken_once(cache: &impl Cache) {
    assert!(cache.claim(&key("taken"), Some(a_while())).await.unwrap());
    assert!(!cache.claim(&key("taken"), Some(a_while())).await.unwrap());
}

pub(crate) async fn a_claim_frees_once_its_ttl_passes(cache: &impl Cache) {
    assert!(
        cache
            .claim(&key("brief-claim"), Some(a_moment()))
            .await
            .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        cache
            .claim(&key("brief-claim"), Some(a_while()))
            .await
            .unwrap()
    );
}

pub(crate) async fn only_one_of_a_race_takes_the_claim(cache: &impl Cache) {
    let raced = key("raced");
    let (first, second, third, fourth) = tokio::join!(
        cache.claim(&raced, Some(a_while())),
        cache.claim(&raced, Some(a_while())),
        cache.claim(&raced, Some(a_while())),
        cache.claim(&raced, Some(a_while())),
    );
    let won = [first, second, third, fourth]
        .into_iter()
        .filter(|taken| *taken.as_ref().unwrap())
        .count();
    assert_eq!(won, 1, "a nonce must be spendable exactly once");
}

pub(crate) async fn levels_keep_their_own_entries(cache: &impl Cache) {
    let system = CacheKey::new(CacheLevel::System, "shared-id").unwrap();
    let response = CacheKey::new(CacheLevel::Response, "shared-id").unwrap();
    cache
        .put(&system, b"system", Some(a_while()))
        .await
        .unwrap();
    cache
        .put(&response, b"response", Some(a_while()))
        .await
        .unwrap();
    assert_eq!(cache.get(&system).await.unwrap(), Some(b"system".to_vec()));
    assert_eq!(
        cache.get(&response).await.unwrap(),
        Some(b"response".to_vec())
    );
}

/// Writes one `#[tokio::test]` per shared test, each on a fresh cache from `$open`, an async fn
/// that returns `None` when its backend cannot run here.
macro_rules! cache_suite {
    ($open:path) => {
        $crate::suite::cache_suite!($open, [
            a_value_round_trips,
            a_key_nothing_wrote_reads_as_nothing,
            writing_again_replaces_the_value,
            removing_reports_whether_it_was_there,
            an_entry_goes_once_its_ttl_passes,
            an_entry_without_a_ttl_stays,
            a_claim_without_a_ttl_is_held,
            a_claim_is_taken_once,
            a_claim_frees_once_its_ttl_passes,
            only_one_of_a_race_takes_the_claim,
            levels_keep_their_own_entries,
        ]);
    };
    ($open:path, [$($test:ident),* $(,)?]) => {
        $(
            #[tokio::test]
            async fn $test() {
                let Some(cache) = $open().await else {
                    return;
                };
                $crate::suite::$test(&cache).await;
            }
        )*
    };
}

pub(crate) use cache_suite;
