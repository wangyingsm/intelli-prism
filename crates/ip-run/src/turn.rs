//! What one turn of a run did, and why the run stopped.

use std::fmt;
use std::iter::Sum;
use std::ops::Add;

use ip_core::{ModelName, Money, TokenCount, Tokens};
use ip_protocol::{Block, ToolCallId, ToolName};

use crate::error::RunError;

/// Which turn of a run this is, counting from one.
///
/// A number rather than an index because every place it appears — a limit, an event, a message
/// to a person — counts turns the way a person does.
///
/// Counting from one is the whole of the type, so reading one back off a wire refuses a zero.
/// Nothing in this process can build one — there is no constructor but [`TurnNumber::FIRST`] and
/// [`TurnNumber::next`] — and a json form that could would be a second way in with a weaker rule.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "u32", into = "u32")]
pub struct TurnNumber(u32);

impl TurnNumber {
    /// The turn a run opens with.
    pub const FIRST: Self = Self(1);

    /// The turn after this one.
    ///
    /// Saturating, because a run that reached four billion turns has long since been stopped by
    /// its own ceiling and a wrap here would restart the count rather than end it.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// How many turns have been taken once this one is done.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for TurnNumber {
    type Error = RunError;

    fn try_from(number: u32) -> Result<Self, Self::Error> {
        if number == 0 {
            return Err(RunError::NotATurnNumber { number });
        }
        Ok(Self(number))
    }
}

impl From<TurnNumber> for u32 {
    fn from(number: TurnNumber) -> Self {
        number.0
    }
}

impl fmt::Display for TurnNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a run has spent: tokens in each direction, and what they cost.
///
/// A run is many requests, so this is what the turns add up to rather than what one of them
/// spent. The cost is what the gateway priced each turn at, which is nothing for a turn whose
/// model carries no price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Spend {
    /// Tokens, in each direction.
    pub tokens: Tokens,
    /// What they cost.
    pub cost: Money,
}

impl Spend {
    /// A run that has spent nothing yet.
    pub const NOTHING: Self = Self {
        tokens: Tokens::ZERO,
        cost: Money::ZERO,
    };

    /// What one turn spent.
    #[must_use]
    pub const fn new(tokens: Tokens, cost: Money) -> Self {
        Self { tokens, cost }
    }
}

impl Add for Spend {
    type Output = Self;

    /// Saturating in both directions, for the reason [`TurnNumber::next`] is: a total that
    /// wrapped would report a run as having spent almost nothing.
    fn add(self, other: Self) -> Self {
        Self {
            tokens: Tokens::new(
                TokenCount::new(
                    self.tokens
                        .input
                        .get()
                        .saturating_add(other.tokens.input.get()),
                ),
                TokenCount::new(
                    self.tokens
                        .output
                        .get()
                        .saturating_add(other.tokens.output.get()),
                ),
            ),
            cost: self.cost + other.cost,
        }
    }
}

impl Sum for Spend {
    fn sum<I: Iterator<Item = Self>>(spent: I) -> Self {
        spent.fold(Self::NOTHING, Self::add)
    }
}

/// What one turn of a run produced.
///
/// The blocks are the model's answer as the canonical conversation holds it, rather than text
/// and calls pulled apart into fields: the loop appends them to the conversation unchanged, and
/// a turn that both said something and asked for a tool is one answer and not two.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// Which turn this was.
    pub number: TurnNumber,
    /// The model that answered it, where the family it answered in named one.
    pub model: Option<ModelName>,
    /// What it answered with.
    pub blocks: Vec<Block>,
    /// What it spent.
    pub spend: Spend,
}

impl Turn {
    /// Everything the model said this turn, in order.
    #[must_use]
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<&str>>()
            .join("")
    }

    /// Every tool the model asked for this turn, in the order it asked.
    pub fn calls(&self) -> impl Iterator<Item = (&ToolCallId, &ToolName, &serde_json::Value)> {
        self.blocks.iter().filter_map(|block| match block {
            Block::ToolCall {
                id,
                name,
                arguments,
            } => Some((id, name, arguments)),
            _ => None,
        })
    }

    /// Whether the model asked for anything to be run, which is what sends the loop round again.
    #[must_use]
    pub fn asks_for_a_tool(&self) -> bool {
        self.calls().next().is_some()
    }
}

