//! The maths that turns a distribution into a class, or into nothing.
//!
//! Nothing here runs a model: the runtime that produces the distribution arrives later.
//! What this crate answers, fed a distribution over the five classes, is one question —
//! *is this confident enough to act on, and if so, at what class?* — and it answers it with
//! the margin between the top two probabilities, never the top probability alone.

pub mod error;

use ip_core::Difficulty;

pub use error::ClassifyError;

/// The confidence threshold the gate holds, as a bare number from configuration.
///
/// Which threshold an operator wants is a valuation of a mis-routed request against an
/// arbiter call, and the measured trade is theirs to pick — so it is never a constant
/// here. A threshold outside 0.0..=1.0 refuses everything or passes everything, and both
/// are mistypes rather than decisions, so they are refused at the door.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Confidence(f64);

impl Confidence {
    /// The threshold itself, at the boundary and inclusive: exactly this margin passes.
    pub fn new(threshold: f64) -> Result<Self, ClassifyError> {
        if !is_finite_number(threshold) {
            return Err(ClassifyError::NotANumber);
        }
        if !(0.0..=1.0).contains(&threshold) {
            return Err(ClassifyError::ThresholdOutOfRange { value: threshold });
        }
        Ok(Self(threshold))
    }

    /// The threshold as the number it was built from.
    pub fn value(self) -> f64 {
        self.0
    }
}

/// A softmax's divisor, as the fitted figure of one artefact that it is.
///
/// The temperature divides the logits before the exponentials: above one it flattens a
/// distribution, below one it sharpens it. It is a property of trained weights — read
/// from a manifest beside the model, never from operator hands — and a quantized build
/// of the same weights fits a different one, because quantization moves the logits.
/// Zero divides by nothing and a negative one flips the ordering the gate reads, so
/// neither is a temperature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Temperature(f64);

impl Temperature {
    /// A divisor for the logits: always positive, and refused otherwise.
    pub fn new(temperature: f64) -> Result<Self, ClassifyError> {
        if !is_finite_number(temperature) {
            return Err(ClassifyError::NotANumber);
        }
        if temperature <= 0.0 {
            return Err(ClassifyError::TemperatureNotPositive { value: temperature });
        }
        Ok(Self(temperature))
    }

    /// The divisor as the number it was built from.
    pub fn value(self) -> f64 {
        self.0
    }
}

/// A probability for each of the five classes, in the order of [`Difficulty::ALL`].
///
/// The alignment with `Difficulty::ALL` *is* the contract: this crate carries no class
/// labels of its own, and a silent reordering of that constant would mis-route every
/// request while every test that builds a distribution from the same constant still
/// passed. The test module asserts the alignment against the constant directly, so it
/// breaks the test instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Distribution([f64; Difficulty::COUNT]);

impl Distribution {
    /// Reads the probabilities in the order of the classes, refusing any that is not a
    /// number or lies outside 0.0..=1.0, any vector that is not one per class, and any
    /// set that is not a distribution — probabilities summing to one.
    ///
    /// The sum is checked within [`SUM_TOLERANCE`], an absolute slack chosen for what
    /// float addition can cost reading five figures (well under 1e-12 per summation) and
    /// still far below any error that means something: an export missing one class of
    /// five sums a fifth short of one and is refused rather than routed at. `softmax`
    /// always satisfies this by construction — its own normalisation is the sum — so the
    /// check exists for the path that feeds this crate from outside, where a mis-read
    /// runtime export should be news at the door rather than a silent mis-route.
    pub fn new(probabilities: &[f64]) -> Result<Self, ClassifyError> {
        if probabilities.len() != Difficulty::COUNT {
            return Err(ClassifyError::WrongWidth {
                got: probabilities.len(),
            });
        }
        let mut held = [0.0; Difficulty::COUNT];
        let mut sum = 0.0;
        for (slot, &probability) in held.iter_mut().zip(probabilities) {
            if !is_finite_number(probability) {
                return Err(ClassifyError::NotANumber);
            }
            if !(0.0..=1.0).contains(&probability) {
                return Err(ClassifyError::OutOfRange { value: probability });
            }
            sum += probability;
            *slot = probability;
        }
        if (sum - 1.0).abs() > SUM_TOLERANCE {
            return Err(ClassifyError::DoesNotSum { sum });
        }
        Ok(Self(held))
    }

