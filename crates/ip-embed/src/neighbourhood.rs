//! A bounded set of vectors, and the nearest of them to a question — if any is near
//! enough.
//!
//! # Why this is a scan and not a graph
//!
//! `DESIGN.md` names an HNSW index, and that is the right structure for a million
//! vectors. This one is per tenant and holds only single-turn requests, which puts its
//! working size in the thousands, and there the measurement says the opposite of what
//! intuition does — an exact scan is *faster* than an approximate search, as well as
//! being exact. Measured on random unit vectors, nearest of 2,000 queries, release
//! build:
//!
//! | numbers | held | exact scan | hnsw search | hnsw build |
//! |---|---|---|---|---|
//! | 384 | 1,000 | 209 µs | 345 µs | 0.98 s |
//! | 384 | 4,000 | 847 µs | 926 µs | 5.17 s |
//! | 384 | 16,000 | 3.46 ms | 1.67 ms | 25.7 s |
//! | 768 | 4,000 | 1.68 ms | 1.73 ms | 11.1 s |
//! | 768 | 16,000 | 7.57 ms | 3.76 ms | 59.1 s |
//!
//! The graph only starts to pay at sixteen thousand, and the build column is why it
//! could not be used even there: the crate that builds it cannot grow one, so every
//! eviction would mean rebuilding, and a rebuild outlasts the entries it rebuilds.
//!
//! The consequence worth keeping in mind is that **the cost of a lookup is linear in
//! what a tenant holds**, which is what [`Capacity`] exists to bound. The day a
//! tenant's index is genuinely large, what changes is the body of [`Neighbourhood::
//! nearest`] and nothing else: no caller learns that the answer became approximate.

use ip_core::Timestamp;

use crate::{EmbedError, Embedding, Nearness, Width};

/// The most vectors one neighbourhood will be allowed to hold.
///
/// Two separate ceilings meet near this figure and neither is arbitrary. A scan is
/// linear, so at 768 numbers apiece this many is already tens of milliseconds a
/// lookup — past which the cache costs more than the upstream call it is avoiding for
/// everyone who misses. And the vectors alone are some two hundred megabytes, per
/// tenant, held for as long as the process runs.
pub const MOST_HELD: usize = 65_536;

/// How many vectors a neighbourhood holds before the least recently used ones go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Capacity(usize);

impl Capacity {
    /// A bound, refusing nothing — which would be a cache that never hits, written as
    /// if it were a setting — and refusing more than [`MOST_HELD`].
    pub fn new(held: usize) -> Result<Self, EmbedError> {
        if held == 0 {
            return Err(EmbedError::HoldsNothing);
        }
        if held > MOST_HELD {
            return Err(EmbedError::HoldsTooMuch {
                got: held,
                most: MOST_HELD,
            });
        }
        Ok(Self(held))
    }

    /// The bound as the number it was built from.
    #[must_use]
    pub fn get(self) -> usize {
        self.0
    }
}

/// One vector and what was filed under it.
#[derive(Debug, Clone)]
struct Held<T> {
    embedding: Embedding,
    value: T,
    /// When this stops being worth returning, whatever is still stored against it.
    until: Timestamp,
    /// Which read or write touched this last, counted rather than clocked: two uses
    /// in one second are ordered, where two timestamps of whole seconds are tied.
    used: u64,
}

/// Every vector one tenant has filed, and the nearest of them to a question.
///
/// Bounded two ways at once, because either alone leaks. Each entry expires at a time
/// of its own, so an answer that has gone stale stops being offered; and the set as a
/// whole is capped, so a tenant asking endlessly varied questions evicts its own least
/// recently used rather than growing without limit.
#[derive(Debug, Clone)]
pub struct Neighbourhood<T> {
    width: Width,
    capacity: Capacity,
    held: Vec<Held<T>>,
    /// The next value of `Held::used`, so every touch orders after every earlier one.
    tick: u64,
}

impl<T> Neighbourhood<T> {
    /// An empty neighbourhood for vectors of one width, holding at most `capacity`.
    #[must_use]
    pub fn new(width: Width, capacity: Capacity) -> Self {
        Self {
            width,
            capacity,
            held: Vec::new(),
            tick: 0,
        }
    }

