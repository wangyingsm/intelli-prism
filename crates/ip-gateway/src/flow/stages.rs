//! The typestate machine: a request as it moves from one stage to the next.

use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{Request, Response};
use ip_cache::{CacheKey, Ttl};
use ip_core::{Capability, CapabilityScope, RouteKey};
use std::sync::Arc;
use std::time::SystemTime;

use super::RequestContext;
use super::headers::{authority_of, is_event_stream, rewrite, set_length, strip_hop_by_hop};
use super::run::{collect, run_body, run_headers};
use crate::body::{GatewayBody, from_bytes};
use crate::cache::{CachedResponse, Freshness, freshness, response_key};
use crate::error::{GatewayError, GatewayErrorKind};
use crate::processor::{HeaderProcessor, ProcessorChain};
use crate::sse::stream_chunks;
use crate::stage::{
    Authorized, BodyProcessed, Forwarded, HeadersProcessed, Received, ResponseBodyProcessed,
    ResponseHeadersProcessed, Routed, Stage, StageName,
};
use crate::table::{Resolution, RoutingTable, request_key};
use crate::upstream::Upstream;

/// A request part way through the dataflow, with the stage it reached in its type.
///
/// Each transition is implemented only on the stage before it, so a step cannot be
/// skipped or reordered: the call that would do so does not compile.
///
/// Stages run in order:
///
/// ```
/// # use ip_gateway::{Flow, ProcessorChain, RoutingTable, RequestContext};
/// # use ip_gateway::body::empty;
/// # use ip_auth::{Authority, Identity};
/// # use ip_core::{Grants, Protocol, Role, TenantId, TraceId, UserId};
/// # use http::Request;
/// # fn context() -> RequestContext {
/// #     RequestContext {
/// #         authority: Authority::new(
/// #             Identity {
/// #                 user: UserId::new("alice").unwrap(),
/// #                 tenant: TenantId::new("acme").unwrap(),
/// #                 role: Role::Member,
/// #             },
/// #             Grants::new(),
/// #         ),
/// #         protocol: Protocol::Http,
/// #         listen: "127.0.0.1:8080".parse().unwrap(),
/// #         trace: TraceId::generate().unwrap(),
/// #         turn: None,
/// #     }
/// # }
/// # async fn run(table: &RoutingTable, chain: &ProcessorChain) {
/// let request = Request::builder().uri("/x").body(empty()).unwrap();
/// let flow = Flow::received(context(), request);
/// let flow = flow.authorize(table).unwrap();
/// let flow = flow.process_headers(chain.request_headers()).await.unwrap();
/// # }
/// ```
///
/// Running a plugin before authorization does not compile:
///
/// ```compile_fail
/// # use ip_gateway::{Flow, ProcessorChain, RoutingTable, RequestContext};
/// # use ip_gateway::body::empty;
/// # use ip_auth::{Authority, Identity};
/// # use ip_core::{Grants, Protocol, Role, TenantId, TraceId, UserId};
/// # use http::Request;
/// # fn context() -> RequestContext {
/// #     RequestContext {
/// #         authority: Authority::new(
/// #             Identity {
/// #                 user: UserId::new("alice").unwrap(),
/// #                 tenant: TenantId::new("acme").unwrap(),
/// #                 role: Role::Member,
/// #             },
/// #             Grants::new(),
/// #         ),
/// #         protocol: Protocol::Http,
/// #         listen: "127.0.0.1:8080".parse().unwrap(),
/// #         trace: TraceId::generate().unwrap(),
/// #         turn: None,
/// #     }
/// # }
/// # async fn run(table: &RoutingTable, chain: &ProcessorChain) {
/// let request = Request::builder().uri("/x").body(empty()).unwrap();
/// let flow = Flow::received(context(), request);
/// // process_headers belongs to Flow<Authorized>, so no plugin runs before authorization.
/// let flow = flow.process_headers(chain.request_headers()).await.unwrap();
/// # }
/// ```
pub struct Flow<S: Stage> {
    context: RequestContext,
    held: S::Held,
}

impl<S: Stage> Flow<S> {
    /// Who the caller is, and where the request arrived.
    pub fn context(&self) -> &RequestContext {
        &self.context
    }

    /// The stage this flow has reached.
    pub fn stage(&self) -> StageName {
        S::NAME
    }
}

impl Flow<Received> {
    /// Takes a request that has been authenticated but not yet processed.
    pub fn received(context: RequestContext, request: Request<GatewayBody>) -> Self {
        Self {
            context,
            held: request,
        }
    }

    /// Resolves the route and checks the caller may use it, before any plugin runs.
    pub fn authorize(self, table: &RoutingTable) -> Result<Flow<Authorized>, GatewayError> {
        let key = key_of(&self.context, &self.held)?;
        let resolution = resolve(table, &key)?;
        allow(&self.context, &resolution)?;
        Ok(Flow {
            context: self.context,
            held: Routed {
                request: self.held,
                resolution,
            },
        })
    }
}

