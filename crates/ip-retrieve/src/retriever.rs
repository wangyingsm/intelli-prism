use std::fmt;
use std::num::NonZeroU32;

use zeroize::Zeroizing;

use crate::error::RetrieveError;
use crate::id::RetrieverId;
use crate::meta::MetaSchema;
use crate::source::Source;
use crate::spelling::prose;
use crate::tool::RetrieverTools;

/// The longest a name may be, in bytes.
const NAME_MAX_BYTES: usize = 64;

/// The longest an endpoint url may be, in bytes.
const ENDPOINT_MAX_BYTES: usize = 1024;

/// The longest a key may be, in bytes.
const KEY_MAX_BYTES: usize = 4096;

/// The most hits one look-up may answer with.
const HITS_MAX: u32 = 50;

/// How many hits a retriever that does not say answers with.
const HITS_DEFAULT: u32 = 5;

/// What a retriever is called, for somebody reading a list of them.
///
/// A label rather than a key: it may carry spaces and any language a person writes in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct RetrieverName(Box<str>);

impl RetrieverName {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "retriever name";

    /// Validates and wraps a name.
    pub fn new(raw: &str) -> Result<Self, RetrieveError> {
        Self::try_from(raw.to_owned())
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RetrieverName {
    type Error = RetrieveError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        prose(&raw, Self::KIND, NAME_MAX_BYTES)?;
        Ok(Self(raw.into_boxed_str()))
    }
}

impl fmt::Display for RetrieverName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a retriever's backend answers: an `http` or `https` url with a host.
///
/// Checked for shape and for nothing else. Nothing is contacted here, the path is whatever the
/// operator wrote, and a url that parses is not a url that answers — what is really there is
/// asked of it when the retriever is registered.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct RetrieverEndpoint(Box<str>);

impl RetrieverEndpoint {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "retriever endpoint";

    /// Validates and wraps an endpoint url.
    pub fn new(raw: &str) -> Result<Self, RetrieveError> {
        Self::try_from(raw.to_owned())
    }

    /// The url as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether it is reached over tls.
    #[must_use]
    pub fn is_secure(&self) -> bool {
        self.0.starts_with("https://")
    }
}

impl TryFrom<String> for RetrieverEndpoint {
    type Error = RetrieveError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.len() > ENDPOINT_MAX_BYTES {
            return Err(RetrieveError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: ENDPOINT_MAX_BYTES,
            });
        }
        let host = match raw.split_once("://") {
            Some(("http" | "https", rest)) => rest,
            _ => return Err(RetrieveError::NotAnEndpoint { value: raw }),
        };
        // A host, not merely a scheme and a path: `http:///search` parses as a url in some
        // readings and names nothing this gateway can call.
        let named = host.split(['/', '?', '#']).next().unwrap_or_default();
        if named.is_empty() || named.chars().any(char::is_whitespace) {
            return Err(RetrieveError::NotAnEndpoint { value: raw });
        }
        Ok(Self(raw.into_boxed_str()))
    }
}

impl fmt::Display for RetrieverEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A key a backend requires.
///
/// **It is a credential, and this type is shaped so it cannot leak by accident.** It does not
/// implement `Serialize`, so no view and no json body can carry it without somebody writing the
/// code to do so on purpose; its `Debug` prints nothing of the value; and it is zeroized when
/// dropped. Reading it takes [`RetrieverKey::expose`], which is deliberately awkward to type.
#[derive(Clone, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct RetrieverKey(Zeroizing<String>);

impl RetrieverKey {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "retriever key";

    /// Validates and wraps a key.
    pub fn new(raw: &str) -> Result<Self, RetrieveError> {
        Self::try_from(raw.to_owned())
    }

    /// The key itself, for presenting to the backend it belongs to.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RetrieverKey {
    type Error = RetrieveError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(RetrieveError::Empty { kind: Self::KIND });
        }
        if raw.len() > KEY_MAX_BYTES {
            return Err(RetrieveError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: KEY_MAX_BYTES,
            });
        }
        // A header is what it will be presented in, and a header holds no control character.
        match raw.chars().find(|ch| ch.is_control()) {
            Some(ch) => Err(RetrieveError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(Zeroizing::new(raw))),
        }
    }
}