    /// The width of vector this holds. Anything else is refused rather than compared.
    #[must_use]
    pub fn width(&self) -> Width {
        self.width
    }

    /// How many it holds before the least recently used go.
    #[must_use]
    pub fn capacity(&self) -> Capacity {
        self.capacity
    }

    /// How many vectors are filed, expired ones included until something sweeps them.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Whether anything is filed at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Drops everything that has stopped being worth returning.
    ///
    /// Swept on the way into every read and every write rather than on a timer: both
    /// already walk the whole set, so this costs nothing either of them was not
    /// already paying, and nothing expired is ever compared against or counted toward
    /// the bound.
    fn forget_expired(&mut self, now: Timestamp) {
        self.held.retain(|held| held.until > now);
    }

    /// The next tick, so a touch orders after everything before it.
    fn touch(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }
}

impl<T: Clone + PartialEq> Neighbourhood<T> {
    /// Files `embedding` under `value` until `until`, refusing a vector of the wrong
    /// width.
    ///
    /// Filing something already filed under the same value replaces it rather than
    /// adding a second copy — the value is what the question resolved to, so the same
    /// question twice is one entry with a renewed expiry, never two competing for the
    /// bound.
    pub fn insert(
        &mut self,
        embedding: Embedding,
        value: T,
        until: Timestamp,
        now: Timestamp,
    ) -> Result<(), EmbedError> {
        if embedding.width() != self.width {
            return Err(EmbedError::WidthMismatch {
                expected: self.width.get(),
                got: embedding.width().get(),
            });
        }
        self.forget_expired(now);
        let used = self.touch();
        if let Some(already) = self.held.iter_mut().find(|held| held.value == value) {
            already.embedding = embedding;
            already.until = until;
            already.used = used;
            return Ok(());
        }
        // Room is made one entry at a time rather than in a sweep: the bound can only
        // ever be exceeded by the one about to be added.
        while self.held.len() >= self.capacity.get() {
            let Some(oldest) = self
                .held
                .iter()
                .enumerate()
                .min_by_key(|(_, held)| held.used)
                .map(|(at, _)| at)
            else {
                break;
            };
            self.held.swap_remove(oldest);
        }
        self.held.push(Held {
            embedding,
            value,
            until,
            used,
        });
        Ok(())
    }

    /// What `query` means the same thing as, if anything filed here does.
    ///
    /// The nearest entry wins, and only if it reaches `threshold` — the boundary
    /// inclusive. Ties go to whichever was filed first, which only two vectors
    /// identical to the last bit can produce. A hit counts as a use, so what is
    /// actually being asked for is what survives the bound.
    pub fn nearest(
        &mut self,
        query: &Embedding,
        threshold: Nearness,
        now: Timestamp,
    ) -> Result<Found<T>, EmbedError> {
        if query.width() != self.width {
            return Err(EmbedError::WidthMismatch {
                expected: self.width.get(),
                got: query.width().get(),
            });
        }
        self.forget_expired(now);
        let mut best: Option<(usize, Nearness)> = None;
        for (at, held) in self.held.iter().enumerate() {
            let nearness = held.embedding.similarity(query)?;
            if best.is_none_or(|(_, standing)| nearness > standing) {
                best = Some((at, nearness));
            }
        }
        let Some((at, nearness)) = best else {
            return Ok(Found::Nothing);
        };
        if !nearness.reaches(threshold) {
            return Ok(Found::Short(nearness));
        }
        let used = self.touch();
        self.held[at].used = used;
        Ok(Found::Near(self.held[at].value.clone(), nearness))
    }
}

