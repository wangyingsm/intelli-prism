//! Which tools a run may call, and what the model is told they are called.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ip_agents::ToolSet;
use ip_mcp::{McpId, McpToolName, Offering};
use ip_protocol::{Tool, ToolName};
use ip_retrieve::{Retrieval, Retriever, RetrieverId, RetrieverTool};

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
///
/// **The kind is the variant and not a field beside it**, for the reason
/// [`ip_retrieve::Source`] gives: a pair of fields saying "a server" and "a retriever's tool"
/// is a contradiction something has to remember to check, while this is a shape that cannot be
/// built. The two halves of each are the ones that kind addresses by, and nothing carries the
/// other's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addressed {
    /// A tool an MCP server offers, discovered by asking it.
    Mcp {
        /// The server that offers it.
        server: McpId,
        /// What that server calls it.
        tool: McpToolName,
    },
    /// A tool a retriever offers, which is one of the five and known without asking.
    Retriever {
        /// The retriever that answers it.
        retriever: RetrieverId,
        /// Which of the five.
        tool: RetrieverTool,
    },
}

impl Addressed {
    /// The name the caller wrote before the dot, whichever kind it named.
    fn source(&self) -> &str {
        match self {
            Self::Mcp { server, .. } => server.as_str(),
            Self::Retriever { retriever, .. } => retriever.as_str(),
        }
    }

    /// The name the caller wrote after the dot, whichever kind it named.
    fn tool(&self) -> &str {
        match self {
            Self::Mcp { tool, .. } => tool.as_str(),
            Self::Retriever { tool, .. } => tool.name(),
        }
    }
}

/// One retriever a run may address: what it is, and what looks something up in it.
///
/// The two are carried together because a run needs both and neither is derivable from the
/// other: the row says which tools are offered, what a filter may name and how many hits are
/// allowed, while the implementation is the backend it was opened against. The binary pairs
/// them, which is what keeps this crate off both backends.
#[derive(Clone)]
pub struct Held {
    /// What the operator registered.
    pub retriever: Retriever,
    /// What looks something up in it.
    pub lookup: Arc<dyn Retrieval>,
}

impl std::fmt::Debug for Held {
    /// Names the retriever rather than the client behind it, which holds a credential.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Held")
            .field("retriever", &self.retriever.id().as_str())
            .finish()
    }
}

/// What a run may address, read before anything is opened.
///
/// Both halves, because the name before the dot is looked up in each: that is what tells a
/// retriever's tool from a server's, and what makes a name held by both an ambiguity rather than
/// whichever this code happened to look in first.
#[derive(Debug, Clone, Copy)]
pub struct Registries<'a> {
    /// Every MCP server this deployment has registered.
    pub servers: &'a [McpId],
    /// Every retriever the run's owner holds.
    pub retrievers: &'a [Held],
}

impl<'a> Registries<'a> {
    /// The retriever of that id, where the owner holds one.
    ///
    /// Taken by value so what comes back outlives the copy it was read through, which is what
    /// lets one pass carry the row it resolved.
    fn retriever(self, named: &str) -> Option<&'a Held> {
        self.retrievers
            .iter()
            .find(|held| held.retriever.id().as_str() == named)
    }
}

/// What a tool set names, split by where each name lives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    /// Every MCP server named, each once however many of its tools are asked for.
    pub servers: BTreeSet<McpId>,
    /// Every retriever named, the same way.
    pub retrievers: BTreeSet<RetrieverId>,
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

/// What a tool set names, split by kind, each named thing once.
///
/// Read before anything is opened, so a template naming a server this deployment does not
/// have is refused without a connection being made.
pub fn sources_named(set: &ToolSet, held: Registries<'_>) -> Result<Sources, RunError> {
    let mut named = Sources::default();
    for asked in set.tools() {
        match resolved(asked.as_str(), held)?.addressed() {
            Addressed::Mcp { server, .. } => named.servers.insert(server),
            Addressed::Retriever { retriever, .. } => named.retrievers.insert(retriever),
        };
    }
    Ok(named)
}

