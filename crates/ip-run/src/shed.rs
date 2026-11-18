//! The servers a run holds open, the retrievers it may look something up in, and the tools both
//! offer it.

use std::collections::BTreeMap;
use std::sync::Arc;

use ip_agents::ToolSet;
use ip_gateway::Embedder;
use ip_mcp::{McpClient, McpId, McpServer, McpSettings, Produced};
use ip_protocol::{Tool, ToolName};
use ip_retrieve::RetrieverId;

use crate::error::RunError;
use crate::lookup::looked;
use crate::tools::{Addressed, Held, Offered, Registries, Toolbox, sources_named};

/// What one run needs opening: the tools it asks for, and everything they can be addressed to.
///
/// A struct rather than five arguments, and the embedder among them because a look-up is the one
/// tool this gateway answers itself: a search needs the deployment's own encoder, and a run that
/// asks for one where there is none is refused here rather than at the model's first call.
pub struct Opening<'a> {
    /// What the template asks for.
    pub set: &'a ToolSet,
    /// Every MCP server this deployment has registered.
    pub servers: &'a [McpServer],
    /// Every retriever the run's owner holds, with what looks something up in each.
    pub retrievers: &'a [Held],
    /// What turns a question into a vector, and none where this deployment embeds nothing.
    pub embedder: Option<&'a Arc<dyn Embedder>>,
    /// How long a call to an MCP server may take.
    pub settings: McpSettings,
}

/// Every server one run needs, opened once and held for as long as the run lasts, and every
/// retriever it may look something up in.
///
/// A server is opened once however many of its tools a template asks for, and the session it
/// opened is kept: an MCP server may hold state across calls, and reconnecting per call would
/// throw that away as well as pay for a handshake each time. A retriever is not opened at all —
/// its backend is one request per look-up — so what is held of one is the row and its client.
pub struct Toolshed {
    clients: BTreeMap<McpId, McpClient>,
    retrievers: BTreeMap<RetrieverId, Held>,
    embedder: Option<Arc<dyn Embedder>>,
    toolbox: Toolbox,
}

impl std::fmt::Debug for Toolshed {
    /// Names the servers and retrievers rather than their clients, which hold credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toolshed")
            .field("servers", &self.clients.keys().collect::<Vec<_>>())
            .field("retrievers", &self.retrievers.keys().collect::<Vec<_>>())
            .field("embeds", &self.embedder.is_some())
            .field("tools", &self.toolbox.declared().len())
            .finish()
    }
}

impl Toolshed {
    /// Opens every server the set names, asks each for its tools once, and resolves the set
    /// against what came back and against the retrievers the run holds.
    ///
    /// Everything that can refuse a run refuses here, before its first turn: a name that is
    /// not a tool address, a server this deployment has not registered, a server that will
    /// not answer, a tool no server offers, a tool a retriever does not offer, a name both
    /// answer to, and a search where this deployment embeds nothing.
    pub async fn open(opening: Opening<'_>) -> Result<Self, RunError> {
        let Opening {
            set,
            servers: registry,
            retrievers,
            embedder,
            settings,
        } = opening;
        let mut clients = BTreeMap::new();
        let mut offerings = Vec::new();
        let registered: Vec<McpId> = registry.iter().map(|known| known.id.clone()).collect();
        let held = Registries {
            servers: &registered,
            retrievers,
        };
        let named = sources_named(set, held)?;
        for id in named.servers {
            let server = registry
                .iter()
                .find(|known| known.id == id)
                .ok_or_else(|| RunError::NoSuchServer {
                    tool: set
                        .tools()
                        .iter()
                        .find(|asked| asked.as_str().starts_with(&format!("{}.", id.as_str())))
                        .map_or_else(String::new, |asked| asked.as_str().to_owned()),
                    server: id.as_str().to_owned(),
                })?;
            let client = McpClient::open(server.clone(), settings)
                .await
                .map_err(|error| RunError::ServerUnreachable {
                    server: id.as_str().to_owned(),
                    why: error.to_string(),
                })?;
            let offering = client
                .tools()
                .await
                .map_err(|error| RunError::ServerUnreachable {
                    server: id.as_str().to_owned(),
                    why: error.to_string(),
                })?;
            clients.insert(id.clone(), client);
            offerings.push((id, offering));
        }
        let offered: Vec<Offered<'_>> = offerings
            .iter()
            .map(|(id, offering)| Offered {
                server: id,
                offering,
            })
            .collect();
        let toolbox = Toolbox::assemble(set, &offered, held)?;
        if let Some(tool) = toolbox.searching().filter(|_| embedder.is_none()) {
            return Err(RunError::NothingEmbeds { tool });
        }
        Ok(Self {
            clients,
            retrievers: named
                .retrievers
                .iter()
                .filter_map(|id| {
                    held.retrievers
                        .iter()
                        .find(|one| one.retriever.id() == id)
                        .map(|one| (id.clone(), one.clone()))
                })
                .collect(),
            embedder: embedder.map(Arc::clone),
            toolbox,
        })
    }

