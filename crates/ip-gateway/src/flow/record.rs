//! Recording what a request cost, once its answer is done with.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use ip_core::{ApiId, Hedge, Latency, Replica, Served, TenantId, Tokens, TraceId, TurnId, UserId};
use ip_storage::{NewUsage, UsageStore};

use tokio::sync::Notify;

use super::followed::Followed;
use crate::body::{BoxError, GatewayBody};
use crate::limit::{Asking, Limiter, Limits};
use crate::measure::{Measuring, measure};
use crate::stage::Dispatched;

/// The records still being written, so a shutdown can wait for them.
///
/// A record is written off the request path, which means the process could otherwise end
/// between the last answer and the row for it.
#[derive(Clone, Default)]
pub struct Pending {
    in_flight: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

impl Pending {
    /// Runs `work` off the request path, counted so a shutdown can wait for it.
    pub(crate) fn writing(&self, work: impl Future<Output = ()> + Send + 'static) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        let counted = self.clone();
        tokio::spawn(async move {
            work.await;
            if counted.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
                counted.idle.notify_waiters();
            }
        });
    }

    /// Returns once nothing is left to write.
    pub async fn settled(&self) {
        while self.in_flight.load(Ordering::SeqCst) > 0 {
            // Arming the wait and reading the count are the two halves of a store and a load
            // the last write races with, which only a sequentially consistent pair orders.
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                break;
            }
            idle.await;
        }
    }
}

/// What a request's tokens are counted towards once its answer is done with.
#[derive(Clone)]
pub(super) struct Limiting {
    limiter: Arc<Limiter>,
    limits: Limits,
    asking: Asking,
}

impl Limiting {
    /// What this request spends counts against these limits, for this scope.
    pub(super) fn new(limiter: Arc<Limiter>, limits: Limits, asking: Asking) -> Self {
        Self {
            limiter,
            limits,
            asking,
        }
    }
}

/// What is known about a request before its answer is, and what writes the row when it ends.
///
/// Cloneable so that one identity can be recorded twice: the request, and the arbitrating
/// call this request caused (see [`Recording::arbitrated`]).
#[derive(Clone)]
pub(super) struct Recording {
    store: Arc<dyn UsageStore>,
    pending: Pending,
    limiting: Option<Limiting>,
    started: Instant,
    trace: TraceId,
    turn: Option<TurnId>,
    tenant: TenantId,
    user: UserId,
    api: ApiId,
}

impl Recording {
    /// What will record this request, now that the route says which api serves it.
    pub(super) fn of(
        store: &Arc<dyn UsageStore>,
        pending: &Pending,
        followed: &Followed,
        started: Instant,
        limiting: Option<Limiting>,
    ) -> Option<Self> {
        Some(Self {
            store: Arc::clone(store),
            pending: pending.clone(),
            limiting,
            started,
            trace: followed.trace,
            turn: followed.turn.clone(),
            tenant: followed.tenant.clone(),
            user: followed.user.clone(),
            api: followed.api.clone()?,
        })
    }

    /// Records an answer the cache gave: the model it would have come from, and nothing spent.
    ///
    /// No endpoint is named, because none was reached; nothing was raced either, so the row
    /// says of it what it says of every answer nothing asked of an endpoint: one endpoint.
    /// The class is kept all the same, and so is the level that named it: who answered
    /// says nothing about how hard the request was treated as, or about who said so.
    pub(super) fn out_of_the_cache(self, body: &Bytes, dispatched: Dispatched) {
        let model = measure(body).and_then(|measured| measured.model);
        self.write(
            model,
            Tokens::ZERO,
            Serving {
                source: Served::Cache,
                endpoint: None,
                arms: Hedge::default(),
                dispatched,
            },
        );
    }

    /// Measures an answer as it goes out, recording what it cost once the last of it has.
    pub(super) fn measuring(
        self,
        body: GatewayBody,
        served_by: Replica,
        raced: Hedge,
        dispatched: Dispatched,
    ) -> GatewayBody {
        Measured {
            inner: body,
            measuring: Measuring::default(),
            served_by: Some(served_by),
            raced,
            dispatched,
            recording: Some(self),
        }
        .boxed_unsync()
    }