impl fmt::Debug for RetrieverKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RetrieverKey(…)")
    }
}

/// Equality over the value, for comparing a key read back against the one written.
///
/// Not an authentication check: nothing here is presented to this gateway, so there is no secret
/// of ours to compare in constant time.
impl PartialEq for RetrieverKey {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl Eq for RetrieverKey {}

/// What a backend requires before it will answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetrieverAuth {
    /// Nothing: the endpoint is reached as it stands.
    #[default]
    None,
    /// A key, presented in the header the backend asks for it in — `api-key` for `qdrant`, a
    /// bearer `Authorization` for `drsg`.
    Key {
        /// The key.
        key: RetrieverKey,
    },
}

impl RetrieverAuth {
    /// The name this kind is stored and reported under.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Key { .. } => "key",
        }
    }

    /// The key, where there is one.
    #[must_use]
    pub const fn key(&self) -> Option<&RetrieverKey> {
        match self {
            Self::None => None,
            Self::Key { key } => Some(key),
        }
    }
}

/// The most hits one look-up may answer with.
///
/// A ceiling the operator sets rather than the model: every hit lands in a prompt, so a model
/// that asks for a hundred is asking this deployment to pay for a hundred. What the model asks
/// for is clamped to this, never refused for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "u32")]
pub struct MaxHits(NonZeroU32);

impl MaxHits {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "max hits";

    /// Validates and wraps a ceiling.
    pub fn new(raw: u32) -> Result<Self, RetrieveError> {
        Self::try_from(raw)
    }

    /// The ceiling as a number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl Default for MaxHits {
    /// Enough to answer from, and few enough for a model to read.
    fn default() -> Self {
        Self(NonZeroU32::new(HITS_DEFAULT).expect("the default is not zero"))
    }
}

impl TryFrom<u32> for MaxHits {
    type Error = RetrieveError;

    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        match NonZeroU32::new(raw) {
            Some(hits) if raw <= HITS_MAX => Ok(Self(hits)),
            _ => Err(RetrieveError::OutOfRange {
                kind: Self::KIND,
                value: raw,
                max: HITS_MAX,
            }),
        }
    }
}

/// Everything a retriever is made of, before anything has checked that it holds together.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrieverParts {
    /// Which retriever this is.
    pub id: RetrieverId,
    /// What it is called.
    pub name: RetrieverName,
    /// Where its backend answers.
    pub endpoint: RetrieverEndpoint,
    /// What that backend requires; nothing when left out.
    #[serde(default)]
    pub auth: RetrieverAuth,
    /// Where it looks, which is also what kind it is.
    pub source: Source,
    /// Which tools it offers.
    pub tools: RetrieverTools,
    /// What a look-up may filter on, and nothing when it may not.
    #[serde(default)]
    pub meta: MetaSchema,
    /// The most hits one look-up answers with.
    #[serde(default)]
    pub max_hits: MaxHits,
}

/// What one retriever is, with every part of it already refused or accepted.
///
/// It does not implement `Serialize`, because [`RetrieverKey`] does not: a retriever is written
/// to storage field by field, and what the api reports is built on purpose rather than derived.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "RetrieverParts")]
pub struct Retriever(RetrieverParts);

impl Retriever {
    /// Assembles a retriever, refusing the parts that contradict each other.
    pub fn new(parts: RetrieverParts) -> Result<Self, RetrieveError> {
        Self::try_from(parts)
    }

    /// Which retriever this is.
    #[must_use]
    pub const fn id(&self) -> &RetrieverId {
        &self.0.id
    }

    /// What it is called.
    #[must_use]
    pub const fn name(&self) -> &RetrieverName {
        &self.0.name
    }

    /// Where its backend answers.
    #[must_use]
    pub const fn endpoint(&self) -> &RetrieverEndpoint {
        &self.0.endpoint
    }

    /// What that backend requires.
    #[must_use]
    pub const fn auth(&self) -> &RetrieverAuth {
        &self.0.auth
    }

    /// Where it looks.
    #[must_use]
    pub const fn source(&self) -> &Source {
        &self.0.source
    }

    /// Which tools it offers.
    #[must_use]
    pub const fn tools(&self) -> &RetrieverTools {
        &self.0.tools
    }

