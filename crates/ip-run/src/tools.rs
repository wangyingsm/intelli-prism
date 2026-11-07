//! Which tools a run may call, and what the model is told they are called.

use std::collections::{BTreeMap, BTreeSet};

use ip_agents::ToolSet;
use ip_mcp::{McpId, McpToolName, Offering};
use ip_protocol::{Tool, ToolName};

use crate::error::RunError;

/// The longest name a model may be told, which every provider caps at the same width.
const WIRE_MAX_BYTES: usize = 64;

/// What separates a server from its tool in the name a model is told.
///
/// Not the dot a template writes: the providers all spell a function name
/// `[A-Za-z0-9_-]{1,64}` and a dot is outside it, so a name carrying one is refused by the
/// upstream rather than by anything here.
const WIRE_JOIN: &str = "__";

/// Where one tool actually lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addressed {
    /// The server that offers it.
    pub server: McpId,
    /// What that server calls it.
    pub tool: McpToolName,
}

/// Everything a run may call, by the name the model will call it under.
///
/// The names the model is told are not the names a template writes, so the way back is a map
/// built once here rather than a parse of whatever the model sends: a model that answers with
/// something close to a name it was given is then simply not found, instead of being resolved
/// to the wrong server by a split in the wrong place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Toolbox {
    by_wire: BTreeMap<String, Addressed>,
    declared: Vec<Tool>,
}

/// Every server a tool set names, each once however many of its tools are asked for.
///
/// Read before anything is opened, so a template naming a server this deployment does not
/// have is refused without a connection being made.
pub fn servers_named(set: &ToolSet) -> Result<BTreeSet<McpId>, RunError> {
    set.tools()
        .iter()
        .map(|asked| addressed(asked.as_str()).map(|one| one.server))
        .collect()
}

impl Toolbox {
    /// Resolves a template's tool set against what the registered servers offer.
    ///
    /// Every refusal here happens before a run's first turn, which is the point of doing it in
    /// one pass: a tool that resolves to nothing would otherwise be found only once a model
    /// asked for it, several paid turns in.
    pub fn assemble(set: &ToolSet, offered: &[Offered<'_>]) -> Result<Self, RunError> {
        let mut box_of = Self::default();
        for asked in set.tools() {
            let addressed = addressed(asked.as_str())?;
            let offering = offered
                .iter()
                .find(|one| *one.server == addressed.server)
                .ok_or_else(|| RunError::NoSuchServer {
                    tool: asked.as_str().to_owned(),
                    server: addressed.server.as_str().to_owned(),
                })?;
            let tool = found(offering, &addressed)?;
            box_of.insert(asked.as_str(), addressed, tool)?;
        }
        Ok(box_of)
    }

    /// Adds one resolved tool under the name the model will be told.
    fn insert(
        &mut self,
        asked: &str,
        addressed: Addressed,
        tool: &ip_mcp::McpTool,
    ) -> Result<(), RunError> {
        let wire = wired(&addressed);
        if wire.len() > WIRE_MAX_BYTES {
            return Err(RunError::ToolNameTooLong {
                tool: asked.to_owned(),
                wire,
                max: WIRE_MAX_BYTES,
            });
        }
        if let Some(taken) = self.by_wire.get(&wire) {
            return Err(RunError::ToolNameCollides {
                wire,
                first: format!("{}.{}", taken.server.as_str(), taken.tool.as_str()),
                second: asked.to_owned(),
            });
        }
        self.declared.push(Tool {
            name: ToolName::new(&wire).map_err(|error| RunError::NotAToolAddress {
                tool: asked.to_owned(),
                why: error.to_string(),
            })?,
            description: tool.description.clone(),
            schema: tool.schema.clone(),
        });
        self.by_wire.insert(wire, addressed);
        Ok(())
    }

    /// Every tool, as the model is told about them.
    #[must_use]
    pub fn declared(&self) -> &[Tool] {
        &self.declared
    }

    /// Where the tool the model just asked for actually lives.
    #[must_use]
    pub fn find(&self, called: &ToolName) -> Option<&Addressed> {
        self.by_wire.get(called.as_str())
    }

    /// Whether a run has no tools at all, which makes it a conversation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_wire.is_empty()
    }
}

/// One registered server and what it answered `tools/list` with.
#[derive(Debug, Clone, Copy)]
pub struct Offered<'a> {
    /// Which server.
    pub server: &'a McpId,
    /// What it offers.
    pub offering: &'a Offering,
}