    /// The softmax of raw model output at a temperature: probabilities, in class order.
    ///
    /// The exponentials are taken relative to the largest logit so the sum cannot
    /// overflow what a finite logit would produce; logits too large in magnitude to keep
    /// even so are refused rather than softened into an infinity that reads as certainty.
    /// Softening does not have to sum to one before it — every probability is one after.
    pub fn softmax(logits: &[f64], temperature: Temperature) -> Result<Self, ClassifyError> {
        if logits.len() != Difficulty::COUNT {
            return Err(ClassifyError::WrongWidth { got: logits.len() });
        }
        for &logit in logits {
            if !is_finite_number(logit) {
                return Err(ClassifyError::NotANumber);
            }
        }
        // The shift the doc comment promises: the largest subtraction is exactly zero, so
        // every exponential lands in (0.0, 1.0], the sum is at least 1.0, and no finite
        // logit at any temperature can divide the exponent far enough down to underflow
        // the whole sum to zero.
        let largest = logits.iter().copied().fold(f64::MIN, f64::max);
        let mut held = [0.0; Difficulty::COUNT];
        let mut sum = 0.0;
        for (slot, &logit) in held.iter_mut().zip(logits) {
            let scaled = ((logit - largest) / temperature.value()).exp();
            *slot = scaled;
            sum += scaled;
        }
        for probability in &mut held {
            *probability /= sum;
        }
        Ok(Self(held))
    }

    /// The probability the gate ranks by: the top minus the second, not the top.
    ///
    /// A model can be 0.5 confident with nothing close behind it, or 0.9 confident with
    /// 0.89 behind it, and only the second case is ambiguous — which is why the raw
    /// maximum is not the confidence measure and never was. Ties yield zero: the margin
    /// of "equally most likely" is the definition of no margin, and the gate reads it as
    /// such.
    pub fn margin(&self) -> Margin {
        // The winner's index is carried out of the scan the gap is measured on, so no
        // second pass recovers a position by comparing floats: the ordering is decided
        // once, here, and `Difficulty::ALL` is read at exactly the slot the scan won.
        let mut largest = f64::MIN;
        let mut second = f64::MIN;
        let mut winner = 0;
        for (index, &probability) in self.0.iter().enumerate() {
            if probability > largest {
                second = largest;
                largest = probability;
                winner = index;
            } else if probability > second {
                second = probability;
            }
        }
        Margin {
            class: Difficulty::ALL[winner],
            gap: largest - second,
        }
    }
}

/// The slack a distribution's probabilities may show when summed: enough for float
/// addition of five terms by wide margin (1e-12 would do, a hundredfold), and still
/// small enough that a missing or duplicated class — which moves the sum fractions of a
/// point — never passes under it.
const SUM_TOLERANCE: f64 = 1e-6;

/// The gap that decides: top minus second, and the class it was measured for.
///
/// The pair travels together because a margin without its class cannot be routed, and
/// the class without its margin cannot be doubted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margin {
    class: Difficulty,
    gap: f64,
}

impl Margin {
    /// The class the top probability names.
    pub fn class(self) -> Difficulty {
        self.class
    }

    /// How far the leader stands over the runner-up.
    pub fn gap(self) -> f64 {
        self.gap
    }
}

/// A distribution and the rule for acting on it, kept apart from the gate call so a
/// configuration could hold both without either call site inventing an order.
#[derive(Debug, Clone, Copy)]
pub struct Classifier {
    temperature: Temperature,
    threshold: Confidence,
}

