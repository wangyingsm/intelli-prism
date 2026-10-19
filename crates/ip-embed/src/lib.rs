//! The arithmetic that turns two texts' vectors into *near enough*, or into nothing.
//!
//! Nothing here runs a model: the runtime that produces the vectors arrives later, the
//! way it does in `ip-classify`. What this crate answers, fed two embeddings, is one
//! question — *do these mean the same thing, by the operator's own standard of the
//! same?* — and it answers it with the cosine between them and a threshold nobody
//! guessed at.
//!
//! # Why a vector is normalised the moment it is built
//!
//! [`Embedding::new`] divides every vector through by its own length, so every
//! embedding this crate holds is one unit long. Three things follow, and they are the
//! reason the normalisation is here rather than at the call sites:
//!
//! - The cosine of two unit vectors is their dot product, so [`Embedding::similarity`]
//!   is a multiply-and-add with no square roots in the inner loop — which matters when
//!   it runs over every vector a tenant has cached.
//! - A distance for an index is then `1.0 - dot`, with no per-query normalisation.
//! - Some embedding models normalise their own output and some do not. Handled here,
//!   that difference stops existing; handled at the call sites, it is a silent factor
//!   on every comparison the one time somebody forgets.

pub mod error;
pub mod neighbourhood;

#[cfg(feature = "runtime")]
pub mod runtime;

pub use error::EmbedError;
pub use neighbourhood::{Capacity, MOST_HELD, Neighbourhood};

/// The widest vector this build will hold.
///
/// Models in use run from 384 to 4096 numbers. The bound is not a prediction about
/// models, it is a guard on a number that arrives from a manifest an operator points
/// at: a mistyped width otherwise allocates before anything has checked that a model of
/// that shape exists.
pub const WIDEST: usize = 8_192;

/// Shorter than this and dividing through by the length divides the rounding rather
/// than the direction.
///
/// Every component of a vector this short is numerical noise: the direction it appears
/// to point in is an artefact of how the zeros rounded, and normalising it would hand
/// back a confident unit vector pointing nowhere in particular. Refusing is the only
/// honest answer, and this is the figure that separates "a very small vector" from "no
/// vector at all" — far below anything a trained model produces, far above the point
/// where the division itself starts to overflow.
const SHORTEST: f64 = 1e-12;

/// How many numbers a vector has, which is a property of the model that made it.
///
/// Two widths are two models, and a comparison across them is not a near miss but a
/// meaningless number — which is why this is a type and not a `usize` passed around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Width(usize);

impl Width {
    /// A width, refusing nothing and refusing more than [`WIDEST`].
    pub fn new(width: usize) -> Result<Self, EmbedError> {
        if width == 0 {
            return Err(EmbedError::NoWidth);
        }
        if width > WIDEST {
            return Err(EmbedError::TooWide {
                got: width,
                most: WIDEST,
            });
        }
        Ok(Self(width))
    }

    /// The width as the number it was built from.
    #[must_use]
    pub fn get(self) -> usize {
        self.0
    }
}

impl std::fmt::Display for Width {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One text's meaning as a model put it: a vector exactly one unit long.
///
/// The length is an invariant, established at construction and relied on by every
/// comparison — there is no way to build one of these that is not normalised, and no
/// way to mutate one afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    numbers: Vec<f32>,
}

impl Embedding {
    /// Reads a model's output, refusing a width of nothing or more than [`WIDEST`], any
    /// number that is not one, and a vector too short to have a direction — then
    /// divides it through by its own length.
    ///
    /// The length is summed in `f64`. A model's output is `f32`, and over four thousand
    /// squares the single-precision sum drifts by enough to leave the result visibly
    /// off one unit long, which would show up as every similarity being slightly wrong
    /// rather than as anything that looks like a bug.
    pub fn new(numbers: &[f32]) -> Result<Self, EmbedError> {
        let _ = Width::new(numbers.len())?;
        let mut squares = 0.0f64;
        for &number in numbers {
            if !number.is_finite() {
                return Err(EmbedError::NotANumber { value: number });
            }
            squares += f64::from(number) * f64::from(number);
        }
        let length = squares.sqrt();
        if length < SHORTEST {
            return Err(EmbedError::NoDirection { length });
        }
        Ok(Self {
            numbers: numbers
                .iter()
                .map(|&number| (f64::from(number) / length) as f32)
                .collect(),
        })
    }

    /// How many numbers this vector has.
    #[must_use]
    pub fn width(&self) -> Width {
        Width(self.numbers.len())
    }

    /// The numbers themselves, one unit long in total.
    #[must_use]
    pub fn numbers(&self) -> &[f32] {
        &self.numbers
    }

