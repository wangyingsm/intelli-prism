//! What a member says about a node it is running for somebody else.

use ip_graph::NodeRan;
use ip_run::RunEvent;

/// One thing a member has to say about a node it was dispatched.
///
/// The three are the whole of the vocabulary, and a stream ends on one of the last two: a member
/// either answers or says it would not start. A stream that ends on [`NodeEvent::Said`] is a
/// member that stopped talking, which is [`crate::ClusterError::CutOff`] and not an answer.
///
/// [`NodeEvent::Said`] carries `ip-run`'s own [`RunEvent`] rather than a wire copy of it, so a
/// node that ran elsewhere streams exactly what a node that ran here streams and the coordinator
/// has one path for both. That is what the derives on `RunEvent` are for; `RunEvent::body` is
/// written for a reader and loses what a reader does not need, so it cannot be read back.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeEvent {
    /// Something the node said on its way, as its own run said it.
    Said(RunEvent),
    /// What the node came to. The last thing a member says about a node that ran.
    Answered(NodeRan),
    /// The member would not run the node at all, and why.
    ///
    /// A refusal rather than a status, because by the time a member knows — a template it cannot
    /// resolve, a caller whose grants it cannot read — it has already answered the request and
    /// opened the stream. It is also the one failure a coordinator may *act* on: nothing has been
    /// spent, so the node can still be run at home.
    Refused(String),
}

impl NodeEvent {
    /// The name this is streamed under.
    ///
    /// For a person watching the stream. A reader needs only the data line, which carries the
    /// whole event including this name as its tag.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Said(_) => "said",
            Self::Answered(_) => "answered",
            Self::Refused(_) => "refused",
        }
    }

    /// Whether a member has nothing further to say after this.
    #[must_use]
    pub const fn terminal(&self) -> bool {
        matches!(self, Self::Answered(_) | Self::Refused(_))
    }
}

#[cfg(test)]
mod tests {
    use ip_run::{Spend, Stopped, TurnNumber};

    use super::*;
    use crate::testing::{stub_answer, stub_stream};

    /// Every variant, since what a wire loses it loses one variant at a time.
    #[test]
    fn every_event_survives_being_written_down_and_read_back() {
        let mut events = stub_stream();
        events.push(NodeEvent::Refused("no template named summarise".to_owned()));

        for event in events {
            let written = serde_json::to_string(&event).expect("an event writes down");
            assert_eq!(
                serde_json::from_str::<NodeEvent>(&written).expect("an event reads back"),
                event,
                "{written} did not come back as what it went out as"
            );
        }
    }

    /// One vocabulary, not two: the json tag is the word the event is streamed under.
    #[test]
    fn an_events_json_tag_is_the_name_it_streams_under() {
        for event in stub_stream() {
            let written: serde_json::Value =
                serde_json::to_value(&event).expect("an event writes down");
            let tag = written
                .as_object()
                .and_then(|written| written.keys().next().cloned())
                .expect("an externally tagged event is an object of one key");
            assert_eq!(tag, event.name());
        }
    }

    /// What a stream may end on, which is what tells an answer from a member that went quiet.
    #[test]
    fn only_an_answer_or_a_refusal_ends_a_stream() {
        assert!(
            !NodeEvent::Said(RunEvent::Text {
                text: String::new()
            })
            .terminal()
        );
        assert!(NodeEvent::Answered(stub_answer()).terminal());
        assert!(NodeEvent::Refused(String::new()).terminal());
    }

    /// The whole of a run's own vocabulary reaches a coordinator, not the part `body` keeps.
    #[test]
    fn what_a_node_said_keeps_the_turn_a_readers_stream_throws_away() {
        let event = NodeEvent::Said(RunEvent::Done {
            text: String::new(),
            stopped: Stopped::OutOfTurns {
                after: TurnNumber::FIRST.next(),
            },
            spend: Spend::NOTHING,
        });
        let written = serde_json::to_string(&event).expect("an event writes down");

        assert_eq!(
            serde_json::from_str::<NodeEvent>(&written).expect("an event reads back"),
            event
        );
    }
}
