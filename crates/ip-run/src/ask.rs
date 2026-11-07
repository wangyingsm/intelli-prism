//! Asking the model one turn, through the gateway the deployment already runs.
//!
//! A turn is an ordinary request as far as the gateway is concerned: it is routed, limited,
//! filtered, priced and recorded like any other. What makes it a run's turn is only that
//! nothing arrived from outside to start it, so the runner builds what a caller would have
//! sent and says whose credential it presents.

use bytes::Bytes;
use http::header::{CONTENT_TYPE, HOST};
use http::{Method, Request};
use http_body_util::BodyExt;
use ip_auth::Authority;
use ip_core::{AbsPath, Dialect, Protocol, RouteRule, TraceId};
use ip_gateway::{Gateway, Presenting, RequestContext, from_bytes};
use ip_protocol::{Conversation, Dropped, Gathered, anthropic, gemini, openai};
use std::net::SocketAddr;

use crate::choice::Pinned;
use crate::error::RunError;

/// Where a run's turns are sent, and as whom.
///
/// A run is started over the management api and has no uri of its own, so the path is a rule
/// the deployment configured — the same one a caller proxying to that family would use. It is
/// settled once for the run rather than per turn, so every turn of a run is routed alike.
#[derive(Debug, Clone)]
pub struct Target {
    /// The path the turns are routed by.
    pub path: AbsPath,
    /// The family the body is written in, which is the one that path's callers speak.
    pub speaks: Dialect,
    /// The account the run was started by, whose quota and grants the turns answer to.
    pub authority: Authority,
    /// Where the gateway listens, which is what the rule is keyed on.
    pub listen: SocketAddr,
    /// The scheme the gateway serves.
    pub protocol: Protocol,
}

impl Target {
    /// Works out where a run's turns go from what the deployment configured.
    ///
    /// A run has no uri of its own, so the rule it is routed by is chosen here rather than
    /// asked for. What settles it is the family: a body cannot be written without choosing
    /// one, so a run that pinned no api in a deployment serving several families is refused
    /// rather than sent to whichever upstream happened to be configured first. Several
    /// upstreams of the *same* family are not ambiguous in that way, and the first is taken.
    pub fn aimed(
        configured: &[RouteRule],
        pinned: &Pinned,
        as_whom: AsWhom,
    ) -> Result<Self, RunError> {
        let candidates: Vec<(&RouteRule, Dialect)> = configured
            .iter()
            .map(|rule| (rule, family(rule)))
            .filter(|(_, speaks)| pinned.api.is_none_or(|wanted| wanted == *speaks))
            .collect();
        let (first, speaks) = *candidates.first().ok_or_else(|| RunError::NowhereToRun {
            api: pinned.api.map(|api| api.to_string()),
        })?;
        if candidates.iter().any(|(_, other)| *other != speaks) {
            return Err(RunError::AmbiguousRun {
                families: {
                    let mut named: Vec<String> = candidates
                        .iter()
                        .map(|(_, speaks)| speaks.to_string())
                        .collect();
                    named.sort_unstable();
                    named.dedup();
                    named
                },
            });
        }
        Ok(Self {
            path: first.key.endpoint().path.clone(),
            speaks,
            authority: as_whom.authority,
            listen: as_whom.listen,
            protocol: as_whom.protocol,
        })
    }
}

/// Who a run's turns are sent as, and from where.
#[derive(Debug, Clone)]
pub struct AsWhom {
    /// The account the run was started by.
    pub authority: Authority,
    /// Where the gateway listens.
    pub listen: SocketAddr,
    /// The scheme it serves.
    pub protocol: Protocol,
}

/// The family one rule's callers speak, which is what the gateway itself works out.
fn family(rule: &RouteRule) -> Dialect {
    rule.caller_speaks
        .or(rule.upstream_speaks)
        .unwrap_or(Dialect::OpenAi)
}

/// What one turn came back with.
#[derive(Debug, Clone, PartialEq)]
pub struct Taken {
    /// The answer, folded back into its parts.
    pub gathered: Gathered,
    /// What the two translations could not carry, named so a run can say so.
    pub lost: Dropped,
    /// What this turn was followed under, which is how it is joined to what it cost.
    pub trace: TraceId,
}