/// What a scan found, and how near it was.
///
/// Three outcomes rather than an `Option`, because *nothing was near enough* and
/// *nothing is filed here* are different facts and only the first has a distance to
/// report. That distance is the whole reason this type exists: a threshold can only
/// be chosen by somebody who can see what their own traffic nearly matched, and an
/// `Option` throws exactly that away.
///
/// The threshold is applied here rather than by the caller so that the index still
/// knows whether a hit happened — a read keeps an entry alive, and an index that did
/// not know it had been read could not say which entries to forget.
#[derive(Debug, Clone, PartialEq)]
pub enum Found<T> {
    /// The nearest was near enough: here it is, and how near.
    Near(T, Nearness),
    /// Something was nearest and is not being returned, with how near it came.
    Short(Nearness),
    /// Nothing is filed here at all, so there is no distance to speak of.
    Nothing,
}

impl<T> Found<T> {
    /// How near the nearest came, whether or not it was near enough.
    ///
    /// Nothing when nothing was filed: an empty index has no near miss, and counting
    /// one as a zero would put a bar at the bottom of every histogram for traffic
    /// that was never compared with anything.
    #[must_use]
    pub fn nearness(&self) -> Option<Nearness> {
        match self {
            Self::Near(_, nearness) | Self::Short(nearness) => Some(*nearness),
            Self::Nothing => None,
        }
    }

    /// What was found, if it was near enough to return.
    #[must_use]
    pub fn near(self) -> Option<(T, Nearness)> {
        match self {
            Self::Near(value, nearness) => Some((value, nearness)),
            Self::Short(_) | Self::Nothing => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_unix_seconds(seconds).expect("a time")
    }

    fn vector(numbers: &[f32]) -> Embedding {
        Embedding::new(numbers).expect("a vector with a direction")
    }

    fn near(value: f32) -> Nearness {
        Nearness::new(value).expect("a nearness")
    }

    fn three() -> Width {
        Width::new(3).expect("a width")
    }

    fn holding(capacity: usize) -> Neighbourhood<&'static str> {
        Neighbourhood::new(three(), Capacity::new(capacity).expect("a bound"))
    }

    /// A vector a little off another one, by a stated amount of a third axis.
    fn off_by(wobble: f32) -> Embedding {
        vector(&[1.0, 0.0, wobble])
    }

    #[test]
    fn what_was_filed_is_found_again() {
        let mut held = holding(8);
        held.insert(vector(&[1.0, 0.0, 0.0]), "answer", at(100), at(0))
            .unwrap();
        let found = held
            .nearest(&vector(&[1.0, 0.0, 0.0]), near(0.99), at(0))
            .unwrap();
        assert_eq!(found, Found::Near("answer", Nearness::SAME));
    }

    #[test]
    fn a_question_near_enough_hits_and_one_a_shade_further_does_not() {
        let mut held = holding(8);
        held.insert(vector(&[1.0, 0.0, 0.0]), "answer", at(100), at(0))
            .unwrap();
        // The cosine of this against the axis is 1/sqrt(1 + w^2); at w = 0.1 that is
        // 0.995, and at w = 0.5 it is 0.894.
        let close = held.nearest(&off_by(0.1), near(0.99), at(0)).unwrap();
        assert_eq!(close.near().map(|(value, _)| value), Some("answer"));
        // Short, not nothing — and it says how short, which is the whole point of
        // the distinction: 0.894 against a threshold of 0.99 is a near miss worth
        // seeing, and an `Option` would have thrown the number away.
        let far = held.nearest(&off_by(0.5), near(0.99), at(0)).unwrap();
        let Found::Short(missed) = far else {
            panic!("0.894 does not reach 0.99, and something was there: {far:?}");
        };
        assert!(
            missed.reaches(near(0.89)) && !missed.reaches(near(0.9)),
            "{missed}"
        );
        // And the same far question does hit a threshold set low enough for it.
        let lowered = held.nearest(&off_by(0.5), near(0.89), at(0)).unwrap();
        assert_eq!(lowered.near().map(|(value, _)| value), Some("answer"));
    }

    #[test]
    fn the_nearest_of_several_is_the_one_returned() {
        let mut held = holding(8);
        held.insert(off_by(0.5), "further", at(100), at(0)).unwrap();
        held.insert(off_by(0.05), "nearest", at(100), at(0))
            .unwrap();
        held.insert(off_by(0.2), "middling", at(100), at(0))
            .unwrap();
        let (value, _) = held
            .nearest(&vector(&[1.0, 0.0, 0.0]), near(0.5), at(0))
            .unwrap()
            .near()
            .expect("one of them is near enough");
        assert_eq!(value, "nearest");
    }

