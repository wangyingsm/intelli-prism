//! What one request through the gateway is followed by.

use std::fmt;
use std::str::FromStr;

use crate::error::CoreError;
use crate::key::decode_hex;

/// Bytes a trace id carries, which is the width w3c tracing uses.
const TRACE_BYTES: usize = 16;

/// The id one request is traced under.
///
/// It is always drawn here and never taken from a caller: a trace a stranger chooses is one a
/// stranger can collide with, follow or fill with noise. A caller that wants to correlate
/// reads the id back from the answer.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId([u8; TRACE_BYTES]);

impl TraceId {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "trace id";

    /// Draws a new trace id from the operating system entropy source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; TRACE_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Parses the hex form a log or a header carries.
    pub fn from_hex(raw: &str) -> Result<Self, CoreError> {
        decode_hex(Self::KIND, raw).map(Self)
    }

    /// The hex form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8; TRACE_BYTES] {
        &self.0
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TraceId").field(&self.to_hex()).finish()
    }
}

impl FromStr for TraceId {
    type Err = CoreError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::from_hex(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_id_round_trips_through_its_hex() {
        let trace = TraceId::generate().unwrap();
        assert_eq!(trace.to_hex().len(), TRACE_BYTES * 2);
        assert_eq!(TraceId::from_hex(&trace.to_hex()).unwrap(), trace);
        assert_eq!(trace.to_string(), trace.to_hex());
        assert_eq!(
            format!("{trace:?}"),
            format!("TraceId({:?})", trace.to_hex())
        );
    }

    #[test]
    fn every_trace_id_is_its_own() {
        let first = TraceId::generate().unwrap();
        let second = TraceId::generate().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.as_bytes().len(), TRACE_BYTES);
    }

    #[test]
    fn what_is_not_a_trace_id_is_refused() {
        assert!(TraceId::from_hex("not hex").is_err());
        assert!(TraceId::from_hex("abcd").is_err());
    }
}