    /// Writes the arbitrating call as a row of its own: the identity is the one that
    /// caused it — trace, tenant and user are the caller's, and the turn only travels
    /// when the caller marked one, which an arbitrating opening by definition did not:
    /// the trace is what pairs the two rows. Nothing else is shared: the row travels
    /// `api`'s route, reaches `endpoint`, began at `began` and bills as `asked` unless
    /// the answer echoed a model of its own. `bytes` is the answer, measured not parsed.
    ///
    /// The spend policy is chosen, not inherited: an arbitrated row carries the
    /// caller's limiting, so its tokens count against the caller's token limits —
    /// the caller caused the spend and pays for it as they would have paid for it
    /// had they asked the model directly.
    pub(super) fn arbitrated(
        mut self,
        api: ApiId,
        endpoint: &ip_core::Endpoint,
        began: Instant,
        asked: ip_core::ModelName,
        bytes: &[u8],
    ) {
        let mut measuring = Measuring::default();
        measuring.read(bytes);
        let measured = measuring.measured();
        self.api = api;
        self.started = began;
        self.write(
            measured
                .as_ref()
                .and_then(|measured| measured.model.clone())
                .or(Some(asked)),
            measured.map_or(Tokens::ZERO, |measured| measured.tokens),
            Serving {
                source: Served::Upstream,
                endpoint: Some(Replica::of(endpoint)),
                arms: Hedge::default(),
                dispatched: Dispatched::NONE,
            },
        );
    }

    /// Writes the row, off the path of the request it belongs to.
    fn write(self, model: Option<ip_core::ModelName>, tokens: Tokens, serving: Serving) {
        let usage = NewUsage {
            trace: self.trace,
            turn: self.turn,
            tenant: self.tenant,
            user: self.user,
            api: self.api,
            model,
            tokens,
            served: serving.source,
            served_by: serving.endpoint,
            raced_across: serving.arms,
            dispatched_class: serving.dispatched.class(),
            decided_by: serving.dispatched.decided_by(),
            // Filled where the filter runs, which is not here yet.
            redacted: ip_core::Redacted::NONE,
            latency: Latency::from_millis(self.started.elapsed().as_millis()),
        };
        let store = self.store;
        let limiting = self.limiting;
        let spent = tokens.total();
        // A caller never waits on the record of what it already has, and never fails for it.
        self.pending.writing(async move {
            if let Err(error) = store.record_usage(usage).await {
                tracing::warn!(%error, "could not record what a request cost");
            }
            if let Some(limiting) = limiting {
                limiting
                    .limiter
                    .spend(&limiting.limits, &limiting.asking, spent)
                    .await;
            }
        });
    }
}

/// How the request was served, which together is what a row keeps of it past its request.
struct Serving {
    /// Where the answer came from.
    source: Served,
    /// Which endpoint answered, when one did.
    endpoint: Option<Replica>,
    /// How many endpoints it was raced across; one is what it was when nothing was reached and
    /// when it was not raced at all.
    arms: Hedge,
    /// What class it was dispatched at and which level named it: the request's own, kept
    /// by a cache answer too, since who answered says nothing about how hard it was
    /// treated as, or about who decided that.
    dispatched: Dispatched,
}

/// A body that measures what passes, and records it when the last of it has.
struct Measured {
    inner: GatewayBody,
    measuring: Measuring,
    served_by: Option<Replica>,
    raced: Hedge,
    dispatched: Dispatched,
    recording: Option<Recording>,
}

impl Measured {
    /// Records what was measured, once, whether the answer ran out or was abandoned.
    fn done(&mut self) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        let measured = std::mem::take(&mut self.measuring).measured();
        let (model, tokens) = match measured {
            Some(measured) => (measured.model, measured.tokens),
            None => (None, Tokens::ZERO),
        };
        recording.write(
            model,
            tokens,
            Serving {
                source: Served::Upstream,
                endpoint: self.served_by.take(),
                arms: self.raced,
                dispatched: self.dispatched,
            },
        );
    }
}

impl Body for Measured {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.measuring.read(data);
                }
            }
            Poll::Ready(None) => this.done(),
            _ => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Measured {
    fn drop(&mut self) {
        self.done();
    }
}