/// Asks one turn and reads what the model answered.
///
/// The answer is asked for whole rather than streamed: a run reads the whole of a turn before
/// it can decide whether to take another, so there is nothing to do with it as it arrives.
/// Commit 13's event stream is of the run, not of any one turn.
pub async fn take(
    gateway: &Gateway,
    target: &Target,
    conversation: &Conversation,
) -> Result<Taken, RunError> {
    let (body, mut lost) = written(target.speaks, conversation);
    let asking = asked(target, &body)?;
    let trace = TraceId::generate().map_err(|error| RunError::TurnNotBuilt {
        detail: error.to_string(),
    })?;
    let context = RequestContext {
        authority: target.authority.clone(),
        protocol: target.protocol,
        listen: target.listen,
        trace,
        turn: None,
        presenting: Presenting::TheDeployments,
    };
    let answered =
        gateway
            .handle(context, asking)
            .await
            .map_err(|error| RunError::TurnRefused {
                status: error.status().as_u16(),
                detail: error.to_string(),
            })?;
    let status = answered.status();
    let bytes = answered
        .into_body()
        .collect()
        .await
        .map_err(|error| RunError::NotAnAnswer {
            detail: error.to_string(),
        })?
        .to_bytes();
    if !status.is_success() {
        return Err(RunError::TurnRefused {
            status: status.as_u16(),
            detail: String::from_utf8_lossy(&bytes).chars().take(256).collect(),
        });
    }
    let (events, dropped) = read(target.speaks, &bytes)?;
    lost.absorb(&dropped);
    Ok(Taken {
        gathered: Gathered::of(&events),
        lost,
        trace,
    })
}

/// The request a caller would have sent to say this.
fn asked(
    target: &Target,
    body: &serde_json::Value,
) -> Result<Request<ip_gateway::GatewayBody>, RunError> {
    let bytes = serde_json::to_vec(body).map_err(|error| RunError::TurnNotBuilt {
        detail: error.to_string(),
    })?;
    Request::builder()
        .method(Method::POST)
        .uri(target.path.as_str())
        // The rule is keyed on where the gateway listens, so a turn has to say it arrived there.
        .header(HOST, target.listen.to_string())
        .header(CONTENT_TYPE, "application/json")
        .body(from_bytes(Bytes::from(bytes)))
        .map_err(|error| RunError::TurnNotBuilt {
            detail: error.to_string(),
        })
}

/// The conversation as the family the turn is written in spells it.
fn written(speaks: Dialect, conversation: &Conversation) -> (serde_json::Value, Dropped) {
    match speaks {
        Dialect::OpenAi => openai::write(conversation),
        Dialect::Anthropic => anthropic::write(conversation),
        Dialect::Gemini => gemini::write(conversation),
    }
}

