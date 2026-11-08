//! The order a graph's nodes run in, as the plane answered it.

use std::collections::{BTreeSet, HashSet};

use crate::error::GraphError;
use crate::id::NodeName;

/// Which round of a run a node belongs to.
///
/// Every node of one generation may run at once, because none of them is downstream of another.
/// The number comes back from the plane's own `toposort`; nothing here computes it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Generation(u32);

impl Generation {
    /// The generation a plane placed a node in.
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// The generation as a number.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// One node's place in the order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Placed {
    /// Which node.
    pub name: NodeName,
    /// Which round it runs in.
    pub generation: Generation,
}

/// The order a graph's nodes run in.
///
/// Built from what `plane.algo`'s `toposort` answered, not from a traversal of the graph: this
/// crate carries no graph algorithm, and that is the point of it carrying the result type instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOrder(Vec<Placed>);

impl RunOrder {
    /// An order, if it places each node once.
    ///
    /// The one refusal is a node placed twice, which would give it two generations to run in. The
    /// generations themselves are taken as the plane gave them.
    pub fn checked(placed: Vec<Placed>) -> Result<Self, GraphError> {
        let mut seen = HashSet::with_capacity(placed.len());
        for one in &placed {
            if !seen.insert(&one.name) {
                return Err(GraphError::DuplicatePlacement {
                    name: one.name.clone(),
                });
            }
        }
        Ok(Self(placed))
    }

    /// Every node, in the order the plane answered with.
    pub fn placed(&self) -> &[Placed] {
        &self.0
    }

    /// How many nodes the order places.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it places no node at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// What may run once `done` has finished, which is the whole of the earliest generation that
    /// is not yet through.
    ///
    /// A caller runs the answer to completion and asks again: nothing of a later generation may
    /// start, because a generation is what may run at once and the one before it must be whole.
    /// Nothing is ready once every node is done, and the answer is then empty.
    ///
    /// This is a filter over the order rather than a walk of the graph — the parents a node waits
    /// for are already expressed by it sitting in a later generation than they do.
    pub fn ready_after(&self, done: &BTreeSet<NodeName>) -> Vec<&NodeName> {
        let Some(earliest) = self
            .0
            .iter()
            .filter(|placed| !done.contains(&placed.name))
            .map(|placed| placed.generation)
            .min()
        else {
            return Vec::new();
        };
        self.0
            .iter()
            .filter(|placed| placed.generation == earliest && !done.contains(&placed.name))
            .map(|placed| &placed.name)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: &str) -> NodeName {
        NodeName::new(raw).expect("a name a node may carry")
    }

    /// An order from `(name, generation)` pairs, the way a plane's answer arrives.
    fn order(placed: &[(&str, u32)]) -> RunOrder {
        RunOrder::checked(
            placed
                .iter()
                .map(|&(raw, generation)| Placed {
                    name: name(raw),
                    generation: Generation::new(generation),
                })
                .collect(),
        )
        .expect("an order placing each node once")
    }

    fn done(names: &[&str]) -> BTreeSet<NodeName> {
        names.iter().map(|&raw| name(raw)).collect()
    }

    fn ready(order: &RunOrder, finished: &[&str]) -> Vec<String> {
        order
            .ready_after(&done(finished))
            .into_iter()
            .map(|name| name.to_string())
            .collect()
    }

    #[test]
    fn a_chain_runs_one_node_at_a_time() {
        let chain = order(&[("intake", 0), ("triage", 1), ("reply", 2)]);
        assert_eq!(ready(&chain, &[]), ["intake"]);
        assert_eq!(ready(&chain, &["intake"]), ["triage"]);
        assert_eq!(ready(&chain, &["intake", "triage"]), ["reply"]);
        assert!(ready(&chain, &["intake", "triage", "reply"]).is_empty());
    }

    #[test]
    fn a_fan_out_runs_its_whole_generation_at_once() {
        let fan = order(&[("intake", 0), ("a", 1), ("b", 1), ("c", 1)]);
        assert_eq!(ready(&fan, &[]), ["intake"]);
        assert_eq!(ready(&fan, &["intake"]), ["a", "b", "c"]);
    }

    #[test]
    fn a_fan_in_waits_for_every_one_of_its_parents() {
        let fan = order(&[("a", 0), ("b", 0), ("summary", 1)]);
        assert_eq!(ready(&fan, &[]), ["a", "b"]);
        assert_eq!(
            ready(&fan, &["a"]),
            ["b"],
            "the summary started before both answers were in"
        );
        assert_eq!(ready(&fan, &["a", "b"]), ["summary"]);
    }

    #[test]
    fn a_diamond_closes_before_its_tail_runs() {
        let diamond = order(&[("intake", 0), ("left", 1), ("right", 1), ("merge", 2)]);
        assert_eq!(ready(&diamond, &["intake"]), ["left", "right"]);
        assert!(
            ready(&diamond, &["intake", "left"]).contains(&"right".to_owned()),
            "the other arm stopped being ready"
        );
        assert_eq!(ready(&diamond, &["intake", "left"]).len(), 1);
        assert_eq!(ready(&diamond, &["intake", "left", "right"]), ["merge"]);
    }

    /// Two components with nothing between them, which a plane answers as one order: each is a
    /// root, so both start at once.
    #[test]
    fn two_graphs_in_one_both_start_at_once() {
        let two = order(&[("a", 0), ("b", 0), ("a2", 1), ("b2", 1)]);
        assert_eq!(ready(&two, &[]), ["a", "b"]);
        assert_eq!(ready(&two, &["a", "b"]), ["a2", "b2"]);
    }

    /// A node finished out of turn — which the runner never does, but which must not make a later
    /// generation ready — leaves the generation it belongs to still blocking.
    #[test]
    fn a_node_done_early_does_not_let_the_next_generation_start() {
        let chain = order(&[("a", 0), ("b", 1), ("c", 2)]);
        assert_eq!(ready(&chain, &["c"]), ["a"]);
        assert_eq!(ready(&chain, &["a", "c"]), ["b"]);
    }

    #[test]
    fn an_order_placing_one_node_twice_is_refused() {
        let twice = vec![
            Placed {
                name: name("a"),
                generation: Generation::new(0),
            },
            Placed {
                name: name("a"),
                generation: Generation::new(1),
            },
        ];
        assert_eq!(
            RunOrder::checked(twice),
            Err(GraphError::DuplicatePlacement { name: name("a") })
        );
    }

    #[test]
    fn an_order_says_how_many_nodes_it_places() {
        let one = order(&[("a", 0)]);
        assert_eq!(one.len(), 1);
        assert!(!one.is_empty());
        assert_eq!(one.placed()[0].generation, Generation::new(0));
        assert_eq!(one.placed()[0].generation.get(), 0);
        let none = RunOrder::checked(Vec::new()).expect("an order placing nothing");
        assert!(none.is_empty());
        assert!(none.ready_after(&done(&[])).is_empty());
    }
}