/// Splits a template's `server.tool` into the two halves it names.
///
/// On the first dot, so a tool whose own name holds one still resolves; a server id holding one
/// cannot be addressed, which is the trade and is why the halves are checked by their own types.
fn addressed(asked: &str) -> Result<Addressed, RunError> {
    let halves = |why: &dyn std::fmt::Display| RunError::NotAToolAddress {
        tool: asked.to_owned(),
        why: why.to_string(),
    };
    let (server, tool) = asked
        .split_once('.')
        .ok_or_else(|| RunError::UnqualifiedTool {
            tool: asked.to_owned(),
        })?;
    Ok(Addressed {
        server: McpId::new(server).map_err(|error| halves(&error))?,
        tool: McpToolName::new(tool).map_err(|error| halves(&error))?,
    })
}

/// The tool a server offers under that name, or that it offers none.
///
/// A tool the offering dropped is not looked for here and needs no word of its own: it was
/// dropped because its name is one [`McpToolName`] refuses, and a template that reached this
/// far is holding a name that type accepted, so the two can never be the same name.
fn found<'a>(
    offered: &Offered<'a>,
    addressed: &Addressed,
) -> Result<&'a ip_mcp::McpTool, RunError> {
    offered
        .offering
        .tools
        .iter()
        .find(|one| one.name == addressed.tool)
        .ok_or_else(|| RunError::NoSuchTool {
            server: addressed.server.as_str().to_owned(),
            tool: addressed.tool.as_str().to_owned(),
        })
}

/// The name a model is told for one addressed tool.
///
/// Nothing is replaced on the way: a server id may hold a dot, but the split is on the first
/// one, so the half that reaches here never does — and what is left of either half is already
/// the set every provider allows. Two different pairs can still meet in the middle, as
/// `a` and `_b` do with `a_` and `b`, which is why the result is checked for having been taken.
fn wired(addressed: &Addressed) -> String {
    format!(
        "{}{WIRE_JOIN}{}",
        addressed.server.as_str(),
        addressed.tool.as_str()
    )
}

#[cfg(test)]
mod tests {
    use ip_agents::ToolName as Asked;
    use ip_mcp::{DroppedTool, McpError, McpTool};

    use super::*;

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: McpToolName::new(name).unwrap(),
            description: Some(format!("what {name} does")),
            schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn offering(names: &[&str]) -> Offering {
        Offering {
            tools: names.iter().map(|name| tool(name)).collect(),
            dropped: Vec::new(),
        }
    }

    fn set(names: &[&str]) -> ToolSet {
        ToolSet::new(names.iter().map(|n| Asked::new(n).unwrap()).collect()).unwrap()
    }

    fn id(raw: &str) -> McpId {
        McpId::new(raw).unwrap()
    }

    fn assembled(asked: &[&str], server: &McpId, offering: &Offering) -> Toolbox {
        Toolbox::assemble(&set(asked), &[Offered { server, offering }]).unwrap()
    }

    fn refused(asked: &[&str], server: &McpId, offering: &Offering) -> RunError {
        Toolbox::assemble(&set(asked), &[Offered { server, offering }]).unwrap_err()
    }

    #[test]
    fn a_tool_is_declared_under_a_name_every_provider_takes() {
        let warehouse = id("warehouse");
        let box_of = assembled(
            &["warehouse.get_stock"],
            &warehouse,
            &offering(&["get_stock", "ping"]),
        );

        assert_eq!(box_of.declared().len(), 1, "only what was asked for");
        let declared = &box_of.declared()[0];
        assert_eq!(declared.name.as_str(), "warehouse__get_stock");
        assert_eq!(declared.description.as_deref(), Some("what get_stock does"));
        assert_eq!(
            declared.schema,
            serde_json::json!({"type": "object", "properties": {}})
        );
        assert!(!box_of.is_empty());
    }

    /// A map rather than a parse, so a name close to one the model was given is simply absent.
    #[test]
    fn the_name_the_model_calls_leads_back_to_the_server_that_offers_it() {
        let warehouse = id("warehouse");
        let box_of = assembled(
            &["warehouse.get_stock"],
            &warehouse,
            &offering(&["get_stock"]),
        );

        assert_eq!(
            box_of.find(&ToolName::new("warehouse__get_stock").unwrap()),
            Some(&Addressed {
                server: id("warehouse"),
                tool: McpToolName::new("get_stock").unwrap(),
            })
        );
        for missed in ["warehouse.get_stock", "get_stock", "warehouse__getstock"] {
            assert_eq!(
                box_of.find(&ToolName::new(missed).unwrap()),
                None,
                "{missed:?} found something"
            );
        }
    }

    #[test]
    fn a_run_may_ask_for_no_tools_at_all() {
        let box_of = Toolbox::assemble(&ToolSet::default(), &[]).unwrap();

        assert!(box_of.is_empty());
        assert_eq!(box_of.declared(), []);
        assert_eq!(box_of, Toolbox::default());
    }