impl Toolbox {
    /// Resolves a template's tool set against what the registered servers offer.
    ///
    /// Every refusal here happens before a run's first turn, which is the point of doing it in
    /// one pass: a tool that resolves to nothing would otherwise be found only once a model
    /// asked for it, several paid turns in.
    pub fn assemble(
        set: &ToolSet,
        offered: &[Offered<'_>],
        held: Registries<'_>,
    ) -> Result<Self, RunError> {
        let mut box_of = Self::default();
        for asked in set.tools() {
            let resolved = resolved(asked.as_str(), held)?;
            let declared = match &resolved {
                Resolved::Mcp { server, tool } => {
                    let offering = offered
                        .iter()
                        .find(|one| *one.server == *server)
                        .ok_or_else(|| RunError::NoSuchServer {
                            tool: asked.as_str().to_owned(),
                            server: server.as_str().to_owned(),
                        })?;
                    let found = found(offering, tool)?;
                    Declaration {
                        description: found.description.clone(),
                        schema: found.schema.clone(),
                    }
                }
                Resolved::Retriever { held, tool, .. } => {
                    let declared = held.retriever.declare(*tool);
                    Declaration {
                        description: Some(declared.description),
                        schema: declared.schema,
                    }
                }
            };
            box_of.insert(asked.as_str(), resolved.addressed(), declared)?;
        }
        Ok(box_of)
    }

    /// Adds one resolved tool under the name the model will be told.
    fn insert(
        &mut self,
        asked: &str,
        addressed: Addressed,
        declared: Declaration,
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
                first: format!("{}.{}", taken.source(), taken.tool()),
                second: asked.to_owned(),
            });
        }
        self.declared.push(Tool {
            name: ToolName::new(&wire).map_err(|error| RunError::NotAToolAddress {
                tool: asked.to_owned(),
                why: error.to_string(),
            })?,
            description: declared.description,
            schema: declared.schema,
        });
        self.by_wire.insert(wire, addressed);
        Ok(())
    }

    /// The first tool here that searches by meaning, and none where nothing does.
    ///
    /// What the up-front embedder check is made of: the four tools that walk edges compare
    /// nothing of ours, so a deployment that embeds nothing may still hold a retriever and run
    /// a template that walks one. Only a search needs the encoder.
    #[must_use]
    pub fn searching(&self) -> Option<String> {
        self.by_wire
            .iter()
            .find(|(_, addressed)| {
                matches!(
                    addressed,
                    Addressed::Retriever {
                        tool: RetrieverTool::Search,
                        ..
                    }
                )
            })
            .map(|(wire, _)| wire.clone())
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

/// One resolved tool address, with the retriever row behind a retriever's.
///
/// The row is carried out of the pass that found it rather than looked up again, so the one
/// place that reads the registries is the one place that refuses a name they do not hold.
enum Resolved<'a> {
    /// A tool addressed to an MCP server.
    Mcp {
        /// The server named.
        server: McpId,
        /// What that server calls it.
        tool: McpToolName,
    },
    /// A tool addressed to a retriever, and the retriever itself.
    Retriever {
        /// The retriever named.
        retriever: RetrieverId,
        /// Which of the five.
        tool: RetrieverTool,
        /// The row that answers it, and what looks something up in it.
        held: &'a Held,
    },
}

impl Resolved<'_> {
    /// The address alone, as a toolbox stores it.
    fn addressed(&self) -> Addressed {
        match self {
            Self::Mcp { server, tool } => Addressed::Mcp {
                server: server.clone(),
                tool: tool.clone(),
            },
            Self::Retriever {
                retriever, tool, ..
            } => Addressed::Retriever {
                retriever: retriever.clone(),
                tool: *tool,
            },
        }
    }
}