impl Classifier {
    /// Binds a temperature — the artefact's fitted one, from its manifest — to the
    /// operator's threshold.
    pub fn new(temperature: Temperature, threshold: Confidence) -> Self {
        Self {
            temperature,
            threshold,
        }
    }

    /// The gate: a distribution of logits in, [the class to dispatch at or
    /// `None`](Gate::Refer). Softened at the temperature this was built with, then held
    /// to the threshold — the boundary inclusive, exactly the margin of your confidence
    /// passes.
    pub fn gate(&self, logits: &[f64]) -> Result<Gate, ClassifyError> {
        Ok(self.gate_distribution(&Distribution::softmax(logits, self.temperature)?))
    }

    /// The same gate one step later, fed probabilities already — when the softmax ran
    /// elsewhere and the numbers must be seen exactly as this side would have made them.
    pub fn gate_distribution(&self, distribution: &Distribution) -> Gate {
        let margin = distribution.margin();
        if margin.gap() >= self.threshold.value() {
            Gate::Dispatch(margin.class())
        } else {
            Gate::Refer
        }
    }
}

/// The gate's answer, and the ruling it stands for: nothing here is ever a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Confident enough: dispatch at this class.
    Dispatch(Difficulty),
    /// Below the threshold, or tied. The whole design rests on an unconfident answer
    /// being handed on rather than acted on — the caller finds someone else to judge,
    /// and this crate has no most-likely-class fallback to fall back to.
    Refer,
}

