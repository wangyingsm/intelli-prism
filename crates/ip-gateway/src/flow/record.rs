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
use ip_core::{ApiId, Latency, Served, TenantId, Tokens, TraceId, TurnId, UserId};
use ip_storage::{NewUsage, UsageStore};

use tokio::sync::Notify;

use super::followed::Followed;
use crate::body::{BoxError, GatewayBody};
use crate::limit::{Asking, Limiter, Limits};
use crate::measure::{Measuring, measure};

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
    fn writing(&self, work: impl Future<Output = ()> + Send + 'static) {
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
    pub(super) fn out_of_the_cache(self, body: &Bytes) {
        let model = measure(body).and_then(|measured| measured.model);
        self.write(model, Tokens::ZERO, Served::Cache);
    }

    /// Measures an answer as it goes out, recording what it cost once the last of it has.
    pub(super) fn measuring(self, body: GatewayBody) -> GatewayBody {
        Measured {
            inner: body,
            measuring: Measuring::default(),
            recording: Some(self),
        }
        .boxed_unsync()
    }

    /// Writes the row, off the path of the request it belongs to.
    fn write(self, model: Option<ip_core::ModelName>, tokens: Tokens, served: Served) {
        let usage = NewUsage {
            trace: self.trace,
            turn: self.turn,
            tenant: self.tenant,
            user: self.user,
            api: self.api,
            model,
            tokens,
            served,
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

/// A body that measures what passes, and records it when the last of it has.
struct Measured {
    inner: GatewayBody,
    measuring: Measuring,
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
        recording.write(model, tokens, Served::Upstream);
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