/// Why a run ended.
///
/// Its json tag is the word [`Stopped::name`] already streams, so the two spell one vocabulary
/// rather than two: what a reader sees in an event is what a wire carries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stopped {
    /// The model answered and asked for nothing further, which is a run going as intended.
    Answered,
    /// It was still asking for tools when the template's ceiling was reached.
    OutOfTurns {
        /// The turn it was stopped after.
        after: TurnNumber,
    },
    /// The run outlasted what the template allows it.
    OutOfTime,
    /// Something refused it partway: an upstream, a tool, or a limit.
    ///
    /// The reason is text because it comes from whichever crate refused — the gateway, the
    /// protocol, an MCP server — and a run reports to a person rather than being matched on.
    Refused {
        /// What refused it, and why.
        why: String,
    },
    /// Whoever started it said stop, and it stopped before taking another turn.
    ///
    /// Its own: a run cut short was going fine, and reporting it as [`Stopped::Refused`] would
    /// read as a run that failed. In a graph that is the difference between the node that broke
    /// and the siblings that were stopped because it did.
    CutShort,
}

impl Stopped {
    /// How this reads on the wire, as one word.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::OutOfTurns { .. } => "out_of_turns",
            Self::OutOfTime => "out_of_time",
            Self::Refused { .. } => "refused",
            Self::CutShort => "cut_short",
        }
    }
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Answered => f.write_str("the model answered"),
            Self::OutOfTurns { after } => write!(f, "still asking for tools after {after} turns"),
            Self::OutOfTime => f.write_str("the run ran out of time"),
            Self::Refused { why } => write!(f, "the run was refused: {why}"),
            Self::CutShort => f.write_str("the run was stopped before its next turn"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spent(input: u32, output: u32, cost: u64) -> Spend {
        Spend::new(
            Tokens::new(TokenCount::new(input), TokenCount::new(output)),
            Money::from_micros(cost),
        )
    }

    fn turn(blocks: Vec<Block>) -> Turn {
        Turn {
            number: TurnNumber::FIRST,
            model: Some(ModelName::new("gpt-4o").unwrap()),
            blocks,
            spend: Spend::NOTHING,
        }
    }

    fn call(id: &str, name: &str) -> Block {
        Block::ToolCall {
            id: ToolCallId::new(id).unwrap(),
            name: ToolName::new(name).unwrap(),
            arguments: serde_json::json!({"sku": "A-1"}),
        }
    }

    #[test]
    fn turns_are_counted_from_one_the_way_a_person_counts_them() {
        assert_eq!(TurnNumber::FIRST.get(), 1);
        assert_eq!(TurnNumber::FIRST.next().get(), 2);
        assert_eq!(TurnNumber::FIRST.to_string(), "1");
        assert!(TurnNumber::FIRST < TurnNumber::FIRST.next());
    }

    /// A wrap here would restart the count rather than end it.
    #[test]
    fn a_turn_count_that_cannot_grow_stays_where_it_is() {
        let last = (0..3).fold(TurnNumber(u32::MAX - 1), |turn, _| turn.next());
        assert_eq!(last.get(), u32::MAX);
    }

    /// A plain number on the wire, and the count still starts where turns start.
    #[test]
    fn a_turn_number_is_written_down_as_its_number_and_read_back() {
        let second = TurnNumber::FIRST.next();

        assert_eq!(serde_json::to_string(&second).unwrap(), "2");
        assert_eq!(serde_json::from_str::<TurnNumber>("2").unwrap(), second);
    }

    /// Nothing in this process can build a turn zero, so nothing off a wire may either.
    #[test]
    fn a_turn_numbered_zero_is_refused() {
        assert_eq!(
            TurnNumber::try_from(0),
            Err(RunError::NotATurnNumber { number: 0 })
        );
        assert!(serde_json::from_str::<TurnNumber>("0").is_err());
        assert!(serde_json::from_str::<TurnNumber>("-1").is_err());
    }

    /// The json tag and the streamed name are one word, for every reason a run can end.
    #[test]
    fn every_reason_a_run_ended_survives_being_written_down_under_its_own_name() {
        for stopped in [
            Stopped::Answered,
            Stopped::OutOfTurns {
                after: TurnNumber::FIRST.next(),
            },
            Stopped::OutOfTime,
            Stopped::Refused {
                why: "the gateway said no".to_owned(),
            },
            Stopped::CutShort,
        ] {
            let written = serde_json::to_value(&stopped).unwrap();
            let tag = match &written {
                serde_json::Value::String(tag) => tag.clone(),
                serde_json::Value::Object(fields) => fields
                    .keys()
                    .next()
                    .cloned()
                    .expect("a tagged reason is an object of one key"),
                other => panic!("a reason was written down as {other}"),
            };

            assert_eq!(tag, stopped.name());
            assert_eq!(
                serde_json::from_value::<Stopped>(written).unwrap(),
                stopped,
                "{} did not come back as what it went out as",
                stopped.name()
            );
        }
    }

    /// The whole of what a run spent, both directions and the cost.
    #[test]
    fn what_a_run_spent_survives_being_written_down() {
        let spend = spent(120, 34, 5_600);
        let written = serde_json::to_string(&spend).unwrap();

        assert_eq!(serde_json::from_str::<Spend>(&written).unwrap(), spend);
    }

    #[test]
    fn what_a_run_spent_is_what_its_turns_spent() {
        let total: Spend = [spent(10, 20, 300), spent(1, 2, 3)].into_iter().sum();
        assert_eq!(total, spent(11, 22, 303));
        assert_eq!(Spend::NOTHING + spent(5, 0, 1), spent(5, 0, 1));
        assert_eq!(Spend::default(), Spend::NOTHING);
    }

    #[test]
    fn a_total_that_cannot_grow_stays_where_it_is() {
        let full = spent(u32::MAX, u32::MAX, 0);
        assert_eq!(full + spent(1, 1, 0), full);
    }

    #[test]
    fn what_the_model_said_is_every_text_block_in_order() {
        let turn = turn(vec![
            Block::Text("Looking".to_owned()),
            call("c1", "get_stock"),
            Block::Text(" it up.".to_owned()),
        ]);

        assert_eq!(turn.text(), "Looking it up.");
    }

    #[test]
    fn the_tools_a_turn_asks_for_are_read_in_the_order_it_asked() {
        let turn = turn(vec![
            call("c1", "get_stock"),
            Block::Text("and".to_owned()),
            call("c2", "ping"),
        ]);

        let asked: Vec<&str> = turn.calls().map(|(_, name, _)| name.as_str()).collect();
        assert_eq!(asked, ["get_stock", "ping"]);
        assert_eq!(turn.calls().next().unwrap().0.as_str(), "c1");
        assert_eq!(
            turn.calls().next().unwrap().2,
            &serde_json::json!({"sku": "A-1"})
        );
        assert!(turn.asks_for_a_tool());
    }

    #[test]
    fn a_turn_that_only_answered_asks_for_nothing() {
        let turn = turn(vec![Block::Text("41 in bay 3.".to_owned())]);
        assert!(!turn.asks_for_a_tool());
        assert_eq!(turn.calls().count(), 0);
    }

    #[test]
    fn every_way_a_run_ends_says_so_in_one_word_and_in_a_sentence() {
        for (stopped, name, said) in [
            (Stopped::Answered, "answered", "the model answered"),
            (
                Stopped::OutOfTurns {
                    after: TurnNumber(8),
                },
                "out_of_turns",
                "still asking for tools after 8 turns",
            ),
            (Stopped::OutOfTime, "out_of_time", "the run ran out of time"),
            (
                Stopped::Refused {
                    why: "the warehouse is closed".to_owned(),
                },
                "refused",
                "the run was refused: the warehouse is closed",
            ),
            (
                Stopped::CutShort,
                "cut_short",
                "the run was stopped before its next turn",
            ),
        ] {
            assert_eq!(stopped.name(), name);
            assert_eq!(stopped.to_string(), said);
        }
    }
}
