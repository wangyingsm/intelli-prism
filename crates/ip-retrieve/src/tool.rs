use std::collections::BTreeSet;

use crate::error::RetrieveError;

/// One thing a retriever can be asked.
///
/// Five, chosen rather than every verb a backend has: each is its own tool with its own schema,
/// so a model is never asked to fill in a parameter that applies to one of them and not another.
/// A graph's `toposort` and `cycles` are deliberately absent — they answer whether a graph is a
/// DAG, which is the run engine's own business and no use to something looking a fact up.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RetrieverTool {
    /// What is nearest this text.
    Search,
    /// What is joined to this hit.
    Neighbors,
    /// What matters most here, by how much is joined to it.
    Rank,
    /// How these two are connected.
    Relate,
    /// What groups these things fall into.
    Cluster,
}

impl RetrieverTool {
    /// The one word this is spelled by, in an address and in an error.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Neighbors => "neighbors",
            Self::Rank => "rank",
            Self::Relate => "relate",
            Self::Cluster => "cluster",
        }
    }

    /// Whether answering it means walking edges, which only a graph has.
    ///
    /// This is what tells the two backends apart: a vector store answers what is nearest and
    /// nothing else, so a collection declaring one of these is a misconfiguration rather than a
    /// call that fails later.
    #[must_use]
    pub const fn needs_edges(self) -> bool {
        !matches!(self, Self::Search)
    }
}

/// Every tool a retriever offers, each held once however often it was named.
///
/// Empty is refused, which is the opposite of a template's tool set: an agent with no tools is a
/// conversation and a legitimate thing to configure, while a retriever with none could never be
/// asked anything.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Vec<RetrieverTool>")]
pub struct RetrieverTools(BTreeSet<RetrieverTool>);

impl RetrieverTools {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "retriever tools";

    /// Validates and wraps a list of tools.
    pub fn new(offered: Vec<RetrieverTool>) -> Result<Self, RetrieveError> {
        Self::try_from(offered)
    }

    /// Every tool offered, in a settled order.
    pub fn offered(&self) -> impl Iterator<Item = RetrieverTool> + '_ {
        self.0.iter().copied()
    }

    /// Whether this one is offered.
    #[must_use]
    pub fn offers(&self, tool: RetrieverTool) -> bool {
        self.0.contains(&tool)
    }

    /// How many are offered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none is, which [`RetrieverTools::new`] refuses and nothing else can produce.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<Vec<RetrieverTool>> for RetrieverTools {
    type Error = RetrieveError;

    fn try_from(offered: Vec<RetrieverTool>) -> Result<Self, Self::Error> {
        let held: BTreeSet<RetrieverTool> = offered.into_iter().collect();
        if held.is_empty() {
            return Err(RetrieveError::OffersNothing);
        }
        Ok(Self(held))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_named_twice_is_held_once() {
        let tools = RetrieverTools::new(vec![
            RetrieverTool::Search,
            RetrieverTool::Search,
            RetrieverTool::Rank,
        ])
        .expect("two of one and one of another is two tools");
        assert_eq!(tools.len(), 2);
        assert!(tools.offers(RetrieverTool::Rank));
        assert!(!tools.offers(RetrieverTool::Cluster));
    }

    #[test]
    fn a_retriever_offering_nothing_is_refused() {
        assert_eq!(
            RetrieverTools::new(Vec::new()),
            Err(RetrieveError::OffersNothing),
            "nothing could ever be asked of it"
        );
    }

    #[test]
    fn only_a_search_is_answerable_without_edges() {
        assert!(!RetrieverTool::Search.needs_edges());
        for tool in [
            RetrieverTool::Neighbors,
            RetrieverTool::Rank,
            RetrieverTool::Relate,
            RetrieverTool::Cluster,
        ] {
            assert!(tool.needs_edges(), "{} walks edges", tool.name());
        }
    }

    #[test]
    fn a_tool_is_spelled_the_same_in_an_address_and_in_json() {
        let written = serde_json::to_string(&RetrieverTool::Neighbors).expect("an enum writes");
        assert_eq!(written, "\"neighbors\"");
        assert_eq!(
            serde_json::from_str::<RetrieverTool>(&written).expect("and reads back"),
            RetrieverTool::Neighbors
        );
    }
}