/// Splits a template's `source.tool` into the two halves it names, and says which kind of source
/// it named.
///
/// On the first dot, so a tool whose own name holds one still resolves; a source id holding one
/// cannot be addressed, which is the trade and is why the halves are checked by their own types.
///
/// **A name held by both registries is refused rather than resolved.** `kb.search` where `kb` is
/// both a registered server and a retriever has two readings, and picking either would mean a
/// template silently calling the wrong one — so it is the operator's to rename, and they are
/// told before the first turn.
///
/// A name held by neither is taken as a server, which is what it has always been: a typo then
/// reads as the server that is not registered, and that refusal has not moved.
fn resolved<'a>(asked: &str, held: Registries<'a>) -> Result<Resolved<'a>, RunError> {
    let halves = |why: &dyn std::fmt::Display| RunError::NotAToolAddress {
        tool: asked.to_owned(),
        why: why.to_string(),
    };
    let (source, tool) = asked
        .split_once('.')
        .ok_or_else(|| RunError::UnqualifiedTool {
            tool: asked.to_owned(),
        })?;
    let retriever = held.retriever(source);
    let server = held.servers.iter().any(|known| known.as_str() == source);
    match (retriever, server) {
        (Some(_), true) => Err(RunError::ToolSourceAmbiguous {
            tool: asked.to_owned(),
            named: source.to_owned(),
        }),
        (Some(one), false) => Ok(Resolved::Retriever {
            retriever: RetrieverId::new(source).map_err(|error| halves(&error))?,
            tool: named(&one.retriever, tool, asked)?,
            held: one,
        }),
        (None, _) => Ok(Resolved::Mcp {
            server: McpId::new(source).map_err(|error| halves(&error))?,
            tool: McpToolName::new(tool).map_err(|error| halves(&error))?,
        }),
    }
}

/// The one of the five a name spells, or that a retriever offers nothing under it.
///
/// A retriever's tools are a closed set known without asking, which is the whole difference from
/// a server: a word that is not one of the five and one of the five this retriever does not offer
/// are the same mistake, and the answer names what it does offer either way.
fn named(held: &Retriever, tool: &str, asked: &str) -> Result<RetrieverTool, RunError> {
    held.tools()
        .offered()
        .find(|one| one.name() == tool)
        .ok_or_else(|| RunError::NoSuchRetrieverTool {
            tool: asked.to_owned(),
            retriever: held.id().as_str().to_owned(),
            offered: held
                .tools()
                .offered()
                .map(RetrieverTool::name)
                .collect::<Vec<_>>()
                .join(", "),
        })
}

/// What a model is told one tool is, whichever kind of source answers it.
///
/// A description is optional for an MCP tool, which may answer `tools/list` without one, and
/// never absent for a retriever's, which this crate writes itself.
struct Declaration {
    description: Option<String>,
    schema: serde_json::Value,
}

/// The tool a server offers under that name, or that it offers none.
///
/// A tool the offering dropped is not looked for here and needs no word of its own: it was
/// dropped because its name is one [`McpToolName`] refuses, and a template that reached this
/// far is holding a name that type accepted, so the two can never be the same name.
fn found<'a>(offered: &Offered<'a>, named: &McpToolName) -> Result<&'a ip_mcp::McpTool, RunError> {
    offered
        .offering
        .tools
        .iter()
        .find(|one| one.name == *named)
        .ok_or_else(|| RunError::NoSuchTool {
            server: offered.server.as_str().to_owned(),
            tool: named.as_str().to_owned(),
        })
}

