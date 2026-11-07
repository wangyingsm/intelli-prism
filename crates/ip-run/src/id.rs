//! What one run of an agent is followed by.

use std::fmt;

/// Bytes a run id carries, the same width a trace id uses.
const RUN_BYTES: usize = 16;

/// The id one run is followed under.
///
/// Drawn here and never taken from a caller, for [`ip_core::TraceId`]'s reason: an id a stranger
/// chooses is one a stranger can collide with or follow. It is not a trace id and does not
/// replace one — a run makes a request of its own per turn, each with its own trace, and this is
/// what ties those together.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId([u8; RUN_BYTES]);

impl RunId {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "run id";

    /// Draws a new run id from the operating system entropy source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; RUN_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// The hex form, which is what an event and a log carry.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RunId").field(&self.to_hex()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_id_is_its_hex_wherever_it_is_printed() {
        let run = RunId::generate().unwrap();
        assert_eq!(run.to_hex().len(), RUN_BYTES * 2);
        assert_eq!(run.to_string(), run.to_hex());
        assert_eq!(format!("{run:?}"), format!("RunId({:?})", run.to_hex()));
    }

    #[test]
    fn two_runs_are_two_ids() {
        assert_ne!(RunId::generate().unwrap(), RunId::generate().unwrap());
    }
}