    /// How near two meanings are: the cosine between them, which for unit vectors is
    /// their dot product.
    ///
    /// Vectors pointing away from each other give a negative cosine, and that is
    /// reported as [`Nearness::NOTHING`] rather than as a negative number. Nothing this
    /// crate does distinguishes *unrelated* from *opposite*: both are as far from a hit
    /// as a cache has any use for, and a threshold that could be met by a negative
    /// similarity would not be a threshold.
    pub fn similarity(&self, other: &Self) -> Result<Nearness, EmbedError> {
        if self.numbers.len() != other.numbers.len() {
            return Err(EmbedError::WidthMismatch {
                expected: self.numbers.len(),
                got: other.numbers.len(),
            });
        }
        let dot: f32 = self
            .numbers
            .iter()
            .zip(&other.numbers)
            .map(|(one, two)| one * two)
            .sum();
        // Rounding can carry the dot product of a vector with itself a hair past one,
        // and a nearness past one is not a thing.
        Ok(Nearness(dot.clamp(0.0, 1.0)))
    }
}

/// How near two meanings have to be before one may answer for the other, and how near
/// two actually are.
///
/// One type for the threshold and for the measurement, so that comparing them needs no
/// conversion and cannot compare the wrong pair of numbers. The orderings are the
/// point: a measured nearness at or above a configured one is a hit, inclusive at the
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Nearness(f32);

impl Nearness {
    /// Not near at all, which is what a negative cosine is reported as.
    pub const NOTHING: Self = Self(0.0);

    /// The same meaning exactly.
    pub const SAME: Self = Self(1.0);

    /// A nearness within the unit, refusing anything outside it and anything that is
    /// not a number.
    ///
    /// A threshold of one admits only an exact match and zero admits everything, and
    /// both are legitimate things for an operator to write; a threshold of 1.5 or of
    /// -1 is not a stricter or looser setting but a mistype, and is refused at the door
    /// rather than quietly switching the feature off.
    pub fn new(nearness: f32) -> Result<Self, EmbedError> {
        if !nearness.is_finite() || !(0.0..=1.0).contains(&nearness) {
            return Err(EmbedError::NotANearness { value: nearness });
        }
        Ok(Self(nearness))
    }

    /// The nearness as the number it was built from.
    #[must_use]
    pub fn value(self) -> f32 {
        self.0
    }

    /// Whether this nearness meets `threshold`, the boundary inclusive: exactly the
    /// nearness an operator configured is near enough.
    #[must_use]
    pub fn reaches(self, threshold: Self) -> bool {
        self.0 >= threshold.0
    }
}