/// The name a model is told for one addressed tool.
///
/// Nothing is replaced on the way: a server id may hold a dot, but the split is on the first
/// one, so the half that reaches here never does — and what is left of either half is already
/// the set every provider allows. Two different pairs can still meet in the middle, as
/// `a` and `_b` do with `a_` and `b`, which is why the result is checked for having been taken.
fn wired(addressed: &Addressed) -> String {
    format!("{}{WIRE_JOIN}{}", addressed.source(), addressed.tool())
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

    /// A deployment that holds no retriever, which is what every test of the MCP half resolves
    /// against: a name held by neither registry is a server, as it always was.
    const fn mcp_only() -> Registries<'static> {
        Registries {
            servers: &[],
            retrievers: &[],
        }
    }

    fn assembled(asked: &[&str], server: &McpId, offering: &Offering) -> Toolbox {
        Toolbox::assemble(&set(asked), &[Offered { server, offering }], mcp_only()).unwrap()
    }

    fn refused(asked: &[&str], server: &McpId, offering: &Offering) -> RunError {
        Toolbox::assemble(&set(asked), &[Offered { server, offering }], mcp_only()).unwrap_err()
    }

    /// A retriever of that id, offering `tools`, with a backend that answers nothing.
    ///
    /// What is resolved is the row, so the backend behind it is never asked here.
    fn held(id: &str, tools: &[RetrieverTool]) -> Held {
        Held {
            retriever: crate::fixtures::retriever(id, tools),
            lookup: Arc::new(Nothing),
        }
    }

    /// A backend nothing in this module calls.
    #[derive(Debug)]
    struct Nothing;

    #[async_trait::async_trait]
    impl ip_retrieve::Retrieval for Nothing {
        async fn look(
            &self,
            _asked: &ip_retrieve::LookUp,
        ) -> Result<Vec<ip_retrieve::Looked>, String> {
            Ok(Vec::new())
        }
    }

    /// Both kinds in one tool set, which is the point of the second source: a template may name
    /// a server's tool and a corpus to look things up in, and the model is told about both.
    #[test]
    fn a_tool_set_naming_both_kinds_resolves_to_both() {
        let warehouse = id("warehouse");
        let kept = [held("kb", &[RetrieverTool::Search])];
        let registries = Registries {
            servers: std::slice::from_ref(&warehouse),
            retrievers: &kept,
        };
        let asked = set(&["warehouse.get_stock", "kb.search"]);

        let named = sources_named(&asked, registries).unwrap();
        assert_eq!(named.servers, [warehouse.clone()].into());
        assert_eq!(named.retrievers, [RetrieverId::new("kb").unwrap()].into());

        let box_of = Toolbox::assemble(
            &asked,
            &[Offered {
                server: &warehouse,
                offering: &offering(&["get_stock"]),
            }],
            registries,
        )
        .unwrap();

        let names: Vec<&str> = box_of
            .declared()
            .iter()
            .map(|one| one.name.as_str())
            .collect();
        // In the order the set holds them, which is the template's own sorted order.
        assert_eq!(names, ["kb__search", "warehouse__get_stock"]);
        assert_eq!(
            box_of.find(&ToolName::new("kb__search").unwrap()),
            Some(&Addressed::Retriever {
                retriever: RetrieverId::new("kb").unwrap(),
                tool: RetrieverTool::Search,
            }),
            "a retriever's tool resolved somewhere else"
        );
        assert_eq!(
            box_of.find(&ToolName::new("warehouse__get_stock").unwrap()),
            Some(&Addressed::Mcp {
                server: warehouse,
                tool: McpToolName::new("get_stock").unwrap(),
            })
        );
    }

    /// What the operator wrote for the model is what reaches the model, through the same
    /// declaration an MCP tool's description travels in.
    #[test]
    fn a_retrievers_own_words_are_what_the_model_is_told() {
        let kept = [Held {
            retriever: crate::fixtures::collection("kb"),
            lookup: Arc::new(Nothing),
        }];
        let box_of = Toolbox::assemble(
            &set(&["kb.search"]),
            &[],
            Registries {
                servers: &[],
                retrievers: &kept,
            },
        )
        .unwrap();

        let declared = &box_of.declared()[0];
        let description = declared.description.as_deref().expect("its own words");
        assert!(description.contains("the handbook"), "{description}");
        assert!(
            description.contains("`team` (Which team the page belongs to)"),
            "the fields a filter may name did not reach the model: {description}"
        );
        assert_eq!(declared.schema["properties"]["hits"]["maximum"], 6);
    }

    /// A retriever's tools are a closed set, so this is answered without asking anything — and
    /// the answer says what it does offer, which an MCP server's cannot.
    #[test]
    fn a_tool_a_retriever_does_not_offer_refuses_the_run() {
        let kept = [held("kb", &[RetrieverTool::Search, RetrieverTool::Rank])];
        let registries = Registries {
            servers: &[],
            retrievers: &kept,
        };
        for asked in ["kb.cluster", "kb.sql"] {
            assert_eq!(
                Toolbox::assemble(&set(&[asked]), &[], registries),
                Err(RunError::NoSuchRetrieverTool {
                    tool: asked.to_owned(),
                    retriever: "kb".to_owned(),
                    offered: "search, rank".to_owned(),
                }),
                "{asked} resolved to something"
            );
        }
    }

    /// The fifth finding: one name, two readings, and no way to tell which the template meant.
    #[test]
    fn a_name_both_registries_answer_to_refuses_the_run() {
        let kb = id("kb");
        let kept = [held("kb", &[RetrieverTool::Search])];
        let registries = Registries {
            servers: std::slice::from_ref(&kb),
            retrievers: &kept,
        };

        assert_eq!(
            Toolbox::assemble(
                &set(&["kb.search"]),
                &[Offered {
                    server: &kb,
                    offering: &offering(&["search"]),
                }],
                registries,
            ),
            Err(RunError::ToolSourceAmbiguous {
                tool: "kb.search".to_owned(),
                named: "kb".to_owned(),
            })
        );
        assert_eq!(
            sources_named(&set(&["kb.search"]), registries),
            Err(RunError::ToolSourceAmbiguous {
                tool: "kb.search".to_owned(),
                named: "kb".to_owned(),
            }),
            "what is opened must refuse it too, before a connection is made"
        );
    }

    /// The ceiling is on the name a model is told, whichever kind of source it names.
    #[test]
    fn a_retriever_whose_declared_name_is_too_long_refuses_the_run() {
        let id = "k".repeat(60);
        let kept = [held(&id, &[RetrieverTool::Search])];
        let asked = format!("{id}.search");

        assert_eq!(
            Toolbox::assemble(
                &set(&[&asked]),
                &[],
                Registries {
                    servers: &[],
                    retrievers: &kept,
                },
            ),
            Err(RunError::ToolNameTooLong {
                tool: asked.clone(),
                wire: format!("{id}__search"),
                max: WIRE_MAX_BYTES,
            }),
            "{asked} was declared at {} bytes",
            asked.len() + 1
        );
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
            Some(&Addressed::Mcp {
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
        let box_of = Toolbox::assemble(&ToolSet::default(), &[], mcp_only()).unwrap();

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
            mcp_only(),
        )
        .unwrap();

        let names: Vec<&str> = box_of
            .declared()
            .iter()
            .map(|one| one.name.as_str())
            .collect();
        assert_eq!(names, ["ledger__sql", "warehouse__sql"]);
        assert_eq!(
            box_of.find(&ToolName::new("ledger__sql").unwrap()),
            Some(&Addressed::Mcp {
                server: ledger,
                tool: McpToolName::new("sql").unwrap(),
            })
        );
    }

    /// The failure M23's stored templates would hit, and the reason the dot is not required by
    /// the type: it reads back, and the run is what refuses.
    #[test]
    fn a_tool_naming_no_server_refuses_the_run_rather_than_the_row() {
        let stored: ToolSet = serde_json::from_str(r#"["sql","search"]"#).unwrap();

        assert_eq!(
            Toolbox::assemble(&stored, &[], mcp_only()),
            Err(RunError::UnqualifiedTool {
                tool: "search".to_owned()
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
            mcp_only(),
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
            mcp_only(),
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
