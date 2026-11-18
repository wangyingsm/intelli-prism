use std::fmt;

use crate::error::RetrieveError;
use crate::spelling::key;

/// The longest an id may be, in bytes.
const ID_MAX_BYTES: usize = 64;

/// Which retriever this is, as a tenant spells it and a tool address names it.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct RetrieverId(Box<str>);

impl RetrieverId {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "retriever id";

    /// Validates and wraps an id.
    pub fn new(raw: &str) -> Result<Self, RetrieveError> {
        Self::try_from(raw.to_owned())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RetrieverId {
    type Error = RetrieveError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        key(&raw, Self::KIND, ID_MAX_BYTES)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

impl fmt::Display for RetrieverId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_spelled_as_every_other_id_in_this_system_is() {
        assert_eq!(
            RetrieverId::new("handbook")
                .expect("a plain word is an id")
                .as_str(),
            "handbook"
        );
        assert!(
            RetrieverId::new("hand book").is_err(),
            "a space is not an id"
        );
        assert!(RetrieverId::new("").is_err(), "nothing is not an id");
        assert!(
            RetrieverId::new(&"a".repeat(ID_MAX_BYTES + 1)).is_err(),
            "an id is bounded, because it is half of a tool address"
        );
    }

    #[test]
    fn an_id_that_is_not_one_is_refused_on_the_way_in() {
        let refused = serde_json::from_str::<RetrieverId>("\"hand book\"");
        assert!(refused.is_err(), "deserializing checks what `new` checks");
    }
}