/// The answer as that family spelled it.
fn read(speaks: Dialect, body: &[u8]) -> Result<(Vec<ip_protocol::Event>, Dropped), RunError> {
    let read = match speaks {
        Dialect::OpenAi => openai::read_answer(body),
        Dialect::Anthropic => anthropic::read_answer(body),
        Dialect::Gemini => gemini::read_answer(body),
    };
    read.map_err(|error| RunError::NotAnAnswer {
        detail: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use ip_protocol::{Block, Message, Stop};

    use ip_core::RouteRule;

    use super::*;
    use crate::fixtures::{Fake, gateway, said, target};

    fn asking() -> Conversation {
        Conversation {
            model: None,
            system: Some("Be brief.".to_owned()),
            messages: vec![Message::text(ip_protocol::Role::User, "how many are left?")],
            tools: Vec::new(),
            sampling: ip_protocol::Sampling::default(),
            stream: false,
        }
    }

    /// A run has no caller to borrow a provider key from, so every turn says it presents the
    /// deployment's own and is aimed at the endpoint the matched rule names.
    fn ruled(api: &str, path: &str, caller: Option<Dialect>) -> RouteRule {
        RouteRule {
            api: ip_core::ApiId::new(api).unwrap(),
            key: ip_core::RouteKey::new(ip_core::Endpoint::new(
                Protocol::Http,
                ip_core::Host::new("127.0.0.1").unwrap(),
                ip_core::Port::new(8080).unwrap(),
                AbsPath::new(path).unwrap(),
            )),
            target: ip_core::RouteTarget::new(ip_core::Endpoint::new(
                Protocol::Https,
                ip_core::Host::new("api.example.com").unwrap(),
                ip_core::Port::new(443).unwrap(),
                AbsPath::new("/v1").unwrap(),
            )),
            strategy: ip_core::Strategy::default(),
            hedge: ip_core::Hedge::default(),
            hedge_delay: ip_core::HedgeDelay::default(),
            caller_speaks: caller,
            upstream_speaks: None,
        }
    }

    fn as_whom() -> AsWhom {
        let aimed = target(Dialect::Anthropic);
        AsWhom {
            authority: aimed.authority,
            listen: aimed.listen,
            protocol: aimed.protocol,
        }
    }

    #[test]
    fn one_upstream_is_where_a_run_that_pinned_nothing_goes() {
        let aimed = Target::aimed(
            &[ruled("anthropic", "/anthropic", Some(Dialect::Anthropic))],
            &Pinned::default(),
            as_whom(),
        )
        .unwrap();

        assert_eq!(aimed.path.as_str(), "/anthropic");
        assert_eq!(aimed.speaks, Dialect::Anthropic);
    }

    /// A rule that says nothing about families is what every rule written before M14 is.
    #[test]
    fn a_rule_naming_no_family_is_read_the_way_the_gateway_reads_it() {
        let aimed = Target::aimed(
            &[ruled("openai", "/v1", None)],
            &Pinned::default(),
            as_whom(),
        )
        .unwrap();

        assert_eq!(aimed.speaks, Dialect::OpenAi);
    }

    #[test]
    fn a_pinned_family_picks_the_upstream_that_speaks_it() {
        let configured = [
            ruled("openai", "/openai", Some(Dialect::OpenAi)),
            ruled("anthropic", "/anthropic", Some(Dialect::Anthropic)),
        ];

        let aimed = Target::aimed(
            &configured,
            &Pinned {
                model: None,
                api: Some(Dialect::Anthropic),
            },
            as_whom(),
        )
        .unwrap();

        assert_eq!(aimed.path.as_str(), "/anthropic");
        assert_eq!(aimed.speaks, Dialect::Anthropic);
    }

    /// The decision this exists to make: a body has to be written in one family, so a run
    /// that named none where several are served is refused rather than sent to the first.
    #[test]
    fn a_run_naming_no_family_where_several_are_served_is_refused() {
        let configured = [
            ruled("openai", "/openai", Some(Dialect::OpenAi)),
            ruled("anthropic", "/anthropic", Some(Dialect::Anthropic)),
        ];

        let refused = Target::aimed(&configured, &Pinned::default(), as_whom()).unwrap_err();

        assert!(
            matches!(&refused, RunError::AmbiguousRun { families } if families == &["anthropic", "openai"]),
            "{refused:?}"
        );
    }

    /// Two upstreams of one family are not ambiguous in the way that matters.
    #[test]
    fn several_upstreams_of_one_family_are_not_ambiguous() {
        let configured = [
            ruled("anthropic-eu", "/eu", Some(Dialect::Anthropic)),
            ruled("anthropic-us", "/us", Some(Dialect::Anthropic)),
        ];

        let aimed = Target::aimed(&configured, &Pinned::default(), as_whom()).unwrap();

        assert_eq!(aimed.path.as_str(), "/eu", "the first the file declared");
    }

    #[test]
    fn a_family_no_upstream_speaks_refuses_the_run() {
        let refused = Target::aimed(
            &[ruled("openai", "/openai", Some(Dialect::OpenAi))],
            &Pinned {
                model: None,
                api: Some(Dialect::Gemini),
            },
            as_whom(),
        )
        .unwrap_err();

        assert!(
            matches!(&refused, RunError::NowhereToRun { api } if api.as_deref() == Some("gemini")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_deployment_with_no_upstreams_at_all_refuses_the_run() {
        let refused = Target::aimed(&[], &Pinned::default(), as_whom()).unwrap_err();

        assert!(
            matches!(&refused, RunError::NowhereToRun { api: None }),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn a_turn_goes_out_as_the_deployments_own_request() {
        let upstream = Fake::answering(&said("four"));

        take(
            &gateway(upstream.clone()),
            &target(Dialect::Anthropic),
            &asking(),
        )
        .await
        .unwrap();

        let seen = upstream.seen.lock().unwrap();
        let (uri, headers, _) = seen.last().unwrap();
        assert_eq!(uri, "https://api.example.com:443/v1/messages");
        assert_eq!(
            headers[http::header::AUTHORIZATION].to_str().unwrap(),
            "Bearer sk-deployment"
        );
    }

    #[tokio::test]
    async fn a_turn_is_written_in_the_family_the_target_speaks() {
        let upstream = Fake::answering(&said("four"));

        take(
            &gateway(upstream.clone()),
            &target(Dialect::Anthropic),
            &asking(),
        )
        .await
        .unwrap();

        let sent = upstream.sent();
        assert_eq!(
            sent["system"], "Be brief.",
            "anthropic carries it at the top"
        );
        assert_eq!(sent["messages"][0]["role"], "user");
    }

    /// The same conversation, spelled by whichever family the run was pinned to.
    #[tokio::test]
    async fn the_same_conversation_is_spelled_differently_for_another_family() {
        let upstream = Fake::answering(
            r#"{"choices":[{"message":{"role":"assistant","content":"four"},"finish_reason":"stop"}]}"#,
        );

        take(
            &gateway(upstream.clone()),
            &target(Dialect::OpenAi),
            &asking(),
        )
        .await
        .unwrap();

        let sent = upstream.sent();
        assert!(
            sent.get("system").is_none(),
            "openai has no top-level system"
        );
        assert_eq!(
            sent["messages"][0]["role"], "system",
            "it becomes the first message instead"
        );
    }

    #[tokio::test]
    async fn what_the_model_answered_comes_back_gathered() {
        let upstream = Fake::answering(&said("four"));

        let taken = take(&gateway(upstream), &target(Dialect::Anthropic), &asking())
            .await
            .unwrap();

        assert_eq!(taken.gathered.text, "four");
        assert_eq!(taken.gathered.stop, Some(Stop::End));
        assert_eq!(taken.gathered.spent_or_zero().total(), 11);
        assert!(taken.gathered.calls.is_empty());
    }

    /// The branch the loop turns on: a turn asking for a tool comes back with its calls.
    #[tokio::test]
    async fn a_turn_that_wants_a_tool_comes_back_with_the_call() {
        let upstream = Fake::answering(
            r#"{
                "id": "msg_2",
                "model": "claude-opus-5",
                "role": "assistant",
                "stop_reason": "tool_use",
                "content": [
                    {"type": "text", "text": "looking"},
                    {"type": "tool_use", "id": "call_1", "name": "warehouse__sql",
                     "input": {"query": "select 1"}}
                ],
                "usage": {"input_tokens": 9, "output_tokens": 7}
            }"#,
        );

        let taken = take(&gateway(upstream), &target(Dialect::Anthropic), &asking())
            .await
            .unwrap();

        assert_eq!(taken.gathered.stop, Some(Stop::ToolUse));
        assert_eq!(taken.gathered.text, "looking");
        assert_eq!(taken.gathered.calls.len(), 1);
        assert!(
            matches!(
                &taken.gathered.calls[0],
                ip_protocol::Event::ToolCall { id, name, arguments }
                    if id.as_str() == "call_1"
                        && name.as_str() == "warehouse__sql"
                        && arguments["query"] == "select 1"
            ),
            "{:?}",
            taken.gathered.calls[0]
        );
    }

    /// A tool result the loop appended is carried out as that family spells one.
    #[tokio::test]
    async fn a_tool_result_the_run_appended_goes_out_with_the_turn() {
        let upstream = Fake::answering(&said("four"));
        let mut conversation = asking();
        conversation.messages.push(Message {
            role: ip_protocol::Role::User,
            blocks: vec![Block::ToolResult {
                id: ip_protocol::ToolCallId::new("call_1").unwrap(),
                content: "four".to_owned(),
            }],
        });

        take(
            &gateway(upstream.clone()),
            &target(Dialect::Anthropic),
            &conversation,
        )
        .await
        .unwrap();

        let sent = upstream.sent();
        let last = sent["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "call_1");
    }

    /// The gateway refusing is the run ending, not a turn to try again: what it refuses it
    /// would refuse an identical turn for too.
    #[tokio::test]
    async fn a_turn_the_gateway_will_not_carry_ends_the_run() {
        let upstream = Fake::answering(&said("four"));
        let mut nowhere = target(Dialect::Anthropic);
        nowhere.path = AbsPath::new("/nothing/claims/this").unwrap();

        let refused = take(&gateway(upstream), &nowhere, &asking())
            .await
            .unwrap_err();

        assert!(
            matches!(refused, RunError::TurnRefused { status: 404, .. }),
            "{refused:?}"
        );
    }

    /// What the upstream answers is passed through by the gateway, so a provider's own
    /// refusal reaches the run as the status it sent.
    #[tokio::test]
    async fn an_upstream_that_refuses_is_reported_with_its_own_status() {
        let upstream = Fake::holding(
            http::StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"slow down"}}"#,
        );

        let refused = take(&gateway(upstream), &target(Dialect::Anthropic), &asking())
            .await
            .unwrap_err();

        assert!(
            matches!(&refused, RunError::TurnRefused { status: 429, detail } if detail.contains("slow down")),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_that_is_not_one_is_refused_rather_than_read_as_empty() {
        let upstream = Fake::answering("this is not json");

        let refused = take(&gateway(upstream), &target(Dialect::Anthropic), &asking())
            .await
            .unwrap_err();

        assert!(
            matches!(refused, RunError::NotAnAnswer { .. }),
            "{refused:?}"
        );
    }
}
