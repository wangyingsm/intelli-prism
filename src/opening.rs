//! Everything one run may call, opened before its first turn.
//!
//! Three routes start runs — an agent, a graph's nodes and a dispatched node on a worker — and
//! each needs the same thing of the store: the MCP servers this deployment has registered, and
//! the retrievers the template's owner holds. This is that, in one place, so a run opened on a
//! worker can call exactly what the same run opened by the coordinator can.
//!
//! **Read before the stream opens, never during it.** A template naming something that is not
//! there is a status code, and would otherwise be a stream that stops a turn in — which is the
//! rule [`Toolshed::open`] already keeps for servers and this keeps for the store behind it.

use std::time::Duration;

use ip_agents::ToolSet;
use ip_mcp::{McpServer, McpSettings};
use ip_run::{Held, Opening, Toolshed};
use ip_storage::{AgentOwner, Page};

use crate::error::Unopened;
use crate::retrieve::{held, retrievers_of};
use crate::state::AppState;

/// How long a run's tool calls wait on an MCP server.
pub const MCP_SETTINGS: McpSettings = McpSettings {
    connect_timeout: Duration::from_secs(10),
    request_timeout: Duration::from_secs(30),
    max_answer_bytes: 4 * 1024 * 1024,
};

/// How many registered servers a run may choose its tools from.
pub const REGISTRY_LIMIT: u32 = 1_000;

/// How many of its owner's retrievers a run may address.
///
/// The same shape as the registry's own ceiling, and for the same reason: a run reads them all
/// before its first turn, so the read is bounded rather than paged.
pub const RETRIEVER_LIMIT: u32 = 1_000;

/// Everything a run of `owner`'s templates may address: every registered server, and every
/// retriever that owner holds, opened against its backend.
///
/// Both halves in one read, because a graph's nodes share one read and open a shed each: reading
/// them per node would mean a graph of ten nodes reading the store twenty times to be told the
/// same thing.
pub async fn sources(
    state: &AppState,
    owner: &AgentOwner,
) -> Result<(Vec<McpServer>, Vec<Held>), Unopened> {
    let servers = state
        .store()
        .mcps(Page::new(REGISTRY_LIMIT, 0, None))
        .await?
        .into_iter()
        .map(|stored| stored.server)
        .collect();
    let stored = state
        .store()
        .retrievers(&retrievers_of(owner), Page::new(RETRIEVER_LIMIT, 0, None))
        .await?;
    Ok((
        servers,
        held(stored.into_iter().map(|one| one.retriever).collect())?,
    ))
}

/// Everything one run may call, opened: for a run of one template, which is every run but a
/// graph's.
pub async fn toolshed(
    state: &AppState,
    owner: &AgentOwner,
    set: &ToolSet,
) -> Result<Toolshed, Unopened> {
    let (servers, retrievers) = sources(state, owner).await?;
    Ok(Toolshed::open(Opening {
        set,
        servers: &servers,
        retrievers: &retrievers,
        embedder: state.embedder(),
        settings: MCP_SETTINGS,
    })
    .await?)
}