    /// What a look-up may filter on.
    #[must_use]
    pub const fn meta(&self) -> &MetaSchema {
        &self.0.meta
    }

    /// The most hits one look-up answers with.
    #[must_use]
    pub const fn max_hits(&self) -> MaxHits {
        self.0.max_hits
    }

    /// What this retriever would answer for a look-up asking for `asked`.
    ///
    /// The clamp rather than a refusal, for the reason [`MaxHits`] records: a model asking for
    /// more than the operator allows is answered with what the operator allows.
    #[must_use]
    pub fn hits_for(&self, asked: Option<u32>) -> u32 {
        let ceiling = self.0.max_hits.get();
        asked.map_or(ceiling, |asked| asked.clamp(1, ceiling))
    }
}

impl TryFrom<RetrieverParts> for Retriever {
    type Error = RetrieveError;

    fn try_from(parts: RetrieverParts) -> Result<Self, Self::Error> {
        // The one contradiction a tagged source cannot make unrepresentable: a collection has no
        // edges, so four of the five tools are a misconfiguration on one rather than a call that
        // would fail a model's turn later.
        if let Some(refused) = parts
            .tools
            .offered()
            .find(|tool| !parts.source.answers(*tool))
        {
            return Err(RetrieveError::NotForThisSource {
                backend: parts.source.name(),
                tool: refused.name(),
            });
        }
        Ok(Self(parts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{CollectionName, PlaneName, VectorProperty};
    use crate::tool::RetrieverTool;

    /// The parts of a retriever over a plane, offering `tools`.
    fn over_a_plane(tools: Vec<RetrieverTool>) -> RetrieverParts {
        RetrieverParts {
            id: RetrieverId::new("handbook").expect("an id"),
            name: RetrieverName::new("The handbook").expect("a name"),
            endpoint: RetrieverEndpoint::new("http://127.0.0.1:44453").expect("an endpoint"),
            auth: RetrieverAuth::None,
            source: Source::Drsg {
                plane: PlaneName::new("handbook").expect("a plane name"),
                label: None,
                property: VectorProperty::new("embedding").expect("a property"),
            },
            tools: RetrieverTools::new(tools).expect("at least one tool"),
            meta: MetaSchema::default(),
            max_hits: MaxHits::default(),
        }
    }

    /// The same, over a collection.
    fn over_a_collection(tools: Vec<RetrieverTool>) -> RetrieverParts {
        RetrieverParts {
            source: Source::Qdrant {
                collection: CollectionName::new("handbook").expect("a collection name"),
                vector: None,
            },
            ..over_a_plane(tools)
        }
    }

    #[test]
    fn a_plane_may_offer_every_tool() {
        let retriever = Retriever::new(over_a_plane(vec![
            RetrieverTool::Search,
            RetrieverTool::Neighbors,
            RetrieverTool::Rank,
            RetrieverTool::Relate,
            RetrieverTool::Cluster,
        ]))
        .expect("a graph answers all five");
        assert_eq!(retriever.tools().len(), 5);
        assert_eq!(retriever.source().name(), "drsg");
    }

    #[test]
    fn a_collection_offering_a_graph_tool_is_refused() {
        assert_eq!(
            Retriever::new(over_a_collection(vec![
                RetrieverTool::Search,
                RetrieverTool::Cluster
            ])),
            Err(RetrieveError::NotForThisSource {
                backend: "qdrant",
                tool: "cluster"
            }),
            "a collection has no edges, so this is a misconfiguration and not a failed call"
        );
        assert!(
            Retriever::new(over_a_collection(vec![RetrieverTool::Search])).is_ok(),
            "a search is all a collection answers, and it is enough"
        );
    }

    #[test]
    fn what_a_model_asks_for_is_clamped_to_what_the_operator_allows() {
        let parts = RetrieverParts {
            max_hits: MaxHits::new(3).expect("three is a ceiling"),
            ..over_a_plane(vec![RetrieverTool::Search])
        };
        let retriever = Retriever::new(parts).expect("a plane with a low ceiling");
        assert_eq!(
            retriever.hits_for(None),
            3,
            "nothing asked takes the ceiling"
        );
        assert_eq!(
            retriever.hits_for(Some(2)),
            2,
            "less than the ceiling stands"
        );
        assert_eq!(
            retriever.hits_for(Some(900)),
            3,
            "more is clamped, not refused"
        );
        assert_eq!(retriever.hits_for(Some(0)), 1, "none at all is one");
    }

    #[test]
    fn a_ceiling_outside_its_range_is_refused() {
        assert_eq!(
            MaxHits::new(0),
            Err(RetrieveError::OutOfRange {
                kind: MaxHits::KIND,
                value: 0,
                max: HITS_MAX
            }),
            "a look-up answering nothing is not a look-up"
        );
        assert!(
            MaxHits::new(HITS_MAX).is_ok(),
            "the ceiling itself is allowed"
        );
        assert!(
            MaxHits::new(HITS_MAX + 1).is_err(),
            "every hit lands in a prompt somebody pays for"
        );
        assert_eq!(MaxHits::default().get(), HITS_DEFAULT);
    }

    #[test]
    fn a_key_is_shaped_so_it_cannot_leak_by_accident() {
        let auth: RetrieverAuth =
            serde_json::from_str(r#"{"kind": "key", "key": "qdrant-api-key"}"#)
                .expect("a key is a credential");
        let held = auth.key().expect("the kind says there is one");
        assert_eq!(held.expose(), "qdrant-api-key");
        assert_eq!(
            format!("{held:?}"),
            "RetrieverKey(…)",
            "debug prints nothing of the value"
        );
        assert_eq!(auth.name(), "key");
        assert!(
            RetrieverAuth::default().key().is_none(),
            "a backend that says nothing requires nothing"
        );
    }

    #[test]
    fn an_endpoint_needs_a_scheme_this_gateway_speaks_and_a_host() {
        assert!(RetrieverEndpoint::new("https://qdrant.internal:6333").is_ok());
        assert!(
            RetrieverEndpoint::new("https://qdrant.internal")
                .expect("tls")
                .is_secure()
        );
        assert!(
            !RetrieverEndpoint::new("http://127.0.0.1:6333")
                .expect("plain")
                .is_secure()
        );
        for refused in ["qdrant.internal:6333", "ftp://qdrant", "http:///search", ""] {
            assert!(
                RetrieverEndpoint::new(refused).is_err(),
                "{refused:?} names nothing this gateway can call"
            );
        }
    }

    #[test]
    fn a_whole_retriever_reads_from_the_json_an_operator_writes() {
        let retriever: Retriever = serde_json::from_str(
            r#"{
                "id": "handbook",
                "name": "The handbook",
                "endpoint": "https://qdrant.internal:6333",
                "auth": {"kind": "key", "key": "k"},
                "source": {"kind": "qdrant", "collection": "handbook"},
                "tools": ["search"],
                "meta": {"team": "Which team the page belongs to"},
                "max_hits": 8
            }"#,
        )
        .expect("every part of it is a part this type takes");
        assert_eq!(retriever.id().as_str(), "handbook");
        assert_eq!(retriever.max_hits().get(), 8);
        assert_eq!(retriever.meta().len(), 1);
        assert!(retriever.tools().offers(RetrieverTool::Search));
    }

    #[test]
    fn a_retriever_leaves_out_what_it_need_not_say() {
        let retriever: Retriever = serde_json::from_str(
            r#"{
                "id": "handbook",
                "name": "The handbook",
                "endpoint": "https://qdrant.internal:6333",
                "source": {"kind": "qdrant", "collection": "handbook"},
                "tools": ["search"]
            }"#,
        )
        .expect("an auth, a schema and a ceiling all have defaults");
        assert_eq!(retriever.auth(), &RetrieverAuth::None);
        assert!(retriever.meta().is_empty());
        assert_eq!(retriever.max_hits().get(), HITS_DEFAULT);
    }

    #[test]
    fn a_field_no_retriever_has_is_refused_rather_than_ignored() {
        assert!(
            serde_json::from_str::<Retriever>(
                r#"{
                    "id": "handbook",
                    "name": "The handbook",
                    "endpoint": "https://qdrant.internal:6333",
                    "source": {"kind": "qdrant", "collection": "handbook"},
                    "tools": ["search"],
                    "nearness": 0.9
                }"#
            )
            .is_err(),
            "a misspelled field is a refusal, as it is everywhere else in this system"
        );
    }
}
