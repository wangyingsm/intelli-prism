use std::collections::BTreeMap;
use std::fmt;

use crate::error::RetrieveError;
use crate::spelling::{key, prose};

/// The longest a field name may be, in bytes.
const FIELD_MAX_BYTES: usize = 64;

/// The longest a note about a field may be, in bytes.
const NOTE_MAX_BYTES: usize = 160;

/// How many fields one schema may describe.
///
/// Bounded for the reason a condition on a branch edge is: every one of these is appended to the
/// prompt of a node that may search, and a retriever may not spend a model's context on the
/// shape of its own payload.
const FIELDS_MAX: usize = 32;

/// A payload field a look-up may filter on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct MetaField(Box<str>);

impl MetaField {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "meta field";

    /// Validates and wraps a field name.
    pub fn new(raw: &str) -> Result<Self, RetrieveError> {
        Self::try_from(raw.to_owned())
    }

    /// The field name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for MetaField {
    type Error = RetrieveError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        key(&raw, Self::KIND, FIELD_MAX_BYTES)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

impl fmt::Display for MetaField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one field means, in the words a model will be shown.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct MetaNote(Box<str>);

impl MetaNote {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "meta note";

    /// Validates and wraps a note.
    pub fn new(raw: &str) -> Result<Self, RetrieveError> {
        Self::try_from(raw.to_owned())
    }

    /// The note as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for MetaNote {
    type Error = RetrieveError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        prose(&raw, Self::KIND, NOTE_MAX_BYTES)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

impl fmt::Display for MetaNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a look-up may filter on, in the words the model doing the filtering is shown.
///
/// This is the half of a retriever an operator writes *for the model* rather than for the
/// backend. A payload field is useless to a model that has not been told it exists, and a model
/// told only the name guesses at what goes in it, so each field carries a line saying what it
/// holds. Empty is allowed, and means a model may search but not filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "BTreeMap<MetaField, MetaNote>")]
pub struct MetaSchema(BTreeMap<MetaField, MetaNote>);

impl MetaSchema {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "meta schema";

    /// Validates and wraps a schema.
    pub fn new(described: BTreeMap<MetaField, MetaNote>) -> Result<Self, RetrieveError> {
        Self::try_from(described)
    }

    /// Every field and what it holds, in a settled order.
    pub fn described(&self) -> impl Iterator<Item = (&MetaField, &MetaNote)> {
        self.0.iter()
    }

    /// How many fields are described.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none is, which means a model may search but not filter.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<BTreeMap<MetaField, MetaNote>> for MetaSchema {
    type Error = RetrieveError;

    fn try_from(described: BTreeMap<MetaField, MetaNote>) -> Result<Self, Self::Error> {
        if described.len() > FIELDS_MAX {
            return Err(RetrieveError::TooMany {
                kind: Self::KIND,
                len: described.len(),
                max: FIELDS_MAX,
            });
        }
        Ok(Self(described))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schema of `count` fields, each described, as json.
    fn written(count: usize) -> String {
        let fields: Vec<String> = (0..count)
            .map(|which| format!("\"field_{which}\": \"what it holds\""))
            .collect();
        format!("{{{}}}", fields.join(", "))
    }

    #[test]
    fn a_schema_says_what_each_field_holds() {
        let schema: MetaSchema = serde_json::from_str(
            r#"{"quarter": "Which quarter the figure is for, as 2026Q1", "team": "Which team"}"#,
        )
        .expect("a field and a line about it");
        assert_eq!(schema.len(), 2);
        let (field, note) = schema.described().next().expect("a schema holds two");
        assert_eq!(
            field.as_str(),
            "quarter",
            "settled order, not written order"
        );
        assert!(note.as_str().starts_with("Which quarter"));
    }

    #[test]
    fn a_schema_describing_nothing_is_a_retriever_that_cannot_be_filtered() {
        let schema = MetaSchema::default();
        assert!(schema.is_empty(), "empty is allowed, unlike a tool list");
        assert_eq!(schema.described().count(), 0);
    }

    #[test]
    fn a_schema_wider_than_a_prompt_should_carry_is_refused() {
        assert!(
            serde_json::from_str::<MetaSchema>(&written(FIELDS_MAX)).is_ok(),
            "the ceiling itself is allowed"
        );
        let refused = serde_json::from_str::<MetaSchema>(&written(FIELDS_MAX + 1));
        assert!(
            refused.is_err(),
            "every field is appended to a prompt, so the count is bounded"
        );
    }

    #[test]
    fn a_field_is_a_key_and_a_note_is_one_line() {
        assert!(MetaField::new("created_at").is_ok());
        assert!(
            MetaField::new("created at").is_err(),
            "a field name is a payload key, not prose"
        );
        assert!(MetaNote::new("Any language a person writes in: 季度").is_ok());
        assert!(
            MetaNote::new(&"n".repeat(NOTE_MAX_BYTES + 1)).is_err(),
            "a note is a line, not a paragraph"
        );
        assert_eq!(
            MetaNote::new("two\nlines"),
            Err(RetrieveError::IllegalChar {
                kind: MetaNote::KIND,
                ch: '\n'
            }),
            "a newline would break the prompt it is appended to"
        );
    }
}
