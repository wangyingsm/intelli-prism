//! One node's work, in the shape it crosses a wire in.

use ip_agents::{AgentId, ExtraPrompt};
use ip_core::{TenantId, UserId};
use ip_graph::{GraphId, GraphOwner, GraphRunId, NodeName};
use ip_run::Requested;

/// One node of one graph run, as the coordinator hands it to another member.
///
/// It carries the request's *intent* and nothing the coordinator resolved. Everything a run needs
/// besides this — the template, the tools, the gateway, the prices, the caller's grants — the
/// member reads from the database and the plane both machines already share, which is why a node
/// can run anywhere without the coordinator shipping it anything live. `ip_run::Toolshed` holds
/// open MCP sessions and so could not cross a wire even if we wanted it to.
///
/// **There is no `Authority` here, and that is the point.** An authority is built from a caller's
/// standing and the grants held for them, and a member that took the coordinator's word for those
/// would widen what a caller holds to whatever the coordinator claimed. So the dispatch names the
/// caller — [`Dispatch::user`] and [`Dispatch::tenant`] — and the member builds the authority
/// itself, against the same grants. A member that cannot read those grants refuses the dispatch
/// rather than running it, which is the same refusal the coordinator would have made.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dispatch {
    /// Which graph run this node belongs to.
    pub run: GraphRunId,
    /// Which node of it, by the name its graph calls it.
    pub node: NodeName,
    /// Which graph the node is in.
    pub graph: GraphId,
    /// Whose graph it is, which is what says whether the caller may reach it at all.
    pub owner: GraphOwner,
    /// Whose run it is.
    pub tenant: TenantId,
    /// Who asked for it, so the member can read their grants for itself.
    pub user: UserId,
    /// The template the node runs.
    pub agent: AgentId,
    /// What the request that started the run asked for, where it asked for anything.
    pub requested: Requested,
    /// What the graph built for this node: its parents' answers, keyed by their names.
    pub input: String,
    /// What the caller added to this node's prompt, where the run was started with anything.
    ///
    /// Carried rather than derived, which is the opposite of the decision the worker's own
    /// `told_what_the_graph_says` records, and for the reason that decision gives: what a node may
    /// *ask for* is the graph's to declare, so a member reads it from the plane and takes nobody's
    /// word for it. This is not a graph fact. It is the caller's own words, exactly as
    /// [`Dispatch::input`] is, and a member has no other way to learn them. A coordinator that
    /// could lie about this could already lie about the input.
    ///
    /// `None` for every node something feeds, because only a node nothing feeds is given what the
    /// caller said — the coordinator decides that and the member does not revisit it.
    pub told: Option<ExtraPrompt>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::stub_dispatch;

    /// Every field, since what a wire loses it loses one field at a time.
    #[test]
    fn a_dispatch_survives_being_written_down_and_read_back() {
        let work = Dispatch {
            told: Some(ExtraPrompt::new("The table has a checksum column.").expect("a prompt")),
            ..stub_dispatch()
        };
        let written = serde_json::to_string(&work).expect("a dispatch writes down");

        assert_eq!(
            serde_json::from_str::<Dispatch>(&written).expect("a dispatch reads back"),
            work
        );
    }

    /// What a member must not be able to be told. The absence is the security property, so it is
    /// asserted rather than left to a reader of the struct.
    #[test]
    fn a_dispatch_carries_the_caller_and_never_what_the_caller_may_do() {
        let written = serde_json::to_value(stub_dispatch()).expect("a dispatch writes down");
        let fields = written.as_object().expect("a dispatch is an object");

        assert_eq!(fields["user"], "dana");
        assert_eq!(fields["tenant"], "acme");
        for widening in ["authority", "grants", "capabilities", "standing", "kind"] {
            assert!(
                !fields.contains_key(widening),
                "a dispatch carried {widening}, which a member must read for itself"
            );
        }
    }

    /// A node that was given nothing by its parents is the first node of a graph, not an error.
    #[test]
    fn a_first_node_is_dispatched_with_whatever_the_run_was_started_on() {
        let work = Dispatch {
            input: String::new(),
            requested: Requested::default(),
            ..stub_dispatch()
        };
        let written = serde_json::to_string(&work).expect("a dispatch writes down");

        assert_eq!(
            serde_json::from_str::<Dispatch>(&written).expect("a dispatch reads back"),
            work
        );
    }
}
