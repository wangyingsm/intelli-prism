//! The servers a run holds open, and the tools they offer it.

use std::collections::BTreeMap;

use ip_agents::ToolSet;
use ip_mcp::{McpClient, McpId, McpServer, McpSettings, Produced};
use ip_protocol::{Tool, ToolName};

use crate::error::RunError;
use crate::tools::{Offered, Toolbox, servers_named};

/// Every server one run needs, opened once and held for as long as the run lasts.
///
/// A server is opened once however many of its tools a template asks for, and the session it
/// opened is kept: an MCP server may hold state across calls, and reconnecting per call would
/// throw that away as well as pay for a handshake each time.
pub struct Toolshed {
    clients: BTreeMap<McpId, McpClient>,
    toolbox: Toolbox,
}

impl std::fmt::Debug for Toolshed {
    /// Names the servers rather than their sessions, which hold a credential.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toolshed")
            .field("servers", &self.clients.keys().collect::<Vec<_>>())
            .field("tools", &self.toolbox.declared().len())
            .finish()
    }
}

impl Toolshed {
    /// Opens every server the set names, asks each for its tools once, and resolves the set
    /// against what came back.
    ///
    /// Everything that can refuse a run refuses here, before its first turn: a name that is
    /// not a tool address, a server this deployment has not registered, a server that will
    /// not answer, and a tool no server offers.
    pub async fn open(
        set: &ToolSet,
        registry: &[McpServer],
        settings: McpSettings,
    ) -> Result<Self, RunError> {
        let mut clients = BTreeMap::new();
        let mut offerings = Vec::new();
        for id in servers_named(set)? {
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
        let toolbox = Toolbox::assemble(set, &offered)?;
        Ok(Self { clients, toolbox })
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
        let Some(client) = self.clients.get(&addressed.server) else {
            return Produced {
                text: format!("the server {:?} is not open", addressed.server.as_str()),
                failed: true,
            };
        };
        match client.run(&addressed.tool, arguments).await {
            Ok(produced) => produced,
            Err(error) => {
                tracing::warn!(
                    server = addressed.server.as_str(),
                    tool = addressed.tool.as_str(),
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

    use super::*;
    use crate::fixtures::{Serving, asking, registered, settings, stub};

    #[tokio::test]
    async fn every_tool_a_template_asks_for_is_declared_to_the_model() {
        let (address, _) = stub(Serving::Both).await;

        let shed = Toolshed::open(
            &asking(&["warehouse.sql", "warehouse.search"]),
            &[registered("warehouse", address)],
            settings(),
        )
        .await
        .unwrap();

        let declared: Vec<&str> = shed
            .declared()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(declared, ["warehouse__sql", "warehouse__search"]);
        assert!(!shed.is_empty());
    }

    /// Two tools on one server is one connection and one listing, not two of each.
    #[tokio::test]
    async fn a_server_is_opened_once_however_many_of_its_tools_are_asked_for() {
        let (address, called) = stub(Serving::Both).await;

        Toolshed::open(
            &asking(&["warehouse.sql", "warehouse.search"]),
            &[registered("warehouse", address)],
            settings(),
        )
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
        let shed = Toolshed::open(
            &asking(&["warehouse.sql"]),
            &[registered("warehouse", address)],
            settings(),
        )
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
        let shed = Toolshed::open(
            &asking(&["warehouse.sql"]),
            &[registered("warehouse", address)],
            settings(),
        )
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
        let shed = Toolshed::open(
            &asking(&["warehouse.sql"]),
            &[registered("warehouse", address)],
            settings(),
        )
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
        let refused = Toolshed::open(&asking(&["elsewhere.sql"]), &[], settings())
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

        let refused = Toolshed::open(
            &asking(&["warehouse.sql"]),
            &[registered("warehouse", nowhere)],
            settings(),
        )
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

        let refused = Toolshed::open(
            &asking(&["warehouse.sql", "warehouse.search"]),
            &[registered("warehouse", address)],
            settings(),
        )
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
        let refused = Toolshed::open(&asking(&["sql"]), &[], settings())
            .await
            .unwrap_err();

        assert!(
            matches!(refused, RunError::UnqualifiedTool { ref tool } if tool == "sql"),
            "{refused:?}"
        );
    }

    /// Nothing is opened for a template that asks for no tools.
    #[tokio::test]
    async fn a_template_with_no_tools_opens_nothing() {
        let shed = Toolshed::open(&ToolSet::default(), &[], settings())
            .await
            .unwrap();

        assert!(shed.is_empty());
        assert!(shed.declared().is_empty());
    }
}
