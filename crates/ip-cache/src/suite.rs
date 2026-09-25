//! Tests every cache backend must pass, written once and run against each backend.

use std::time::Duration;

use crate::cache::Cache;
use crate::counter::Counters;
use crate::key::{CacheKey, CacheLevel};
use crate::publication::Publication;
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

/// Long enough that an announcement arrives unless something is wrong.
const ANNOUNCED_WITHIN: Duration = Duration::from_secs(5);

pub(crate) async fn nothing_is_published_until_something_is(cache: &impl Publication) {
    assert_eq!(cache.published(&key("rules")).await.unwrap(), None);
}

pub(crate) async fn a_newer_revision_lands_and_an_older_one_does_not(cache: &impl Publication) {
    assert!(cache.publish(&key("rules"), 2, b"second").await.unwrap());
    assert!(!cache.publish(&key("rules"), 1, b"first").await.unwrap());
    assert!(!cache.publish(&key("rules"), 2, b"again").await.unwrap());
    let published = cache.published(&key("rules")).await.unwrap().unwrap();
    assert_eq!(
        (published.revision, published.value),
        (2, b"second".to_vec())
    );

    assert!(cache.publish(&key("rules"), 3, b"third").await.unwrap());
    assert_eq!(
        cache
            .published(&key("rules"))
            .await
            .unwrap()
            .unwrap()
            .revision,
        3
    );
}

pub(crate) async fn a_follower_starts_where_publishing_stands_and_hears_what_comes_after(
    cache: &impl Publication,
) {
    cache.publish(&key("rules"), 4, b"four").await.unwrap();
    let mut following = cache.follow(&key("rules")).await.unwrap();
    assert_eq!(*following.borrow_and_update(), 4);

    cache.publish(&key("rules"), 5, b"five").await.unwrap();
    tokio::time::timeout(ANNOUNCED_WITHIN, following.changed())
        .await
        .expect("the announcement arrived")
        .unwrap();
    assert_eq!(*following.borrow_and_update(), 5);
}

pub(crate) async fn a_burst_of_changes_wakes_a_follower_at_the_newest(cache: &impl Publication) {
    let mut following = cache.follow(&key("rules")).await.unwrap();
    for revision in 1..=3 {
        cache
            .publish(&key("rules"), revision, b"burst")
            .await
            .unwrap();
    }
    tokio::time::timeout(ANNOUNCED_WITHIN, async {
        while *following.borrow_and_update() < 3 {
            following.changed().await.unwrap();
        }
    })
    .await
    .expect("the newest announcement arrived");
    assert_eq!(*following.borrow(), 3);
}

pub(crate) async fn a_follower_hears_only_its_own_topic(cache: &impl Publication) {
    let mut following = cache.follow(&key("rules")).await.unwrap();
    cache
        .publish(&key("rules-other"), 9, b"other")
        .await
        .unwrap();
    let heard = tokio::time::timeout(Duration::from_millis(300), following.changed()).await;
    assert!(heard.is_err(), "a follower of one topic heard another");
}

/// Writes one `#[tokio::test]` per shared publication test, each on a fresh cache from
/// `$open`, an async fn that returns `None` when its backend cannot run here.
macro_rules! publication_suite {
    ($open:path) => {
        mod publication {
            $crate::suite::cache_suite!(
                $open,
                [
                    nothing_is_published_until_something_is,
                    a_newer_revision_lands_and_an_older_one_does_not,
                    a_follower_starts_where_publishing_stands_and_hears_what_comes_after,
                    a_burst_of_changes_wakes_a_follower_at_the_newest,
                    a_follower_hears_only_its_own_topic,
                ]
            );
        }
    };
}

pub(crate) use publication_suite;

pub(crate) async fn a_count_starts_where_the_first_call_puts_it(counters: &impl Counters) {
    assert_eq!(counters.counted(&key("fresh")).await.unwrap(), None);
    assert_eq!(
        counters.count(&key("fresh"), 3, a_while()).await.unwrap(),
        3
    );
    assert_eq!(counters.counted(&key("fresh")).await.unwrap(), Some(3));
}

pub(crate) async fn counting_again_adds_to_what_is_there(counters: &impl Counters) {
    counters.count(&key("adding"), 10, a_while()).await.unwrap();
    assert_eq!(
        counters.count(&key("adding"), 5, a_while()).await.unwrap(),
        15
    );
    assert_eq!(counters.counted(&key("adding")).await.unwrap(), Some(15));
}

pub(crate) async fn a_count_lets_go_of_itself_once_its_stretch_passes(counters: &impl Counters) {
    counters
        .count(&key("passing"), 7, a_moment())
        .await
        .unwrap();
    tokio::time::sleep(a_moment().get() * 3).await;
    assert_eq!(counters.counted(&key("passing")).await.unwrap(), None);
    assert_eq!(
        counters.count(&key("passing"), 1, a_while()).await.unwrap(),
        1
    );
}

pub(crate) async fn seeding_decides_only_when_nothing_is_counted(counters: &impl Counters) {
    assert_eq!(
        counters.seed(&key("seeded"), 100, a_while()).await.unwrap(),
        100
    );
    assert_eq!(
        counters.seed(&key("seeded"), 999, a_while()).await.unwrap(),
        100
    );
    assert_eq!(counters.counted(&key("seeded")).await.unwrap(), Some(100));

    counters.count(&key("seeded"), 5, a_while()).await.unwrap();
    assert_eq!(
        counters.seed(&key("seeded"), 999, a_while()).await.unwrap(),
        105
    );
}

pub(crate) async fn a_seeded_count_carries_on_from_what_it_was_seeded_with(
    counters: &impl Counters,
) {
    counters.seed(&key("carried"), 40, a_while()).await.unwrap();
    assert_eq!(
        counters.count(&key("carried"), 2, a_while()).await.unwrap(),
        42
    );
}

pub(crate) async fn counts_under_different_keys_never_meet(counters: &impl Counters) {
    counters.count(&key("mine"), 1, a_while()).await.unwrap();
    counters.count(&key("yours"), 2, a_while()).await.unwrap();
    assert_eq!(counters.counted(&key("mine")).await.unwrap(), Some(1));
    assert_eq!(counters.counted(&key("yours")).await.unwrap(), Some(2));
}

/// Writes one `#[tokio::test]` per shared counter test, each on a fresh cache from `$open`.
macro_rules! counter_suite {
    ($open:path) => {
        mod counter {
            $crate::suite::cache_suite!(
                $open,
                [
                    a_count_starts_where_the_first_call_puts_it,
                    counting_again_adds_to_what_is_there,
                    a_count_lets_go_of_itself_once_its_stretch_passes,
                    seeding_decides_only_when_nothing_is_counted,
                    a_seeded_count_carries_on_from_what_it_was_seeded_with,
                    counts_under_different_keys_never_meet,
                ]
            );
        }
    };
}

pub(crate) use counter_suite;
