//! What must hold for every graph, not for the ones somebody drew.
//!
//! The unit tests beside the code drive `ready_after` over a chain, a fan-out, a fan-in and a
//! diamond. These say what must be true of all of them: a node is never ready before one of its
//! parents is done, however the graph is shaped. That is the one correctness condition the whole
//! milestone rests on — run a node early and it reads an answer that does not exist yet — and no
//! finite set of hand-drawn graphs stands in for it.
//!
//! Everything here goes through the public api, so a property that holds here holds for a caller.

use std::collections::BTreeSet;

use ip_graph::{Generation, NodeName, Placed, RunOrder};
use proptest::prelude::*;

/// At most eight nodes. The property is about the shape of the rule rather than about size, and
/// every shape that can go wrong — a chain, a fan, a diamond, two components at once — already
/// fits in eight.
const MOST: usize = 8;

fn node_name(index: usize) -> NodeName {
    NodeName::new(&format!("n{index}")).expect("a name built out of a number")
}

/// Reads back the index [`node_name`] built a name from.
fn node_index(name: &NodeName) -> usize {
    name.as_str()
        .trim_start_matches('n')
        .parse()
        .expect("a name this file built")
}

/// A DAG over `count` nodes, with the generations a toposort would answer for it.
///
/// `edges[child * MOST + parent]`, read only where `parent < child`, makes `parent` feed `child`.
/// An edge therefore always points from a lower index to a higher one, so the draw cannot produce
/// a cycle and every graph it produces is one the plane would accept. The generation is then the
/// definition rather than a guess: one past the latest parent, zero for a node with none.
fn build(count: usize, edges: &[bool]) -> (RunOrder, Vec<Vec<usize>>) {
    let mut parents = vec![Vec::new(); count];
    let mut generation = vec![0_u32; count];
    for child in 0..count {
        for parent in 0..child {
            if edges[child * MOST + parent] {
                parents[child].push(parent);
                generation[child] = generation[child].max(generation[parent] + 1);
            }
        }
    }
    let placed = (0..count)
        .map(|index| Placed {
            name: node_name(index),
            generation: Generation::new(generation[index]),
        })
        .collect();
    (
        RunOrder::checked(placed).expect("an order placing each node once"),
        parents,
    )
}

proptest! {
    /// The milestone's one invariant: whatever has finished, nothing that is handed back as ready
    /// is waiting on an answer that has not been produced.
    #[test]
    fn a_node_is_never_ready_before_every_one_of_its_parents_is_done(
        count in 1_usize..=MOST,
        edges in prop::collection::vec(any::<bool>(), MOST * MOST),
        finished in prop::collection::vec(any::<bool>(), MOST),
    ) {
        let (order, parents) = build(count, &edges);
        let done: BTreeSet<NodeName> = (0..count)
            .filter(|&index| finished[index])
            .map(node_name)
            .collect();
        for ready in order.ready_after(&done) {
            prop_assert!(!done.contains(ready), "{ready} was ready after finishing");
            for &parent in &parents[node_index(ready)] {
                prop_assert!(
                    done.contains(&node_name(parent)),
                    "{ready} was ready while its parent n{parent} had not finished",
                );
            }
        }
    }

    /// Driven the way a run will drive it — take what is ready, finish it, ask again — the order
    /// reaches every node, and in no more rounds than it has nodes. A rule that refused to hand
    /// anything back would satisfy the property above and hang a graph; this is what rules it out.
    #[test]
    fn running_what_is_ready_until_nothing_is_reaches_every_node(
        count in 1_usize..=MOST,
        edges in prop::collection::vec(any::<bool>(), MOST * MOST),
    ) {
        let (order, _) = build(count, &edges);
        let mut done = BTreeSet::new();
        let mut rounds = 0_usize;
        loop {
            let ready: Vec<NodeName> = order.ready_after(&done).into_iter().cloned().collect();
            if ready.is_empty() {
                break;
            }
            rounds += 1;
            prop_assert!(rounds <= count, "a graph of {count} nodes took {rounds} rounds");
            done.extend(ready);
        }
        prop_assert_eq!(done.len(), count, "a node was never reached");
    }

    /// Everything handed back at once sits in one generation, which is what lets a runner spawn
    /// the whole answer into a `JoinSet` and wait for all of it.
    #[test]
    fn everything_ready_at_once_belongs_to_one_generation(
        count in 1_usize..=MOST,
        edges in prop::collection::vec(any::<bool>(), MOST * MOST),
        finished in prop::collection::vec(any::<bool>(), MOST),
    ) {
        let (order, _) = build(count, &edges);
        let done: BTreeSet<NodeName> = (0..count)
            .filter(|&index| finished[index])
            .map(node_name)
            .collect();
        let generations: BTreeSet<Generation> = order
            .ready_after(&done)
            .into_iter()
            .map(|name| {
                order
                    .placed()
                    .iter()
                    .find(|placed| &placed.name == name)
                    .expect("a ready node is one the order places")
                    .generation
            })
            .collect();
        prop_assert!(generations.len() <= 1, "{generations:?} were ready at once");
    }
}
