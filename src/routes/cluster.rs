//! Running one node of somebody else's graph, for another member of this cluster.
//!
//! The one route in this server that is not for a person or for a caller's api key. What
//! authenticates it is the cluster secret, which says exactly one thing: whoever presented it is
//! a member of this deployment's own cluster. That buys a member the right to say *which* run a
//! node belongs to and whose — never what that caller may do. The authority every turn goes out
//! under is built here, from this machine's own reads of the same database, by the same code a
//! local run uses.
//!
//! Where the line between a status and an event falls, and why: **the secret and the shape of the
//! request are statuses; everything about the work is an event.** A dispatch the member will not
//! authenticate never reaches the work, so it is a 401. A template that has gone missing is a
//! `refused` event on an open stream, because by the time this machine knows, the coordinator is
//! already streaming to somebody and a status code has nowhere to go. It is also the one failure
//! a coordinator may act on: a refusal means no turn was taken, so the node can still be run at
//! home.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use ip_agents::{AgentConfig, Prompt};
use ip_cache::CacheBackend;
use ip_cluster::{Dispatch, NodeEvent};
use ip_config::Secret;
use ip_core::Protocol;
use ip_graph::{GraphRunId, NodeRan};
use ip_run::{
    AsWhom, Pinned, Reporting, RunEvent, RunId, RunSignal, Running, Spend, Stopped, Target,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::graph::templates_of;
use crate::cluster::{already_stopped, signal_topic};
use crate::manage::authority_of;
use crate::opening::toolshed;
use crate::state::AppState;

/// Takes one node's work from another member and streams what the node does with it.
///
/// The body is read only once the secret has checked out, so an unauthenticated caller learns
/// nothing about the shape of a dispatch — not even that it got it wrong.
pub(super) async fn node(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if !presents(state.cluster_secret(), &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let work: Dispatch = match serde_json::from_slice(&body) {
        Ok(work) => work,
        Err(error) => {
            return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
        }
    };
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move { dispatched(state, work, sender).await });
    Sse::new(
        UnboundedReceiverStream::new(receiver).map(|event| Ok::<_, Infallible>(written(&event))),
    )
    .into_response()
}

/// Whether the caller presents the secret this cluster's members share.
///
/// False where this deployment holds none: a deployment with no secret is in no cluster, so it
/// has no peers and nothing may be dispatched to it. That is the opposite of the scrape
/// endpoint's rule, where an operator who set no token asked for an open endpoint — here nobody
/// asked for anything.
///
/// The comparison is `Secret`'s own, which is constant time: a secret checked with a short
/// circuiting equality tells whoever is guessing how much of it they had right.
fn presents(secret: Option<&Secret>, headers: &HeaderMap) -> bool {
    let Some(secret) = secret else {
        return false;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| &Secret::from(presented) == secret)
}

/// One event as the stream carries it: the name for a person, the whole event for a reader.
///
/// An event that will not write down is itself streamed as a refusal, so a coordinator reads the
/// failure as a node that did not run rather than as a stream it cannot parse.
fn written(event: &NodeEvent) -> SseEvent {
    match serde_json::to_string(event) {
        Ok(data) => SseEvent::default().event(event.name()).data(data),
        Err(error) => {
            tracing::error!(%error, "a dispatched node's event could not be written down");
            SseEvent::default().event("refused").data(UNWRITABLE)
        }
    }
}

/// What is streamed when an event will not write down, which is a [`NodeEvent::Refused`].
const UNWRITABLE: &str = r#"{"refused":"an event of this node could not be written down"}"#;

/// What a node of a run that has already stopped answers, where the run has.
///
/// Read before the node runs rather than only followed, because a follower starts at the revision
/// standing *now* and wakes only on what comes after: a node handed to this member after the
/// coordinator published the stop has nothing to wake on and would otherwise run in full.
///
/// Answered as a node that ran and was cut short, and never as a refusal. A refusal is what sends
/// the coordinator off to run the node at home, which for a run that has already stopped is the
/// one wrong answer there is — it would turn a stop into the work happening anyway, elsewhere.
async fn cut_short_already(state: &AppState, run: GraphRunId) -> Option<NodeEvent> {
    already_stopped(state.shared_cache().as_ref(), run)
        .await
        .then(|| {
            NodeEvent::Answered(NodeRan {
                answer: String::new(),
                stopped: Stopped::CutShort,
                spend: Spend::NOTHING,
                traces: Vec::new(),
            })
        })
}

/// Flips `stop` when this run's stop is published, for as long as the task is left running.
///
/// Nothing when the topic cannot be followed, and that is **not** a reason to refuse the node: a
/// stop that cannot arrive leaves a node running its course, which is what every member did
/// before the stop existed. The alternative turns a cache that is down into work nobody can take.
async fn follow_the_stop(
    cache: &Arc<dyn CacheBackend>,
    run: GraphRunId,
    stop: watch::Sender<RunSignal>,
) -> Option<JoinHandle<()>> {
    let topic = match signal_topic(run) {
        Ok(topic) => topic,
        Err(error) => {
            tracing::warn!(%error, %run, "a dispatched node has no signal topic to follow");
            return None;
        }
    };
    let mut following = match cache.follow(&topic).await {
        Ok(following) => following,
        Err(error) => {
            tracing::warn!(
                %error, %run,
                "could not follow a run's signals; this node runs its course"
            );
            return None;
        }
    };
    Some(tokio::spawn(async move {
        // One kind of thing is published under this topic, so the first move of the revision is
        // the stop and there is nothing after it to go on waiting for.
        if following.changed().await.is_ok() {
            let _ = stop.send(RunSignal::Stop);
        }
    }))
}

/// One node's turns, said into the dispatch's own stream.
struct Saying {
    /// Where the stream is written.
    say: UnboundedSender<NodeEvent>,
}

impl Reporting for Saying {
    /// Everything but the run's own `done`, which this answers as [`NodeEvent::Answered`].
    ///
    /// The same omission the local runner makes, for the same reason and so that the two streams
    /// are the same stream: both carry the run's three closing facts once, under the name that
    /// says a *node* ended rather than a run.
    fn say(&self, event: RunEvent) {
        if matches!(event, RunEvent::Done { .. }) {
            return;
        }
        let _ = self.say.send(NodeEvent::Said(event));
    }
}

/// What the dispatched node runs under: the template this machine stores, told what it may ask for.
async fn node_config(state: &AppState, work: &Dispatch) -> Result<AgentConfig, String> {
    let config = match state
        .store()
        .agent(&templates_of(&work.owner), &work.agent)
        .await
    {
        Ok(Some(held)) => held.template.config,
        Ok(None) => {
            return Err(format!(
                "the template {} is not stored for this owner",
                work.agent
            ));
        }
        Err(error) => return Err(format!("the templates could not be read: {error}")),
    };
    told_what_the_graph_says(state, work, config).await
}

/// The dispatched node's configuration with what the graph lets it ask for added to its prompt.
///
/// **Derived here rather than carried on the dispatch**, which is the same decision
/// [`Dispatch`] records for the authority and for the same reason: both machines read the one
/// plane, so a member works out what this node may ask for exactly as the coordinator did. A
/// dispatch that carried the instruction could carry one the graph does not say, and a member
/// taking the coordinator's word for it would be told what a node may do by something other than
/// the graph that declares it.
///
/// Two statements rather than a whole graph read: only the loop backs and the branches decide
/// this, and each is one read of its own.
///
/// A member that cannot reach the plane refuses the node. It cannot tell a graph with no loop
/// backs and no branches from one it failed to read, and running a node that may send its work
/// back, or choose what follows it, without telling it so is the defect this exists to close —
/// the same node would have been told, had the coordinator run it itself.
async fn told_what_the_graph_says(
    state: &AppState,
    work: &Dispatch,
    config: AgentConfig,
) -> Result<AgentConfig, String> {
    let Some(plane) = state.graphs() else {
        return Err("this deployment keeps no graphs, so a node of one cannot run here".to_owned());
    };
    let loops = plane
        .loops(&work.owner, &work.graph)
        .await
        .map_err(|error| format!("the graph's loop backs could not be read: {error}"))?;
    let branches = plane
        .branches(&work.owner, &work.graph)
        .await
        .map_err(|error| format!("the graph's branches could not be read: {error}"))?;
    let Some(advice) = ip_graph::graph_advice(&loops, &branches, &work.node) else {
        return Ok(config);
    };
    let told = Prompt::new(&format!("{}\n\n{advice}", config.prompt().as_str())).map_err(|_| {
        format!(
            "the node {} is told what its graph lets it ask for, and its prompt has no room for it",
            work.node
        )
    })?;
    Ok(config.with_prompt(told))
}

/// Resolves the dispatched node against this machine's own database and runs it.
///
/// Every refusal is an event rather than a return, because the stream is already open by the
/// time this runs. The order is the order a local run resolves in, and deliberately so: the
/// caller's authority before the template, so a dispatch naming somebody who is not in that
/// tenant is refused before this machine reads anything of that tenant's.
async fn dispatched(state: AppState, work: Dispatch, say: UnboundedSender<NodeEvent>) {
    let refuse = |why: String| {
        let _ = say.send(NodeEvent::Refused(why));
    };
    tracing::info!(
        run = %work.run,
        graph = %work.graph,
        node = %work.node,
        agent = %work.agent,
        "running a node dispatched by another member of this cluster"
    );
    let store = state.store();
    let kind = match store.user(&work.user).await {
        Ok(Some(held)) => held.kind,
        Ok(None) => return refuse(format!("this deployment has no account {}", work.user)),
        Err(error) => return refuse(format!("the accounts could not be read: {error}")),
    };
    let authority = match authority_of(store.as_ref(), &work.user, kind, &work.tenant).await {
        Ok(Some(authority)) => authority,
        Ok(None) => {
            return refuse(format!("{} is not a member of {}", work.user, work.tenant));
        }
        Err(error) => {
            return refuse(format!(
                "what {} holds could not be read: {error}",
                work.user
            ));
        }
    };
    let config = match node_config(&state, &work).await {
        Ok(config) => config,
        Err(why) => return refuse(why),
    };
    let pinned = match Pinned::resolve(&work.requested, &config) {
        Ok(pinned) => pinned,
        Err(error) => return refuse(error.to_string()),
    };
    let target = match Target::aimed(
        state.upstreams(),
        &pinned,
        AsWhom {
            authority,
            listen: state.listen(),
            protocol: Protocol::Http,
        },
    ) {
        Ok(target) => target,
        Err(error) => return refuse(error.to_string()),
    };
    let shed = match toolshed(&state, &templates_of(&work.owner), config.tool_set()).await {
        Ok(shed) => shed,
        Err(error) => return refuse(error.to_string()),
    };
    let id = match RunId::generate() {
        Ok(id) => id,
        Err(error) => return refuse(format!("this run could not be named: {error}")),
    };
    if let Some(cut) = cut_short_already(&state, work.run).await {
        let _ = say.send(cut);
        return;
    }
    let (stop, signals) = watch::channel(RunSignal::Carry);
    let following = follow_the_stop(state.shared_cache(), work.run, stop).await;
    let ran = ip_run::run(
        &Running {
            id,
            gateway: state.gateway(),
            target: &target,
            shed: &shed,
            config: &config,
            pinned: &pinned,
            reporting: &Saying { say: say.clone() },
            signals: &signals,
        },
        &work.input,
    )
    .await;
    // For the life of the node and no longer: the topic outlives this node, and a follower left
    // behind would hold a connection for a run this machine has finished with.
    if let Some(following) = following {
        following.abort();
    }
    let _ = say.send(NodeEvent::Answered(NodeRan {
        answer: ran.text(),
        stopped: ran.stopped,
        spend: ran.spend,
        traces: ran.traces,
    }));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::http::Request;
    use ip_agents::{
        AgentConfig, AgentConfigParts, AgentId, AgentName, AgentRole, AgentTemplate, MaxTurns,
        Prompt, ReqTimeout, Sandbox, TimeLimit, ToolSet,
    };
    use ip_cluster::{STUB_SECRET, stub_dispatch};
    use ip_core::{
        Capability, CapabilityScope, Grant, TenantId, TnKey, UserId, route::ClusterAddress,
    };
    use ip_drsg::{Replies, StubPlane};
    use ip_graph::{GraphOwner, GraphPlane, NodeName};
    use ip_run::Requested;
    use ip_storage::{
        AccountKind, AgentOwner, AgentStore, By, GrantStore, Membership, MembershipStore, NewAgent,
        NewTenant, NewUser, SqliteStore, Standing, TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, hashed};
    use super::*;
    use crate::cluster::Signalling;

    /// The deployment a worker serves: one upstream, answering anthropic.
    const SERVING: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./unopened.db"

[cache]
backend = "sled"
path = "./unopened"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[[upstream]]
id = "anthropic"
base_url = "https://api.example.com/v1"
api_key = "sk-deployment"
model = "claude-opus-5"
caller_speaks = "anthropic"
upstream_speaks = "anthropic"
"#;

    /// An upstream that answers every turn with the same finished message.
    struct Answering;

    #[async_trait::async_trait]
    impl ip_gateway::upstream::Upstream for Answering {
        async fn send(
            &self,
            _request: Request<ip_gateway::body::GatewayBody>,
        ) -> Result<axum::http::Response<ip_gateway::body::GatewayBody>, ip_gateway::UpstreamError>
        {
            let body = serde_json::json!({
                "id": "msg_1",
                "model": "claude-opus-5",
                "role": "assistant",
                "type": "message",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "four"}],
                "usage": {"input_tokens": 10, "output_tokens": 2},
            })
            .to_string();
            Ok(axum::http::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(ip_gateway::from_bytes(body.into()))
                .unwrap())
        }
    }

    fn user(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    fn by(actor: &str, through: &'static str) -> By {
        By::new(user(actor), through)
    }

    /// The template `stub_dispatch` names, stored for the owner it names.
    fn template() -> AgentTemplate {
        AgentTemplate {
            id: AgentId::new("summarise").unwrap(),
            name: AgentName::new("summarise").unwrap(),
            sandbox: Sandbox::default(),
            config: AgentConfig::new(AgentConfigParts {
                role: AgentRole::System,
                prompt: Prompt::new("Be helpful.").unwrap(),
                tool_set: ToolSet::default(),
                max_turns: MaxTurns::new(4).unwrap(),
                time_limit: TimeLimit::new(60).unwrap(),
                req_timeout: ReqTimeout::new(10).unwrap(),
                best_effort: None,
                model: None,
                api: None,
            })
            .unwrap(),
        }
    }

    /// One tenant and three accounts, which is every shape a dispatch can name.
    ///
    /// `dana` owns `acme` and may proxy; `bob` is a member of it holding nothing, so the
    /// authority this machine builds for him refuses the turn; `mallory` is in no tenant at all.
    async fn store() -> Arc<SqliteStore> {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .create_tenant(
                NewTenant {
                    id: tenant("acme"),
                    key: TnKey::generate().unwrap(),
                },
                &by("root", "/_ip/tenants"),
            )
            .await
            .unwrap();
        for id in ["root", "dana", "bob", "mallory"] {
            let kind = if id == "root" {
                AccountKind::SystemAdministrator
            } else {
                AccountKind::Regular
            };
            store
                .create_user(
                    NewUser {
                        id: user(id),
                        passphrase: hashed(),
                        kind,
                    },
                    &by("root", "/_ip/users"),
                )
                .await
                .unwrap();
        }
        for (id, standing) in [("dana", Standing::Owner), ("bob", Standing::Member)] {
            store
                .attach(
                    Membership {
                        user: user(id),
                        tenant: tenant("acme"),
                        standing,
                    },
                    &by("root", "/_ip/tenants/{tenant}/members"),
                )
                .await
                .unwrap();
        }
        store
            .grant(
                &Grant::new(
                    Capability::SysAgent,
                    CapabilityScope::Tenant {
                        user: user("dana"),
                        tenant: tenant("acme"),
                    },
                )
                .unwrap(),
                &by("root", "/_ip/users/{user}/grants"),
            )
            .await
            .unwrap();
        store
            .put_agent(
                NewAgent {
                    owner: AgentOwner::Tenant(tenant("acme")),
                    template: template(),
                },
                &by("root", "/_ip/agents"),
            )
            .await
            .unwrap();
        Arc::new(store)
    }

    /// An upstream that keeps what it was sent, so a test can read what the model was told.
    struct Recording {
        /// Each request the gateway sent on, in order.
        seen: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl ip_gateway::upstream::Upstream for Recording {
        async fn send(
            &self,
            request: axum::http::Request<ip_gateway::body::GatewayBody>,
        ) -> Result<axum::http::Response<ip_gateway::body::GatewayBody>, ip_gateway::UpstreamError>
        {
            use http_body_util::BodyExt;

            let body = request.into_body().collect().await.unwrap().to_bytes();
            if let Ok(said) = serde_json::from_slice::<serde_json::Value>(&body) {
                self.seen
                    .lock()
                    .expect("the record is not poisoned")
                    .push(said);
            }
            Answering
                .send(axum::http::Request::new(ip_gateway::from_bytes(
                    Vec::new().into(),
                )))
                .await
        }
    }

    /// What a plane answers a worker's two reads with when the graph declares nothing.
    ///
    /// One empty table serves both the loop backs and the branches, because a scripted reply
    /// answers per *method* and both are `plane.cypher`. What the columns are called is not read.
    fn nothing_declared() -> Replies {
        Replies::scripted([(
            "plane.cypher",
            serde_json::json!({"columns": ["from", "to", "cap"], "rows": []}),
        )])
    }

    /// A worker holding its cluster's secret, with a template its peers may dispatch.
    ///
    /// It holds a plane, as a member of a cluster must: membership is kept there, so a deployment
    /// that cannot read it cannot be dispatched to. The stub comes back with the router because a
    /// dropped one closes its socket, and then the worker refuses what it cannot read.
    async fn worker_over(replies: Replies) -> (Router, StubPlane) {
        let plane = StubPlane::start(replies).await;
        let state = super::super::harness::serving(store().await, SERVING, Arc::new(Answering))
            .with_cluster_secret(Secret::from(STUB_SECRET))
            .with_graphs(Arc::new(GraphPlane::new(plane.client())))
            .with_cluster(
                ClusterAddress::of("127.0.0.1:8080".parse().unwrap())
                    .unwrap()
                    .unwrap(),
            );
        (super::super::router(state), plane)
    }

    /// The same over a graph that declares no loop back, which is every test but one.
    async fn worker() -> Worker {
        let (router, plane) = worker_over(nothing_declared()).await;
        Worker { router, plane }
    }

    /// A worker and the plane it reads, kept together so neither outlives the other.
    struct Worker {
        /// What a dispatch is sent to.
        router: Router,
        /// The plane it reads the graph's loop backs from, held open for as long as the router
        /// is: a dropped stub closes its socket, and the worker then refuses what it cannot read.
        #[expect(dead_code, reason = "held for its lifetime, not read")]
        plane: StubPlane,
    }

    impl Worker {
        /// Sends one request, shaped like [`tower::ServiceExt::oneshot`] so that a test of a
        /// worker reads exactly as a test of a bare router does.
        async fn oneshot(&self, request: Request<Body>) -> Result<Response<Body>, Infallible> {
            self.router.clone().oneshot(request).await
        }
    }

    /// A dispatch as another member sends one, with whatever the test changes about it.
    fn sent(secret: Option<&str>, work: &Dispatch) -> Request<Body> {
        let builder = Request::builder()
            .method("POST")
            .uri(ip_cluster::DISPATCH_PATH)
            .header("content-type", "application/json");
        let builder = match secret {
            Some(secret) => builder.header("authorization", format!("Bearer {secret}")),
            None => builder,
        };
        builder
            .body(Body::from(serde_json::to_string(work).unwrap()))
            .unwrap()
    }

    /// Every event of a dispatch's stream, as its name and what it carried.
    async fn streamed(response: Response<Body>) -> Vec<(String, NodeEvent)> {
        let body = body_of(response).await;
        let mut said = Vec::new();
        let mut name = String::new();
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                name = rest.trim().to_owned();
            } else if let Some(rest) = line.strip_prefix("data:") {
                said.push((
                    name.clone(),
                    serde_json::from_str(rest.trim())
                        .unwrap_or_else(|error| panic!("{error}: {rest}")),
                ));
            }
        }
        said
    }

    /// A node that really runs: its turn, its text, and what it came to.
    #[tokio::test]
    async fn a_dispatched_node_runs_and_answers() {
        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &stub_dispatch()))
            .await
            .unwrap();

        assert_eq!(answered.status(), StatusCode::OK);
        let said = streamed(answered).await;
        let names: Vec<&str> = said.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names.last(), Some(&"answered"), "{names:?}");
        let Some((_, NodeEvent::Answered(ran))) = said.last() else {
            panic!("a dispatch did not end in an answer: {said:?}");
        };
        assert_eq!(ran.answer, "four");
        assert_eq!(ran.stopped, ip_run::Stopped::Answered);
        assert_eq!(ran.traces.len(), 1, "one turn, followed under one trace");
    }

    /// A node that runs here is told what a node of that graph may ask for, derived here.
    ///
    /// **The point of the test is that nothing told it.** The dispatch carries the node, the
    /// graph and the owner, and no instruction; this machine reads the loop backs and the branches
    /// from the plane both machines share and works out the same text the coordinator would have.
    /// A member that believed a dispatch instead could be told a node may do what its graph does
    /// not say.
    ///
    /// Answered in turn rather than per method, because the two reads are both `plane.cypher` and
    /// each wants its own table — the second being the one a scripted reply would have fed the
    /// loop backs' cap to as a condition.
    #[tokio::test]
    async fn a_dispatched_node_is_told_what_the_graph_lets_it_ask_for() {
        let upstream = Arc::new(Recording {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let plane = StubPlane::start(Replies::in_turn([
            serde_json::json!({
                "columns": ["from", "to", "cap"],
                "rows": [["triage", "intake", 3]],
            }),
            serde_json::json!({
                "columns": ["from", "to", "condition"],
                "rows": [["triage", "escalate", "the complaint names a legal risk"]],
            }),
        ]))
        .await;
        let state = super::super::harness::serving(
            store().await,
            SERVING,
            Arc::clone(&upstream) as Arc<dyn ip_gateway::upstream::Upstream>,
        )
        .with_cluster_secret(Secret::from(STUB_SECRET))
        .with_graphs(Arc::new(GraphPlane::new(plane.client())));
        let router = super::super::router(state);

        let answered = router
            .oneshot(sent(Some(STUB_SECRET), &stub_dispatch()))
            .await
            .unwrap();
        assert_eq!(answered.status(), StatusCode::OK);
        let _ = streamed(answered).await;

        let sent = upstream
            .seen
            .lock()
            .expect("the record is not poisoned")
            .clone();
        let told = sent
            .first()
            .expect("the node took a turn")
            .get("system")
            .expect("a system prompt crossed")
            .to_string();
        assert!(
            told.contains(ip_graph::LOOP_BACK_KEY) && told.contains("intake (at most 3 times)"),
            "a node dispatched here was not told what its graph lets it ask for: {told}"
        );
        assert!(
            told.contains(ip_graph::BRANCH_KEY)
                && told.contains("escalate (when the complaint names a legal risk)"),
            "a node dispatched here was not told what its graph lets it choose: {told}"
        );
    }

    /// A worker that cannot read the graph refuses the node rather than running it uninstructed.
    ///
    /// The coordinator runs a refused node at home, where it is told properly, so this degrades
    /// to a local run rather than to a node that cannot ask for what its graph declares.
    #[tokio::test]
    async fn a_worker_that_cannot_read_the_graph_refuses_the_node() {
        let state = super::super::harness::serving(store().await, SERVING, Arc::new(Answering))
            .with_cluster_secret(Secret::from(STUB_SECRET));
        let router = super::super::router(state);

        let answered = router
            .oneshot(sent(Some(STUB_SECRET), &stub_dispatch()))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let Some((_, NodeEvent::Refused(why))) = said.last() else {
            panic!("a worker with no plane ran the node anyway: {said:?}");
        };
        assert!(why.contains("keeps no graphs"), "{why}");
    }

    /// What a node said on the way is the run's own vocabulary, not a second one.
    #[tokio::test]
    async fn what_a_dispatched_node_says_is_what_a_local_one_says() {
        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &stub_dispatch()))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let turns: Vec<&RunEvent> = said
            .iter()
            .filter_map(|(_, event)| match event {
                NodeEvent::Said(event) => Some(event),
                _ => None,
            })
            .collect();
        assert!(
            turns.iter().any(|event| event.name() == "turn"),
            "no turn was reported: {said:?}"
        );
        assert!(
            !turns.iter().any(|event| event.name() == "done"),
            "a run's own done was streamed beside the node's answer: {said:?}"
        );
    }

    #[tokio::test]
    async fn a_dispatch_carrying_another_secret_is_refused() {
        let answered = worker()
            .await
            .oneshot(sent(Some("cluster-not-ours"), &stub_dispatch()))
            .await
            .unwrap();

        assert_eq!(answered.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_dispatch_carrying_no_secret_at_all_is_refused() {
        let answered = worker()
            .await
            .oneshot(sent(None, &stub_dispatch()))
            .await
            .unwrap();

        assert_eq!(answered.status(), StatusCode::UNAUTHORIZED);
    }

    /// A deployment in no cluster has no peers, so nothing may be dispatched to it — the
    /// opposite of the scrape endpoint, where setting no token asks for an open endpoint.
    #[tokio::test]
    async fn a_deployment_with_no_cluster_secret_refuses_every_dispatch() {
        let state = super::super::harness::serving(store().await, SERVING, Arc::new(Answering));
        let router = super::super::router(state);

        let answered = router
            .oneshot(sent(Some(STUB_SECRET), &stub_dispatch()))
            .await
            .unwrap();

        assert_eq!(answered.status(), StatusCode::UNAUTHORIZED);
    }

    /// The shape of the request is a status, and it is read only after the secret.
    #[tokio::test]
    async fn a_body_that_is_not_a_dispatch_is_refused_once_the_secret_has_checked_out() {
        let not_one = Request::builder()
            .method("POST")
            .uri(ip_cluster::DISPATCH_PATH)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {STUB_SECRET}"))
            .body(Body::from(r#"{"run":"nope"}"#))
            .unwrap();

        let answered = worker().await.oneshot(not_one).await.unwrap();

        assert_eq!(answered.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// An unauthenticated caller is told nothing about the shape of a dispatch, not even that it
    /// got it wrong: the secret is checked before the body is looked at.
    #[tokio::test]
    async fn a_body_that_is_not_a_dispatch_is_still_only_unauthorized_without_the_secret() {
        let not_one = Request::builder()
            .method("POST")
            .uri(ip_cluster::DISPATCH_PATH)
            .header("content-type", "application/json")
            .body(Body::from("not json at all"))
            .unwrap();

        let answered = worker().await.oneshot(not_one).await.unwrap();

        assert_eq!(answered.status(), StatusCode::UNAUTHORIZED);
    }

    /// The stream is already open by the time this machine knows, so it is an event and the
    /// coordinator reads it as a node that never started.
    #[tokio::test]
    async fn a_template_this_machine_does_not_store_is_a_refusal_rather_than_a_status() {
        let work = Dispatch {
            agent: AgentId::new("not-here").unwrap(),
            ..stub_dispatch()
        };

        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &work))
            .await
            .unwrap();

        assert_eq!(answered.status(), StatusCode::OK, "the stream had opened");
        let said = streamed(answered).await;
        let [(name, NodeEvent::Refused(why))] = said.as_slice() else {
            panic!("a missing template was not one refusal: {said:?}");
        };
        assert_eq!(name, "refused");
        assert!(why.contains("not-here"), "{why}");
    }

    /// The whole reason a dispatch names a caller rather than carrying an authority.
    #[tokio::test]
    async fn a_dispatch_naming_somebody_outside_the_tenant_is_refused() {
        let work = Dispatch {
            user: user("mallory"),
            ..stub_dispatch()
        };

        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &work))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let [(_, NodeEvent::Refused(why))] = said.as_slice() else {
            panic!("an outsider's dispatch was run: {said:?}");
        };
        assert!(why.contains("mallory") && why.contains("acme"), "{why}");
    }

    #[tokio::test]
    async fn a_dispatch_naming_an_account_this_machine_has_never_heard_of_is_refused() {
        let work = Dispatch {
            user: user("nobody"),
            ..stub_dispatch()
        };

        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &work))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let [(_, NodeEvent::Refused(why))] = said.as_slice() else {
            panic!("an unknown account's dispatch was run: {said:?}");
        };
        assert!(why.contains("nobody"), "{why}");
    }

    /// A member of the tenant who holds nothing in it. The authority is built here, so his turn
    /// is refused here — exactly as it would be if he asked this gateway by hand.
    #[tokio::test]
    async fn a_dispatch_naming_a_member_who_may_not_proxy_is_refused_the_turn() {
        let work = Dispatch {
            user: user("bob"),
            ..stub_dispatch()
        };

        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &work))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let outcome = said
            .iter()
            .find_map(|(_, event)| match event {
                NodeEvent::Refused(why) => Some(why.clone()),
                NodeEvent::Answered(ran) => Some(format!("{:?}", ran.stopped)),
                NodeEvent::Said(_) => None,
            })
            .unwrap_or_else(|| panic!("nothing came back: {said:?}"));

        assert!(
            outcome.contains("Refused"),
            "a member of the tenant holding nothing in it was served: {outcome}"
        );
    }

    /// The owner's templates, never the deployment's: a dispatch cannot name a global template
    /// for a tenant's run, because the owner it carries is what the lookup is scoped by.
    #[tokio::test]
    async fn a_dispatch_reads_the_template_under_the_owner_it_names() {
        let work = Dispatch {
            owner: GraphOwner::Global,
            ..stub_dispatch()
        };

        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &work))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let [(_, NodeEvent::Refused(why))] = said.as_slice() else {
            panic!("a tenant's template was run as the deployment's: {said:?}");
        };
        assert!(why.contains("summarise"), "{why}");
    }

    /// What the dispatch asked for reaches the run rather than being dropped on the way.
    ///
    /// The family is what can be seen from outside: this deployment serves anthropic only, so a
    /// dispatch pinning openai has nowhere to run, and could only be refused if `requested` got
    /// as far as [`Target::aimed`].
    #[tokio::test]
    async fn what_a_dispatch_asks_for_reaches_the_run_it_names() {
        let work = Dispatch {
            node: NodeName::new("reply").unwrap(),
            requested: Requested {
                model: None,
                api: Some(ip_agents::ChosenApi::OpenAi),
            },
            ..stub_dispatch()
        };

        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &work))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let [(_, NodeEvent::Refused(why))] = said.as_slice() else {
            panic!("a family nothing serves was run anyway: {said:?}");
        };
        assert!(why.contains("openai"), "{why}");
    }

    /// The fallback a stream carries when an event will not write down is itself an event, so a
    /// coordinator reads a failure here as a node that did not run.
    #[test]
    fn what_an_unwritable_event_is_streamed_as_is_itself_a_refusal() {
        assert!(matches!(
            serde_json::from_str::<NodeEvent>(UNWRITABLE),
            Ok(NodeEvent::Refused(_))
        ));
    }

    /// The name a reader sees is the tag its data carries, for every event this streams.
    #[tokio::test]
    async fn every_events_name_is_the_tag_its_data_carries() {
        let answered = worker()
            .await
            .oneshot(sent(Some(STUB_SECRET), &stub_dispatch()))
            .await
            .unwrap();

        for (name, event) in streamed(answered).await {
            assert_eq!(name, event.name());
        }
    }

    /// A worker whose turns are slow enough that a stop published mid-run can reach it.
    ///
    /// The template this fixture stores allows four turns, so a run that is never stopped takes
    /// four and a run that is stopped takes fewer — which is what the assertions read.
    struct Slowly;

    #[async_trait::async_trait]
    impl ip_gateway::upstream::Upstream for Slowly {
        async fn send(
            &self,
            _request: Request<ip_gateway::body::GatewayBody>,
        ) -> Result<axum::http::Response<ip_gateway::body::GatewayBody>, ip_gateway::UpstreamError>
        {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            // A tool call, so the loop takes another turn rather than finishing on this one.
            let body = serde_json::json!({
                "id": "msg_1",
                "model": "claude-opus-5",
                "role": "assistant",
                "type": "message",
                "stop_reason": "tool_use",
                "content": [{
                    "type": "tool_use",
                    "id": "call_1",
                    "name": "nothing",
                    "input": {},
                }],
                "usage": {"input_tokens": 10, "output_tokens": 2},
            })
            .to_string();
            Ok(axum::http::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(ip_gateway::from_bytes(body.into()))
                .unwrap())
        }
    }

    /// A worker taking its time over each turn, so a stop has somewhere to land.
    async fn slow_worker() -> (Router, AppState, StubPlane) {
        let plane = StubPlane::start(nothing_declared()).await;
        let state = super::super::harness::serving(store().await, SERVING, Arc::new(Slowly))
            .with_cluster_secret(Secret::from(STUB_SECRET))
            .with_graphs(Arc::new(GraphPlane::new(plane.client())));
        (super::super::router(state.clone()), state, plane)
    }

    /// Publishes a run's stop the way a coordinator does, through the seam that carries it.
    async fn publish_the_stop(state: &AppState, run: GraphRunId) {
        <Signalling as ip_graph::Stopping>::stopped(
            &Signalling::new(Arc::clone(state.shared_cache())),
            run,
        )
        .await;
    }

    /// The whole of commit 6: a stop said on one machine ends a node running on another.
    #[tokio::test]
    async fn a_stop_the_coordinator_published_ends_a_node_running_here() {
        let (router, state, _plane) = slow_worker().await;
        let work = stub_dispatch();
        let stopping = {
            let state = state.clone();
            let run = work.run;
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(90)).await;
                publish_the_stop(&state, run).await;
            }
        };

        let (answered, ()) = tokio::join!(router.oneshot(sent(Some(STUB_SECRET), &work)), stopping);

        let said = streamed(answered.unwrap()).await;
        let Some((_, NodeEvent::Answered(ran))) = said.last() else {
            panic!("a stopped node did not answer: {said:?}");
        };
        assert_eq!(
            ran.stopped,
            ip_run::Stopped::CutShort,
            "a node that was told to stop reported something else: {said:?}"
        );
        let turns = said
            .iter()
            .filter(|(name, _)| name == "said")
            .filter(|(_, event)| matches!(event, NodeEvent::Said(RunEvent::Turn { .. })))
            .count();
        assert!(
            (1..4).contains(&turns),
            "the node took every turn it was allowed, so nothing stopped it: {turns}"
        );
    }

    /// A stop is addressed to one run, which is what keeps one tenant's stop out of another's.
    #[tokio::test]
    async fn a_stop_published_for_another_run_does_not_touch_this_node() {
        let (router, state, _plane) = slow_worker().await;
        let other = GraphRunId::generate().expect("a run id");
        let stopping = {
            let state = state.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                publish_the_stop(&state, other).await;
            }
        };

        let (answered, ()) = tokio::join!(
            router.oneshot(sent(Some(STUB_SECRET), &stub_dispatch())),
            stopping
        );

        let said = streamed(answered.unwrap()).await;
        let Some((_, NodeEvent::Answered(ran))) = said.last() else {
            panic!("the node did not answer: {said:?}");
        };
        assert!(
            matches!(ran.stopped, ip_run::Stopped::OutOfTurns { .. }),
            "somebody else's stop ended this node: {said:?}"
        );
    }

    /// Read before the node runs, because a follower started now has nothing to wake on.
    ///
    /// Answered as a node that ran and was cut short, never refused: a refusal is what sends the
    /// coordinator off to run the node at home, which for a stopped run is the one wrong answer.
    #[tokio::test]
    async fn a_node_dispatched_after_the_stop_was_published_is_cut_short_and_never_run() {
        let (router, state, _plane) = slow_worker().await;
        let work = stub_dispatch();
        publish_the_stop(&state, work.run).await;

        let answered = router
            .oneshot(sent(Some(STUB_SECRET), &work))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let [(name, NodeEvent::Answered(ran))] = said.as_slice() else {
            panic!("a node of a stopped run said more than that it was cut short: {said:?}");
        };
        assert_eq!(name, "answered");
        assert_eq!(ran.stopped, ip_run::Stopped::CutShort);
        assert_eq!(ran.spend, Spend::NOTHING, "a stopped node spent something");
        assert!(ran.traces.is_empty(), "a stopped node took a turn");
    }

    /// A stop that cannot arrive is not a reason to refuse work — it is what every member did
    /// before the stop existed.
    #[tokio::test]
    async fn a_worker_that_cannot_follow_the_signals_runs_the_node_to_completion() {
        let plane = StubPlane::start(nothing_declared()).await;
        let state = super::super::harness::serving(store().await, SERVING, Arc::new(Answering))
            .with_cluster_secret(Secret::from(STUB_SECRET))
            .with_graphs(Arc::new(GraphPlane::new(plane.client())));
        state.cache_failure().fail_now();
        let router = super::super::router(state.clone());

        let answered = router
            .oneshot(sent(Some(STUB_SECRET), &stub_dispatch()))
            .await
            .unwrap();

        let said = streamed(answered).await;
        let Some((_, NodeEvent::Answered(ran))) = said.last() else {
            panic!("a cache that cannot answer stopped the node: {said:?}");
        };
        assert_eq!(ran.answer, "four");
        assert_eq!(ran.stopped, ip_run::Stopped::Answered);
    }
}