    /// Two servers may both offer `sql`, which is the whole reason a template qualifies a name.
    #[test]
    fn two_servers_offering_the_same_tool_are_told_apart() {
        let warehouse = id("warehouse");
        let ledger = id("ledger");
        let box_of = Toolbox::assemble(
            &set(&["warehouse.sql", "ledger.sql"]),
            &[
                Offered {
                    server: &warehouse,
                    offering: &offering(&["sql"]),
                },
                Offered {
                    server: &ledger,
                    offering: &offering(&["sql"]),
                },
            ],
        )
        .unwrap();

        let names: Vec<&str> = box_of
            .declared()
            .iter()
            .map(|one| one.name.as_str())
            .collect();
        assert_eq!(names, ["warehouse__sql", "ledger__sql"]);
        assert_eq!(
            box_of
                .find(&ToolName::new("ledger__sql").unwrap())
                .unwrap()
                .server,
            ledger
        );
    }

    /// The failure M23's stored templates would hit, and the reason the dot is not required by
    /// the type: it reads back, and the run is what refuses.
    #[test]
    fn a_tool_naming_no_server_refuses_the_run_rather_than_the_row() {
        let stored: ToolSet = serde_json::from_str(r#"["sql","search"]"#).unwrap();

        assert_eq!(
            Toolbox::assemble(&stored, &[]),
            Err(RunError::UnqualifiedTool {
                tool: "sql".to_owned()
            })
        );
    }

    #[test]
    fn a_tool_on_a_server_nobody_registered_refuses_the_run() {
        let warehouse = id("warehouse");

        assert_eq!(
            refused(&["ledger.sql"], &warehouse, &offering(&["sql"])),
            RunError::NoSuchServer {
                tool: "ledger.sql".to_owned(),
                server: "ledger".to_owned(),
            }
        );
    }

    #[test]
    fn a_tool_a_registered_server_does_not_offer_refuses_the_run() {
        let warehouse = id("warehouse");

        assert_eq!(
            refused(&["warehouse.get_stock"], &warehouse, &offering(&["ping"])),
            RunError::NoSuchTool {
                server: "warehouse".to_owned(),
                tool: "get_stock".to_owned(),
            }
        );
    }

    /// A dropped tool needs no word of its own: it was dropped for carrying a name the
    /// template's own type would have refused, so no template can be asking for *it*.
    #[test]
    fn a_server_whose_only_tool_was_dropped_offers_nothing() {
        let warehouse = id("warehouse");
        let offering = Offering {
            tools: Vec::new(),
            dropped: vec![DroppedTool {
                name: "get stock".to_owned(),
                why: McpError::IllegalChar {
                    kind: McpToolName::KIND,
                    ch: ' ',
                },
            }],
        };

        // The template cannot even spell it, so the dropped name is reached through one it can.
        assert_eq!(
            refused(&["warehouse.get_stock"], &warehouse, &offering),
            RunError::NoSuchTool {
                server: "warehouse".to_owned(),
                tool: "get_stock".to_owned(),
            }
        );
    }

    #[test]
    fn a_qualified_name_past_what_a_model_may_be_told_refuses_the_run() {
        let long = id(&"w".repeat(40));
        let asked = format!("{}.{}", long.as_str(), "t".repeat(40));

        let refused = refused(&[&asked], &long, &offering(&["t".repeat(40).as_str()]));
        assert!(
            matches!(refused, RunError::ToolNameTooLong { max, .. } if max == WIRE_MAX_BYTES),
            "{refused:?}"
        );
    }

    /// Two different pairs that meet in the middle: `a` + `_b` and `a_` + `b` are both legal
    /// and both become `a___b`. Refused rather than resolved to whichever was seen first.
    #[test]
    fn two_tools_a_model_could_not_tell_apart_refuse_the_run() {
        let plain = id("a");
        let scored = id("a_");
        let refused = Toolbox::assemble(
            &set(&["a._b", "a_.b"]),
            &[
                Offered {
                    server: &plain,
                    offering: &offering(&["_b"]),
                },
                Offered {
                    server: &scored,
                    offering: &offering(&["b"]),
                },
            ],
        )
        .unwrap_err();

        assert_eq!(
            refused,
            RunError::ToolNameCollides {
                wire: "a___b".to_owned(),
                first: "a._b".to_owned(),
                second: "a_.b".to_owned(),
            }
        );
    }

    /// The split is on the first dot, so a server id holding one cannot be addressed at all:
    /// the rest falls on the tool side, where its own type refuses it.
    #[test]
    fn a_server_whose_id_holds_a_dot_cannot_be_addressed() {
        let dotted = id("wh.eu");

        let refused = Toolbox::assemble(
            &set(&["wh.eu.sql"]),
            &[Offered {
                server: &dotted,
                offering: &offering(&["sql"]),
            }],
        );
        assert!(
            matches!(
                refused,
                Err(RunError::NotAToolAddress { ref tool, ref why })
                    if tool == "wh.eu.sql" && why.contains('.')
            ),
            "{refused:?}"
        );
    }
}