    #[test]
    fn the_threshold_boundary_falls_inside_the_hit() {
        let mut held = holding(8);
        held.insert(off_by(0.1), "answer", at(100), at(0)).unwrap();
        let (_, measured) = held
            .nearest(&vector(&[1.0, 0.0, 0.0]), near(0.0), at(0))
            .unwrap()
            .near()
            .expect("something is there");
        // Exactly the nearness it measured is near enough; a hair more is not.
        assert!(
            held.nearest(&vector(&[1.0, 0.0, 0.0]), measured, at(0))
                .unwrap()
                .near()
                .is_some()
        );
        let over = near((measured.value() + 1e-5).min(1.0));
        assert!(
            held.nearest(&vector(&[1.0, 0.0, 0.0]), over, at(0))
                .unwrap()
                .near()
                .is_none()
        );
    }

    #[test]
    fn an_answer_that_has_gone_stale_is_not_offered() {
        let mut held = holding(8);
        held.insert(vector(&[1.0, 0.0, 0.0]), "answer", at(100), at(0))
            .unwrap();
        let still = held
            .nearest(&vector(&[1.0, 0.0, 0.0]), near(0.9), at(99))
            .unwrap();
        assert!(still.near().is_some(), "it has not expired yet");
        let gone = held
            .nearest(&vector(&[1.0, 0.0, 0.0]), near(0.9), at(100))
            .unwrap();
        assert_eq!(
            gone,
            Found::Nothing,
            "the expiry is the moment it stops, inclusive, and a swept index has \
             nothing to report a distance to"
        );
        assert!(
            held.is_empty(),
            "and it is swept rather than merely skipped"
        );
    }

    #[test]
    fn filing_past_the_bound_drops_the_least_recently_used() {
        let mut held = holding(2);
        held.insert(off_by(0.1), "first", at(100), at(0)).unwrap();
        held.insert(off_by(0.2), "second", at(100), at(0)).unwrap();
        held.insert(off_by(0.3), "third", at(100), at(0)).unwrap();
        assert_eq!(held.len(), 2, "the bound holds");
        // "first" was the oldest, so it is the one that went.
        let found = held.nearest(&off_by(0.1), near(0.0), at(0)).unwrap();
        assert_eq!(found.near().map(|(value, _)| value), Some("second"));
    }

    /// The property a bound without an ordering would not have, and the one a cache
    /// lives or dies by: being asked for is what keeps an entry alive.
    #[test]
    fn being_read_saves_an_entry_from_being_the_one_that_goes() {
        let mut held = holding(2);
        held.insert(off_by(0.1), "first", at(100), at(0)).unwrap();
        held.insert(off_by(0.9), "second", at(100), at(0)).unwrap();
        // Reading the older one makes the newer one the least recently used.
        let read = held.nearest(&off_by(0.1), near(0.9), at(0)).unwrap();
        assert_eq!(read.near().map(|(value, _)| value), Some("first"));
        held.insert(off_by(0.95), "third", at(100), at(0)).unwrap();
        assert_eq!(held.len(), 2);
        let survivor = held.nearest(&off_by(0.1), near(0.99), at(0)).unwrap();
        assert_eq!(
            survivor.near().map(|(value, _)| value),
            Some("first"),
            "the entry that was read should have outlived the one that was not"
        );
    }

    #[test]
    fn the_same_question_twice_is_one_entry_with_a_renewed_expiry() {
        let mut held = holding(8);
        held.insert(off_by(0.1), "answer", at(100), at(0)).unwrap();
        held.insert(off_by(0.1), "answer", at(500), at(0)).unwrap();
        assert_eq!(held.len(), 1, "one value, one entry");
        let later = held.nearest(&off_by(0.1), near(0.9), at(200)).unwrap();
        assert!(
            later.near().is_some(),
            "the second filing should have carried the later expiry"
        );
    }