impl Flow<Authorized> {
    /// Where the request is going, and the rule that sent it there.
    pub fn resolution(&self) -> &Resolution {
        &self.held.resolution
    }

    /// Runs the request header chain.
    pub async fn process_headers(
        mut self,
        chain: &[Arc<dyn HeaderProcessor>],
    ) -> Result<Flow<HeadersProcessed>, GatewayError> {
        run_headers(
            HeadersProcessed::NAME,
            chain,
            self.held.request.headers_mut(),
        )
        .await?;
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<HeadersProcessed> {
    /// Runs the request body chain, or leaves the body untouched when there is none.
    pub async fn process_body(
        mut self,
        processors: &ProcessorChain,
    ) -> Result<Flow<BodyProcessed>, GatewayError> {
        let body = run_body(
            BodyProcessed::NAME,
            processors.request_body(),
            self.held.request.body_mut(),
        )
        .await?;
        if let Some(bytes) = body {
            set_length(self.held.request.headers_mut(), bytes.len());
            *self.held.request.body_mut() = from_bytes(bytes);
        }
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<BodyProcessed> {
    /// Names this request in the response cache, buffering the body it is keyed by so the
    /// request can still be forwarded.
    pub async fn response_key(&mut self) -> Result<CacheKey, GatewayError> {
        let route = key_of(&self.context, &self.held.request)?;
        let body = std::mem::replace(self.held.request.body_mut(), crate::body::empty());
        let bytes = collect(StageName::BodyRead, body).await?;
        *self.held.request.body_mut() = from_bytes(bytes.clone());
        response_key(&route, &self.context.authority.identity().tenant, &bytes)
            .map_err(|error| GatewayError::new(BodyProcessed::NAME, error.into()))
    }

    /// Sends the request to the endpoint the rule chose.
    pub async fn forward(self, upstream: &dyn Upstream) -> Result<Flow<Forwarded>, GatewayError> {
        let Routed {
            request,
            resolution,
        } = self.held;
        let request = rewrite(request, resolution.primary())?;
        let mut response = upstream
            .send(request)
            .await
            .map_err(|error| GatewayError::new(Forwarded::NAME, error.into()))?;
        strip_hop_by_hop(response.headers_mut());
        Ok(Flow {
            context: self.context,
            held: response,
        })
    }
}

impl Flow<Forwarded> {
    /// Runs the response header chain.
    pub async fn process_response_headers(
        mut self,
        chain: &[Arc<dyn HeaderProcessor>],
    ) -> Result<Flow<ResponseHeadersProcessed>, GatewayError> {
        run_headers(
            ResponseHeadersProcessed::NAME,
            chain,
            self.held.headers_mut(),
        )
        .await?;
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<ResponseHeadersProcessed> {
    /// Runs the response body chain, or leaves the body untouched when there is none.
    pub async fn process_response_body(
        mut self,
        processors: &ProcessorChain,
    ) -> Result<Flow<ResponseBodyProcessed>, GatewayError> {
        if is_event_stream(self.held.headers()) && !processors.response_chunk().is_empty() {
            self.held.headers_mut().remove(CONTENT_LENGTH);
            let body = std::mem::replace(self.held.body_mut(), crate::body::empty());
            *self.held.body_mut() = stream_chunks(body, processors.response_chunk().to_vec());
            return Ok(Flow {
                context: self.context,
                held: self.held,
            });
        }
        let body = run_body(
            ResponseBodyProcessed::NAME,
            processors.response_body(),
            self.held.body_mut(),
        )
        .await?;
        if let Some(bytes) = body {
            set_length(self.held.headers_mut(), bytes.len());
            *self.held.body_mut() = from_bytes(bytes);
        }
        Ok(Flow {
            context: self.context,
            held: self.held,
        })
    }
}

impl Flow<ResponseBodyProcessed> {
    /// The answer to keep and how long to keep it, buffering the body so it can still be sent,
    /// or nothing when this response may not be kept at all.
    ///
    /// The response's own expiration headers outrank `default`, which stands when it names none.
    pub async fn cacheable(
        &mut self,
        default: Ttl,
    ) -> Result<Option<(CachedResponse, Ttl)>, GatewayError> {
        // A stream is answered as it arrives and an error is not an answer to repeat.
        if !self.held.status().is_success() || is_event_stream(self.held.headers()) {
            return Ok(None);
        }
        let ttl = match freshness(self.held.headers(), SystemTime::now()) {
            Freshness::For(ttl) => ttl,
            Freshness::Unsaid => default,
            Freshness::Never => return Ok(None),
        };
        let body = std::mem::replace(self.held.body_mut(), crate::body::empty());
        let bytes = collect(StageName::ResponseBodyRead, body).await?;
        *self.held.body_mut() = from_bytes(bytes.clone());
        let content_type = self.held.headers().get(CONTENT_TYPE).cloned();
        Ok(Some((CachedResponse::new(content_type, bytes), ttl)))
    }

    /// Hands the response back to the kernel.
    pub fn into_response(self) -> Result<Response<GatewayBody>, GatewayError> {
        Ok(self.held)
    }
}

fn key_of(
    context: &RequestContext,
    request: &Request<GatewayBody>,
) -> Result<RouteKey, GatewayError> {
    let host = authority_of(request, context.listen);
    request_key(
        context.protocol,
        &host,
        context.listen,
        request.uri().path(),
    )
    .map_err(|error| GatewayError::new(StageName::HeaderRead, error.into()))
}

fn resolve(table: &RoutingTable, key: &RouteKey) -> Result<Resolution, GatewayError> {
    let resolution = table.resolve(key).ok_or_else(|| {
        GatewayError::new(
            Authorized::NAME,
            GatewayErrorKind::NoRoute { key: key.clone() },
        )
    })?;
    if let Some(target) = resolution
        .targets()
        .iter()
        .find(|target| !target.protocol.is_forwarded())
    {
        return Err(GatewayError::new(
            Authorized::NAME,
            GatewayErrorKind::ProtocolNotServed {
                protocol: target.protocol,
            },
        ));
    }
    Ok(resolution)
}

fn allow(context: &RequestContext, resolution: &Resolution) -> Result<(), GatewayError> {
    let identity = context.authority.identity();
    let scope = CapabilityScope::Api {
        user: identity.user.clone(),
        tenant: identity.tenant.clone(),
        api: resolution.rule().api.clone(),
    };
    if context.authority.allows(Capability::ApiAccess, &scope) {
        return Ok(());
    }
    Err(GatewayError::new(
        Authorized::NAME,
        GatewayErrorKind::Forbidden {
            capability: Capability::ApiAccess,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::super::Gateway;
    use super::super::fixtures::*;
    use super::*;
    use crate::processor::ProcessorChain;
    use crate::stage::StageName;
    use crate::table::RoutingTable;
    use http::StatusCode;
    use ip_core::Protocol;

    #[tokio::test]
    async fn a_routed_request_reaches_the_target_with_its_remainder_and_query() {
        let upstream = FakeUpstream::answering("pong");
        let gateway = Gateway::new(table(), ProcessorChain::new(), upstream.clone());
        let response = gateway
            .handle(
                context(granted()),
                request("/anthropic/messages?beta=1", ""),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "pong");
        assert_eq!(
            upstream.last().uri,
            "https://api.example.com:443/v1/messages?beta=1"
        );
    }

    #[tokio::test]
    async fn a_path_no_rule_carries_is_not_found() {
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        );
        let error = gateway
            .handle(context(granted()), request("/elsewhere", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
        assert_eq!(error.stage(), StageName::Authorization);
    }

    #[tokio::test]
    async fn a_caller_holding_no_api_access_is_refused() {
        let gateway = Gateway::new(
            table(),
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        );
        let error = gateway
            .handle(context(ungranted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert!(matches!(
            error.kind(),
            GatewayErrorKind::Forbidden {
                capability: Capability::ApiAccess
            }
        ));
    }

    #[tokio::test]
    async fn a_protocol_this_build_does_not_carry_is_not_implemented() {
        let table = RoutingTable::from_rules(vec![rule(endpoint(
            Protocol::Ws,
            "api.example.com",
            443,
            "/v1",
        ))]);
        let gateway = Gateway::new(
            table,
            ProcessorChain::new(),
            FakeUpstream::answering("pong"),
        );
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn an_upstream_that_will_not_answer_is_a_bad_gateway() {
        let gateway = Gateway::new(table(), ProcessorChain::new(), FakeUpstream::failing());
        let error = gateway
            .handle(context(granted()), request("/anthropic", ""))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.stage(), StageName::Route);
    }

    #[tokio::test]
    async fn each_stage_names_itself_as_the_flow_advances() {
        let request = request("/anthropic", "");
        let flow = Flow::received(context(granted()), request);
        assert_eq!(flow.stage(), StageName::Receive);
        let flow = flow.authorize(&table()).unwrap();
        assert_eq!(flow.stage(), StageName::Authorization);
        let processors = ProcessorChain::new();
        let flow = flow
            .process_headers(processors.request_headers())
            .await
            .unwrap();
        assert_eq!(flow.stage(), StageName::HeaderProcess);
        let flow = flow.process_body(&processors).await.unwrap();
        assert_eq!(flow.stage(), StageName::BodyProcess);
        let upstream = FakeUpstream::answering("pong");
        let flow = flow.forward(upstream.as_ref()).await.unwrap();
        assert_eq!(flow.stage(), StageName::Route);
        let flow = flow
            .process_response_headers(processors.response_headers())
            .await
            .unwrap();
        assert_eq!(flow.stage(), StageName::ResponseHeaderProcess);
        let flow = flow.process_response_body(&processors).await.unwrap();
        assert_eq!(flow.stage(), StageName::ResponseBodyProcess);
        assert_eq!(body_of(flow.into_response().unwrap()).await, "pong");
    }
}