impl std::fmt::Display for Nearness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.3}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(numbers: &[f32]) -> Embedding {
        Embedding::new(numbers).expect("a vector with a direction")
    }

    fn length_of(embedding: &Embedding) -> f64 {
        embedding
            .numbers()
            .iter()
            .map(|&number| f64::from(number) * f64::from(number))
            .sum::<f64>()
            .sqrt()
    }

    #[test]
    fn a_vector_is_one_unit_long_whatever_it_arrived_as() {
        // The invariant the whole crate rests on, over four orders of magnitude of
        // input scale: a model that normalises its own output and one that does not
        // are the same model by the time anything here compares them.
        for scale in [1e-6f32, 1.0, 7.5, 1e6] {
            let scaled = vector(&[3.0 * scale, -4.0 * scale, 0.0]);
            assert!(
                (length_of(&scaled) - 1.0).abs() < 1e-6,
                "a vector at scale {scale} came out {} long",
                length_of(&scaled)
            );
        }
    }

    #[test]
    fn scaling_a_vector_does_not_change_what_it_means() {
        // The direction is the meaning; the length is the model's arbitrary choice.
        let small = vector(&[1.0, 2.0, 3.0]);
        let large = vector(&[1000.0, 2000.0, 3000.0]);
        assert!(
            small
                .similarity(&large)
                .unwrap()
                .reaches(Nearness::new(0.999_99).unwrap())
        );
    }

    #[test]
    fn a_vector_means_exactly_what_it_means() {
        let one = vector(&[0.3, 0.4, -0.5]);
        assert_eq!(one.similarity(&one).unwrap(), Nearness::SAME);
    }

    #[test]
    fn two_unrelated_meanings_are_not_near() {
        let across = vector(&[1.0, 0.0])
            .similarity(&vector(&[0.0, 1.0]))
            .unwrap();
        assert!(across.value().abs() < 1e-6, "{across}");
    }

    #[test]
    fn an_opposite_meaning_is_reported_as_no_nearer_than_an_unrelated_one() {
        // Documented on `similarity`: nothing here tells apart unrelated from
        // opposite, and a negative similarity must never be able to meet a threshold.
        let opposed = vector(&[1.0, 2.0])
            .similarity(&vector(&[-1.0, -2.0]))
            .unwrap();
        assert_eq!(opposed, Nearness::NOTHING);
        assert!(!opposed.reaches(Nearness::new(0.0001).unwrap()));
        // And it does meet a threshold of zero, which is what a threshold of zero is:
        // the operator saying they want every answer this cache can find.
        assert!(opposed.reaches(Nearness::new(0.0).unwrap()));
    }

    #[test]
    fn the_threshold_boundary_falls_inside_the_hit() {
        let near = Nearness::new(0.97).unwrap();
        assert!(near.reaches(Nearness::new(0.97).unwrap()), "inclusive");
        assert!(near.reaches(Nearness::new(0.96).unwrap()));
        assert!(!near.reaches(Nearness::new(0.970_001).unwrap()));
    }

    #[test]
    fn a_vector_of_nothing_is_refused() {
        assert_eq!(Embedding::new(&[]), Err(EmbedError::NoWidth));
        assert_eq!(Width::new(0), Err(EmbedError::NoWidth));
    }

    #[test]
    fn a_vector_wider_than_any_model_is_refused_before_it_is_read() {
        let too_wide = vec![1.0f32; WIDEST + 1];
        assert_eq!(
            Embedding::new(&too_wide),
            Err(EmbedError::TooWide {
                got: WIDEST + 1,
                most: WIDEST
            })
        );
        assert!(Width::new(WIDEST).is_ok(), "the bound itself is allowed");
    }

    #[test]
    fn a_number_that_is_not_one_is_refused_wherever_it_appears() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            // Compared by hand rather than with `assert_eq!` on the error: a NaN is
            // not equal to itself, so the derived comparison would fail on the very
            // case this is here for.
            let Err(EmbedError::NotANumber { value }) = Embedding::new(&[1.0, bad, 2.0]) else {
                panic!("{bad} should be refused");
            };
            assert!(
                value.to_bits() == bad.to_bits(),
                "the error should name {bad}, and named {value}"
            );
        }
    }

    #[test]
    fn a_vector_with_no_direction_left_in_it_is_refused() {
        assert!(matches!(
            Embedding::new(&[0.0, 0.0, 0.0]),
            Err(EmbedError::NoDirection { .. })
        ));
        // Not merely the exact zero: anything short enough that normalising would
        // divide the rounding. A vector this size normalises to a confident unit
        // vector pointing wherever the last bits happened to fall.
        assert!(matches!(
            Embedding::new(&[1e-30, -1e-31]),
            Err(EmbedError::NoDirection { .. })
        ));
        // And a vector that is small but real still has a direction.
        assert!(Embedding::new(&[1e-6, 1e-6]).is_ok());
    }

    #[test]
    fn two_models_answers_are_not_compared() {
        let narrow = vector(&[1.0, 0.0]);
        let wide = vector(&[1.0, 0.0, 0.0]);
        assert_eq!(
            narrow.similarity(&wide),
            Err(EmbedError::WidthMismatch {
                expected: 2,
                got: 3
            })
        );
    }

    #[test]
    fn a_threshold_outside_the_unit_is_a_mistype_and_is_refused() {
        for bad in [1.5f32, -0.1, f32::NAN, f32::INFINITY] {
            let Err(EmbedError::NotANearness { value }) = Nearness::new(bad) else {
                panic!("{bad} should be refused");
            };
            assert!(
                value.to_bits() == bad.to_bits(),
                "the error should name {bad}, and named {value}"
            );
        }
        // The ends are both meaningful: one admits only an exact match, zero admits
        // whatever the index finds.
        assert!(Nearness::new(1.0).is_ok());
        assert!(Nearness::new(0.0).is_ok());
    }

    #[test]
    fn a_width_says_what_it_is() {
        let embedding = vector(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(embedding.width(), Width::new(4).unwrap());
        assert_eq!(embedding.width().get(), 4);
        assert_eq!(embedding.width().to_string(), "4");
    }

    #[test]
    fn a_nearness_reads_as_a_score_an_operator_can_compare() {
        // It goes out on a response header, so what it prints is part of the contract:
        // enough digits to tune a threshold by, not so many that it reads as precision
        // the model does not have.
        assert_eq!(Nearness::new(0.973_456).unwrap().to_string(), "0.973");
        assert_eq!(Nearness::SAME.to_string(), "1.000");
    }
}