    #[test]
    fn another_models_vector_is_refused_rather_than_compared() {
        let mut held = holding(8);
        let wrong = vector(&[1.0, 0.0]);
        assert_eq!(
            held.insert(wrong.clone(), "answer", at(100), at(0)),
            Err(EmbedError::WidthMismatch {
                expected: 3,
                got: 2
            })
        );
        assert_eq!(
            held.nearest(&wrong, near(0.9), at(0)),
            Err(EmbedError::WidthMismatch {
                expected: 3,
                got: 2
            })
        );
    }

    /// The distinction this type exists for: *nothing was near enough* carries a
    /// number and *nothing is filed here* does not.
    ///
    /// A threshold can only be chosen by somebody who can see what their own traffic
    /// nearly matched. Reporting an empty index as a nearness of zero would put a bar
    /// at the bottom of every histogram for traffic that was never compared with
    /// anything, and reporting a near miss as nothing at all would hide the bulge
    /// just under the line that says the threshold is too tight.
    #[test]
    fn a_near_miss_reports_its_distance_and_an_empty_index_has_none() {
        let mut held = holding(8);
        assert_eq!(
            held.nearest(&off_by(0.1), near(0.99), at(0)).unwrap(),
            Found::Nothing
        );
        assert_eq!(
            held.nearest(&off_by(0.1), near(0.99), at(0))
                .unwrap()
                .nearness(),
            None,
            "nothing was compared, so there is no distance"
        );

        held.insert(vector(&[1.0, 0.0, 0.0]), "answer", at(100), at(0))
            .unwrap();
        let missed = held
            .nearest(&off_by(0.5), near(0.99), at(0))
            .unwrap()
            .nearness()
            .expect("something was compared, even though it fell short");
        // 1/sqrt(1 + 0.5^2) is 0.894.
        assert!(missed.reaches(near(0.89)), "{missed}");
        assert!(!missed.reaches(near(0.9)), "{missed}");
    }

    /// A miss leaves the ordering alone. Only a hit counts as a use, or a question
    /// nobody could answer would keep alive the entry it failed to match.
    #[test]
    fn falling_short_does_not_count_as_having_read_the_entry() {
        let mut held = holding(2);
        held.insert(off_by(0.1), "first", at(100), at(0)).unwrap();
        held.insert(off_by(0.9), "second", at(100), at(0)).unwrap();
        // Nearest to "first" and still short of the threshold, so nothing is touched.
        // The query is not identical to anything filed, or it would score 1.0 and no
        // threshold could make it fall short.
        assert!(matches!(
            held.nearest(&off_by(0.15), near(0.999), at(0)).unwrap(),
            Found::Short(_)
        ));
        held.insert(off_by(0.95), "third", at(100), at(0)).unwrap();
        assert_eq!(
            held.nearest(&off_by(0.1), near(0.0), at(0))
                .unwrap()
                .near()
                .map(|(value, _)| value),
            Some("second"),
            "the oldest should still have gone, since the near miss read nothing"
        );
    }

    #[test]
    fn nothing_filed_means_nothing_found() {
        let mut held = holding(8);
        assert!(held.is_empty());
        assert_eq!(
            held.nearest(&vector(&[1.0, 0.0, 0.0]), near(0.0), at(0))
                .unwrap(),
            Found::Nothing,
            "not even at a threshold of nothing: there is nothing to return, and no \
             distance to report either"
        );
    }

    #[test]
    fn a_bound_of_nothing_or_of_more_than_can_be_scanned_is_refused() {
        assert_eq!(Capacity::new(0), Err(EmbedError::HoldsNothing));
        assert_eq!(
            Capacity::new(MOST_HELD + 1),
            Err(EmbedError::HoldsTooMuch {
                got: MOST_HELD + 1,
                most: MOST_HELD
            })
        );
        assert_eq!(Capacity::new(MOST_HELD).unwrap().get(), MOST_HELD);
    }

    #[test]
    fn a_neighbourhood_says_what_it_is_for() {
        let held = holding(16);
        assert_eq!(held.width(), three());
        assert_eq!(held.capacity(), Capacity::new(16).unwrap());
    }
}