    /// Looks something up in the retriever a tool was addressed to.
    ///
    /// Separate from [`Self::run`] only because what it does is a different thing: the arguments
    /// are read, the words embedded and the rows rendered here rather than passed to a server
    /// that does its own. Nothing here ends a run either.
    async fn looking(
        &self,
        called: &ToolName,
        addressed: &Addressed,
        arguments: &serde_json::Map<String, serde_json::Value>,
    ) -> Produced {
        let Addressed::Retriever { retriever, tool } = addressed else {
            // Unreachable: the caller matched the other variant away.
            return Produced {
                text: format!("the tool {:?} is not open", called.as_str()),
                failed: true,
            };
        };
        let Some(held) = self.retrievers.get(retriever) else {
            return Produced {
                text: format!("the retriever {:?} is not open", retriever.as_str()),
                failed: true,
            };
        };
        looked(held, self.embedder.as_ref(), *tool, arguments).await
    }

    /// Every tool, as the model is told about them.
    #[must_use]
    pub fn declared(&self) -> &[Tool] {
        self.toolbox.declared()
    }

    /// Whether the model is told about no tools at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.toolbox.is_empty()
    }

    /// Runs what the model asked for, and says what came back.
    ///
    /// Nothing here ends a run. A tool the model invented, a server that has gone away and a
    /// tool that failed are all answers the model is given and may act on — which is what
    /// [`Produced::failed`] is for. A run ends on what the *gateway* refuses, not on what a
    /// tool does, because a model told its tool failed can try another and often does.
    pub async fn run(
        &self,
        called: &ToolName,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Produced {
        let Some(addressed) = self.toolbox.find(called) else {
            return Produced {
                text: format!("there is no tool called {:?}", called.as_str()),
                failed: true,
            };
        };
        let Addressed::Mcp { server, tool } = addressed else {
            return self.looking(called, addressed, &arguments).await;
        };
        let Some(client) = self.clients.get(server) else {
            return Produced {
                text: format!("the server {:?} is not open", server.as_str()),
                failed: true,
            };
        };
        match client.run(tool, arguments).await {
            Ok(produced) => produced,
            Err(error) => {
                tracing::warn!(
                    server = server.as_str(),
                    tool = tool.as_str(),
                    %error,
                    "a tool failed; the model is told and the run goes on"
                );
                Produced {
                    text: format!("the tool failed: {error}"),
                    failed: true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use async_trait::async_trait;
    use ip_retrieve::{LookUp, Looked, Retrieval};

    use super::*;
    use crate::fixtures::{Serving, asking, holding, registered, retrieving, stub};

    /// A backend nothing in these tests asks.
    #[derive(Debug)]
    struct Nothing;

    #[async_trait]
    impl Retrieval for Nothing {
        async fn look(&self, _asked: &LookUp) -> Result<Vec<Looked>, String> {
            Ok(Vec::new())
        }
    }

    /// One that answers the rows it was built with.
    #[derive(Debug)]
    struct Answering(Vec<Looked>);

    #[async_trait]
    impl Retrieval for Answering {
        async fn look(&self, _asked: &LookUp) -> Result<Vec<Looked>, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn every_tool_a_template_asks_for_is_declared_to_the_model() {
        let (address, _) = stub(Serving::Both).await;

        let shed = Toolshed::open(holding(
            &asking(&["warehouse.sql", "warehouse.search"]),
            &[registered("warehouse", address)],
        ))
        .await
        .unwrap();

        let declared: Vec<&str> = shed
            .declared()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        // Name order, which is the order a tool set holds its tools in.
        assert_eq!(declared, ["warehouse__search", "warehouse__sql"]);
        assert!(!shed.is_empty());
    }

    /// Two tools on one server is one connection and one listing, not two of each.
    #[tokio::test]
    async fn a_server_is_opened_once_however_many_of_its_tools_are_asked_for() {
        let (address, called) = stub(Serving::Both).await;

        Toolshed::open(holding(
            &asking(&["warehouse.sql", "warehouse.search"]),
            &[registered("warehouse", address)],
        ))
        .await
        .unwrap();

        let asked = called.lock().unwrap();
        let listings = asked
            .iter()
            .filter(|one| one["method"] == "tools/list")
            .count();
        let handshakes = asked
            .iter()
            .filter(|one| one["method"] == "initialize")
            .count();
        assert_eq!((handshakes, listings), (1, 1));
    }

    #[tokio::test]
    async fn what_the_model_calls_reaches_the_server_that_offers_it() {
        let (address, called) = stub(Serving::Both).await;
        let shed = Toolshed::open(holding(
            &asking(&["warehouse.sql"]),
            &[registered("warehouse", address)],
        ))
        .await
        .unwrap();
        let mut arguments = serde_json::Map::new();
        arguments.insert("query".to_owned(), serde_json::json!("select 1"));

        let produced = shed
            .run(&ToolName::new("warehouse__sql").unwrap(), arguments)
            .await;

        assert!(!produced.failed, "{produced:?}");
        assert!(produced.text.contains("select 1"));
        let asked = called.lock().unwrap();
        let call = asked
            .iter()
            .find(|one| one["method"] == "tools/call")
            .expect("the call went out");
        assert_eq!(
            call["params"]["name"], "sql",
            "the server is told its own name for the tool, not the wire name"
        );
    }

    /// A model that invents a tool is told so and goes on, rather than ending the run.
    #[tokio::test]
    async fn a_tool_the_model_invented_comes_back_as_a_failed_result() {
        let (address, _) = stub(Serving::Both).await;
        let shed = Toolshed::open(holding(
            &asking(&["warehouse.sql"]),
            &[registered("warehouse", address)],
        ))
        .await
        .unwrap();

        let produced = shed
            .run(
                &ToolName::new("warehouse__drop_everything").unwrap(),
                serde_json::Map::new(),
            )
            .await;

        assert!(produced.failed);
        assert!(produced.text.contains("no tool"), "{produced:?}");
    }

    /// Neither does a tool that fails: the model is told and may try another.
    #[tokio::test]
    async fn a_tool_the_server_will_not_run_comes_back_as_a_failed_result() {
        let (address, _) = stub(Serving::RefusesCalls).await;
        let shed = Toolshed::open(holding(
            &asking(&["warehouse.sql"]),
            &[registered("warehouse", address)],
        ))
        .await
        .unwrap();

        let produced = shed
            .run(
                &ToolName::new("warehouse__sql").unwrap(),
                serde_json::Map::new(),
            )
            .await;

        assert!(produced.failed);
        assert!(produced.text.contains("failed"), "{produced:?}");
    }

    /// Refused before anything is opened, so a typo costs no connection.
    #[tokio::test]
    async fn a_server_this_deployment_does_not_have_refuses_the_run() {
        let refused = Toolshed::open(holding(&asking(&["elsewhere.sql"]), &[]))
            .await
            .unwrap_err();

        assert!(
            matches!(refused, RunError::NoSuchServer { ref server, .. } if server == "elsewhere"),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn a_registered_server_that_will_not_answer_refuses_the_run() {
        let nowhere: SocketAddr = "127.0.0.1:1".parse().unwrap();

        let refused = Toolshed::open(holding(
            &asking(&["warehouse.sql"]),
            &[registered("warehouse", nowhere)],
        ))
        .await
        .unwrap_err();

        assert!(
            matches!(refused, RunError::ServerUnreachable { ref server, .. } if server == "warehouse"),
            "{refused:?}"
        );
    }

    /// A server that is reachable but offers less than the template was written for.
    #[tokio::test]
    async fn a_tool_the_server_does_not_offer_refuses_the_run() {
        let (address, _) = stub(Serving::SqlOnly).await;

        let refused = Toolshed::open(holding(
            &asking(&["warehouse.sql", "warehouse.search"]),
            &[registered("warehouse", address)],
        ))
        .await
        .unwrap_err();

        assert!(
            matches!(refused, RunError::NoSuchTool { ref tool, .. } if tool == "search"),
            "{refused:?}"
        );
    }

    /// The one every milestone since M23 has to keep passing: a stored template written
    /// before any server existed names a bare tool, and the run is refused, not the read.
    #[tokio::test]
    async fn a_template_naming_a_bare_tool_refuses_the_run() {
        let refused = Toolshed::open(holding(&asking(&["sql"]), &[]))
            .await
            .unwrap_err();

        assert!(
            matches!(refused, RunError::UnqualifiedTool { ref tool } if tool == "sql"),
            "{refused:?}"
        );
    }

    /// The refusal that exists so a model is never declared a tool this deployment cannot
    /// answer: it costs a status code here and paid turns anywhere later.
    #[tokio::test]
    async fn a_run_that_asks_to_search_where_nothing_embeds_is_refused_up_front() {
        let kept = [retrieving("kb", Nothing)];

        let refused = Toolshed::open(Opening {
            set: &asking(&["kb.search"]),
            servers: &[],
            retrievers: &kept,
            embedder: None,
            settings: crate::fixtures::settings(),
        })
        .await
        .unwrap_err();

        assert_eq!(
            refused,
            RunError::NothingEmbeds {
                tool: "kb__search".to_owned()
            }
        );
    }

    /// The four tools that walk edges compare nothing of ours, so a deployment without an
    /// artefact may still run a template that uses them.
    #[tokio::test]
    async fn a_run_that_only_walks_edges_needs_no_encoder() {
        let kept = [retrieving("kb", Nothing)];

        let shed = Toolshed::open(Opening {
            set: &asking(&["kb.neighbors"]),
            servers: &[],
            retrievers: &kept,
            embedder: None,
            settings: crate::fixtures::settings(),
        })
        .await
        .expect("a walk needs no encoder");

        assert_eq!(
            shed.declared()
                .iter()
                .map(|one| one.name.as_str())
                .collect::<Vec<_>>(),
            ["kb__neighbors"]
        );
    }

    /// What the model calls reaches the retriever it was addressed to, and comes back as the
    /// rows it answered.
    #[tokio::test]
    async fn what_the_model_calls_reaches_the_retriever_that_answers_it() {
        let kept = [retrieving(
            "kb",
            Answering(vec![Looked {
                id: "7".to_owned(),
                score: Some(0.5),
                payload: serde_json::json!({"title": "Leave"}),
            }]),
        )];
        let shed = Toolshed::open(Opening {
            set: &asking(&["kb.neighbors"]),
            servers: &[],
            retrievers: &kept,
            embedder: None,
            settings: crate::fixtures::settings(),
        })
        .await
        .unwrap();
        let mut arguments = serde_json::Map::new();
        arguments.insert("node".to_owned(), serde_json::json!(7));

        let produced = shed
            .run(&ToolName::new("kb__neighbors").unwrap(), arguments)
            .await;

        assert!(!produced.failed, "{produced:?}");
        assert!(produced.text.contains("Leave"), "{produced:?}");
    }

    /// A debug line names what a run holds and never what it holds it with.
    #[tokio::test]
    async fn a_shed_names_its_sources_and_not_their_credentials() {
        let kept = [retrieving("kb", Nothing)];
        let (address, _) = stub(Serving::Both).await;

        let shed = Toolshed::open(Opening {
            set: &asking(&["warehouse.sql", "kb.neighbors"]),
            servers: &[registered("warehouse", address)],
            retrievers: &kept,
            embedder: None,
            settings: crate::fixtures::settings(),
        })
        .await
        .unwrap();

        let said = format!("{shed:?}");
        assert!(said.contains("warehouse"), "{said}");
        assert!(said.contains("kb"), "{said}");
        assert!(said.contains("embeds: false"), "{said}");
        assert!(!said.contains("token"), "{said}");
    }

    /// Nothing is opened for a template that asks for no tools.
    #[tokio::test]
    async fn a_template_with_no_tools_opens_nothing() {
        let shed = Toolshed::open(holding(&ToolSet::default(), &[]))
            .await
            .unwrap();

        assert!(shed.is_empty());
        assert!(shed.declared().is_empty());
    }
}