/// Refuse what no comparison can decide — which is every non-number, an infinity
/// included: it would divide a whole distribution into zeroes and call a uniform spread
/// a margin of nothing.
fn is_finite_number(value: f64) -> bool {
    value.is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A threshold with no opinion left in it, for tests about everything else.
    fn threshold(value: f64) -> Confidence {
        Confidence::new(value).expect("a threshold within the unit")
    }

    fn temperature(value: f64) -> Temperature {
        Temperature::new(value).expect("a positive temperature")
    }

    #[test]
    fn the_five_slots_are_the_order_of_the_classes() {
        // The alignment is the contract. If `Difficulty::ALL` is ever reordered, this
        // fails here — every distribution below silently mis-routes alongside it.
        assert_eq!(
            Difficulty::ALL,
            [
                Difficulty::Easy,
                Difficulty::Routine,
                Difficulty::Median,
                Difficulty::Hard,
                Difficulty::Research,
            ]
        );
    }

    #[test]
    fn a_clear_winner_gates_at_its_own_class() {
        let distribution = Distribution::new(&[0.05, 0.05, 0.80, 0.05, 0.05]).unwrap();
        assert_eq!(distribution.margin().class(), Difficulty::Median);
        assert!((distribution.margin().gap() - 0.75).abs() < 1e-12);
    }

    #[test]
    fn two_near_equal_leaders_do_not_gate() {
        let distribution = Distribution::new(&[0.05, 0.45, 0.44, 0.05, 0.01]).unwrap();
        assert!((distribution.margin().gap() - 0.01).abs() < 1e-12);
        let gate =
            Classifier::new(temperature(1.0), threshold(0.25)).gate_distribution(&distribution);
        assert_eq!(gate, Gate::Refer);
    }

    #[test]
    fn the_margin_is_the_top_two_gap_not_the_maximum() {
        // A high maximum with a close runner-up is the ambiguous case the margin exists
        // to see; reading the maximum alone would wave it through.
        let distribution = Distribution::new(&[0.02, 0.02, 0.90, 0.01, 0.05]).unwrap();
        assert!((distribution.margin().gap() - 0.85).abs() < 1e-12);
        let close =
            Classifier::new(temperature(1.0), threshold(0.87)).gate_distribution(&distribution);
        assert_eq!(close, Gate::Refer);
        // The same 0.90 maximum with nobody behind it is a different matter entirely.
        let clear = Distribution::new(&[0.025, 0.025, 0.90, 0.025, 0.025]).unwrap();
        let gate = Classifier::new(temperature(1.0), threshold(0.87)).gate_distribution(&clear);
        assert_eq!(gate, Gate::Dispatch(Difficulty::Median));
    }

    #[test]
    fn a_hot_temperature_softens_and_a_cold_one_sharpens() {
        let logits = [0.0, 0.0, 2.0, 0.0, 0.0];
        // Softened at one, the leader stands clear of threshold 0.5.
        let calm = Distribution::softmax(&logits, temperature(1.0)).unwrap();
        let gate = Classifier::new(temperature(1.0), threshold(0.5)).gate_distribution(&calm);
        assert_eq!(gate, Gate::Dispatch(Difficulty::Median));
        // Divided by ten the logits are nearly equal and so is their spread: the same
        // classifier's call changes. This is what calibration measures and what the
        // temperature is for.
        let softened = Distribution::softmax(&logits, temperature(10.0)).unwrap();
        let gate = Classifier::new(temperature(10.0), threshold(0.5)).gate_distribution(&softened);
        assert_eq!(gate, Gate::Refer);
        // Multiplied — the temperature below one sharpens the same spread.
        let sharp = Distribution::softmax(&logits, temperature(0.5)).unwrap();
        assert!(sharp.margin().gap() > calm.margin().gap());
    }

    #[test]
    fn a_temperature_is_positive_or_it_is_nothing() {
        assert_eq!(
            Temperature::new(0.0),
            Err(ClassifyError::TemperatureNotPositive { value: 0.0 })
        );
        assert_eq!(
            Temperature::new(-1.0),
            Err(ClassifyError::TemperatureNotPositive { value: -1.0 })
        );
    }

    #[test]
    fn a_threshold_is_a_probability_or_it_is_refused() {
        assert_eq!(
            Confidence::new(1.5),
            Err(ClassifyError::ThresholdOutOfRange { value: 1.5 })
        );
        // A threshold of one passes only a certainty; zero passes everything, the
        // operator's way of saying never ask the arbiter.
        assert!(Confidence::new(1.0).is_ok());
        assert!(Confidence::new(0.0).is_ok());
    }

    #[test]
    fn not_a_number_refuses_eagerly_at_every_door() {
        assert_eq!(
            Distribution::new(&[0.2, 0.2, f64::NAN, 0.4, 0.0]),
            Err(ClassifyError::NotANumber)
        );
        assert_eq!(Temperature::new(f64::NAN), Err(ClassifyError::NotANumber));
        assert_eq!(Confidence::new(f64::NAN), Err(ClassifyError::NotANumber));
    }

    #[test]
    fn probabilities_are_per_class() {
        // Five numbers or nothing — a shorter vector is not a class missing but a call
        // with a different model behind it, which is worth its own error.
        assert_eq!(
            Distribution::new(&[0.5, 0.5, 0.0]),
            Err(ClassifyError::WrongWidth { got: 3 })
        );
        // A probability past one is outside arithmetic this crate trusts.
        assert_eq!(
            Distribution::new(&[0.0, 0.0, 1.5, 0.0, -0.5]),
            Err(ClassifyError::OutOfRange { value: 1.5 })
        );
    }

    #[test]
    fn every_negative_logit_softens_to_a_real_distribution() {
        // The case the shift exists for: at this temperature the largest of these
        // logits divides to exp(-900), below the smallest subnormal — unshifted, every
        // exponential underflows to zero, the sum is zero, and the gate reads the NaN
        // it leaves as no winner at all, where "no winner" used to mean a panic.
        let distribution =
            Distribution::softmax(&[-100.0, -100.0, -90.0, -100.0, -100.0], temperature(0.1))
                .unwrap();
        let total: f64 = distribution.0.iter().sum();
        assert!(
            (total - 1.0).abs() < 1e-12,
            "a distribution that sums to one"
        );
        // The leader still leads: the softmax did not flatten the answer away.
        assert_eq!(distribution.margin().class(), Difficulty::Median);
        assert!(distribution.margin().gap() > 0.98);
    }

    #[test]
    fn an_infinity_is_refused_wherever_one_appears() {
        // The silent decalibration: an infinite temperature softened every distribution
        // into uniform — a gate that refers everything and raises nothing.
        assert_eq!(
            Temperature::new(f64::INFINITY),
            Err(ClassifyError::NotANumber)
        );
        assert_eq!(
            Temperature::new(f64::NEG_INFINITY),
            Err(ClassifyError::NotANumber)
        );
        assert_eq!(
            Confidence::new(f64::INFINITY),
            Err(ClassifyError::NotANumber)
        );
        assert_eq!(
            Distribution::new(&[f64::INFINITY, 0.0, 0.0, 0.0, 0.0]),
            Err(ClassifyError::NotANumber)
        );
    }

    #[test]
    fn a_distribution_must_still_be_one() {
        // A hand-fed export that forgot a class, or read it twice, is refused rather
        // than routed at: this is the door the gate's trust rests on.
        assert_eq!(
            Distribution::new(&[0.1, 0.1, 0.1, 0.1, 0.1]),
            Err(ClassifyError::DoesNotSum { sum: 0.5 })
        );
        // And rounding a real distribution to the last bits still passes.
        assert!(Distribution::new(&[0.2, 0.2, 0.2, 0.2, 0.2]).is_ok());
        assert!(
            Distribution::new(&[0.333_333, 0.333_333, 0.111_111, 0.111_111, 0.111_112]).is_ok()
        );
    }

    #[test]
    fn a_softmax_softens_to_probabilities_and_never_overflows() {
        let probabilities =
            Distribution::softmax(&[-1.0, 0.0, 1.0, 0.0, 0.0], temperature(1.0)).unwrap();
        let total: f64 = probabilities.0.iter().sum();
        assert!((total - 1.0).abs() < 1e-12, "a softmax sums to one");
        // A property of the shift, at any magnitude of finite logits and any
        // temperature: the largest exponential is exactly one, and every other is
        // smaller, so no exponential can overflow however large the logits are.
        let enormous =
            Distribution::softmax(&[-1e300, -1e300, -1e299, 0.0, 0.0], temperature(0.25)).unwrap();
        let total: f64 = enormous.0.iter().sum();
        assert!(
            (total - 1.0).abs() < 1e-12,
            "finite logits soften, never underflow"
        );
    }

    #[test]
    fn the_threshold_boundary_falls_inside_the_gate() {
        let distribution = Distribution::new(&[0.10, 0.10, 0.50, 0.15, 0.15]).unwrap();
        let gap = distribution.margin().gap();
        assert!((gap - 0.35).abs() < 1e-12);
        // Exactly at the threshold passes; a hair below it does not. Stated in the doc
        // comment on `gate`: inclusive at the boundary.
        let at = Classifier::new(temperature(1.0), Confidence::new(gap).unwrap())
            .gate_distribution(&distribution);
        assert_eq!(at, Gate::Dispatch(Difficulty::Median));
        let under = Classifier::new(temperature(1.0), Confidence::new(gap + 1e-9).unwrap())
            .gate_distribution(&distribution);
        assert_eq!(under, Gate::Refer);
    }

    #[test]
    fn a_tie_refuses_rather_than_choosing_by_slot() {
        // Equal top two: the margin is zero, and whichever class array order happened to
        // list first is the one a naive maximum-read would have routed. It does not.
        let distribution = Distribution::new(&[0.34, 0.33, 0.33, 0.0, 0.0]).unwrap();
        let gate =
            Classifier::new(temperature(1.0), threshold(0.02)).gate_distribution(&distribution);
        assert_eq!(gate, Gate::Refer);
    }

    #[test]
    fn the_gate_itself_never_guesses() {
        // Below the threshold the answer is `Refer` with no class attached; there is no
        // most-likely fallback for a caller to reach by accident.
        let gate =
            Classifier::new(temperature(1.0), threshold(0.9)).gate(&[0.0, 1.0, 0.5, 0.0, 0.0]);
        assert_eq!(gate.unwrap(), Gate::Refer);
    }
}
