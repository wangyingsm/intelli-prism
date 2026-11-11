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
use ip_core::{
    ApiId, Hedge, Latency, Money, Price, Replica, Served, TenantId, Tokens, TraceId, TurnId, UserId,
};
use ip_storage::{NewUsage, UsageStore};

use tokio::sync::Notify;

use super::followed::Followed;
use crate::body::{BoxError, GatewayBody};
use crate::filter::{Filtered, Reported};
use crate::limit::{Asking, Limiter, Limits, Spent};
use crate::measure::{Measuring, measure};
use crate::meter::Meters;
use crate::price::Prices;
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

/// What the rules in force say about a request's spending: what it counts against, and
/// what it costs.
///
/// A composite because both come off the same loaded routing, and because `Recording::of`
/// already took five arguments.
pub(super) struct Ruled {
    limiting: Option<Limiting>,
    prices: Prices,
}

impl Ruled {
    /// The limits this request spends against, and the prices it is costed at.
    pub(super) fn new(limiting: Option<Limiting>, prices: Prices) -> Self {
        Self { limiting, prices }
    }
}

/// Where what a request cost is kept: the rows, the dashboard, or both.
///
/// A composite because `Recording::of` already took five arguments, and because the
/// two destinations are one decision — a request is worth describing if *either* of
/// them is listening, and the description is identical for both.
#[derive(Clone, Default)]
pub(super) struct Keeping {
    store: Option<Arc<dyn UsageStore>>,
    meters: Option<Arc<Meters>>,
}

impl Keeping {
    /// What a gateway keeps, which may be neither.
    pub(super) fn new(store: Option<Arc<dyn UsageStore>>, meters: Option<Arc<Meters>>) -> Self {
        Self { store, meters }
    }

    /// Whether anything is listening at all. Nothing is described when nothing is.
    fn any(&self) -> bool {
        self.store.is_some() || self.meters.is_some()
    }

    /// The instruments, so one half can be replaced without disturbing the other.
    pub(super) fn meters(&self) -> Option<Arc<Meters>> {
        self.meters.clone()
    }

    /// The store, likewise.
    pub(super) fn store(&self) -> Option<Arc<dyn UsageStore>> {
        self.store.clone()
    }
}

/// What is known about a request before its answer is, and what writes the row when it ends.
///
/// Cloneable so that one identity can be recorded twice: the request, and the arbitrating
/// call this request caused (see [`Recording::arbitrated`]).
#[derive(Clone)]
pub(super) struct Recording {
    keeping: Keeping,
    pending: Pending,
    limiting: Option<Limiting>,
    prices: Prices,
    started: Instant,
    trace: TraceId,
    turn: Option<TurnId>,
    tenant: TenantId,
    user: UserId,
    api: ApiId,
}

impl Recording {
    /// What will record this request, now that the route says which api serves it.
    ///
    /// Nothing at all when nothing is listening *and* nothing is limited, and nothing
    /// when no route was resolved: a request that never reached an api has no api to be
    /// counted under.
    pub(super) fn of(
        keeping: &Keeping,
        pending: &Pending,
        followed: &Followed,
        started: Instant,
        ruled: Ruled,
    ) -> Option<Self> {
        // Limiting counts too, not only keeping: a quota is enforced by what this writes
        // at the end of a request, so a gateway that limits and records nothing must
        // still reach that code or its counters never move.
        if !keeping.any() && ruled.limiting.is_none() {
            return None;
        }
        Some(Self {
            keeping: keeping.clone(),
            pending: pending.clone(),
            limiting: ruled.limiting,
            prices: ruled.prices,
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
    ///
    /// `source` names *which* cache, which is a different promise in each case and is
    /// the one thing an audit will want of this row. Only a cache answers here, so
    /// only [`Served::Cache`] and [`Served::Semantic`] belong in it.
    pub(super) fn out_of_the_cache(
        self,
        body: &Bytes,
        dispatched: Dispatched,
        filtered: &Filtered,
        source: Served,
    ) {
        let model = measure(body).and_then(|measured| measured.model);
        self.write(
            model,
            Tokens::ZERO,
            Serving {
                source,
                endpoint: None,
                arms: Hedge::default(),
                dispatched,
                // The request was filtered before its key was taken, so a hit is a hit
                // on redacted bytes and the row says what that cost.
                filtered: filtered.clone(),
            },
        );
    }

    /// Measures an answer as it goes out, recording what it cost once the last of it has.
    pub(super) fn measuring(self, body: GatewayBody, attributed: Attributed) -> GatewayBody {
        let Attributed {
            served_by,
            raced,
            dispatched,
            filtered,
            reported,
        } = attributed;
        Measured {
            inner: body,
            measuring: Measuring::default(),
            served_by: Some(served_by),
            raced,
            dispatched,
            filtered,
            reported,
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
    pub(super) fn arbitrated(mut self, call: Arbitrated<'_>) {
        let Arbitrated {
            api,
            endpoint,
            began,
            asked,
            bytes,
        } = call;
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
                // The arbiter's question is the gateway's own, and nothing filters it.
                filtered: Filtered::default(),
            },
        );
    }

    /// Writes the row, off the path of the request it belongs to.
    fn write(self, model: Option<ip_core::ModelName>, tokens: Tokens, serving: Serving) {
        // Counted here rather than beside the store, so that a gateway recording no
        // rows still has a dashboard, and so that the two can never describe the same
        // request differently: there is one place that knows what it was.
        // Above the return below, or a gateway with a dashboard and no rows prices nothing.
        let cost = model
            .as_ref()
            .and_then(|model| self.prices.of(model))
            .map(|price| priced(price, tokens, serving.arms));
        if let Some(meters) = &self.keeping.meters {
            meters.answered(&crate::meter::Answered {
                tenant: &self.tenant,
                api: self.api.as_str(),
                model: model.as_ref(),
                served: serving.source,
                tokens,
                latency: Latency::from_millis(self.started.elapsed().as_millis()),
                dispatched: serving.dispatched,
                raced: serving.arms,
                redacted: serving.filtered.redaction(),
                cost,
            });
        }
        let Some(store) = self.keeping.store.clone() else {
            // Nothing to write, but what was spent still has to be counted against
            // the limits, which is the other half of what this does.
            self.spend(Spent {
                tokens: tokens.total(),
                cost,
            });
            return;
        };
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
            redacted: serving.filtered.recorded(),
            latency: Latency::from_millis(self.started.elapsed().as_millis()),
            cost,
        };
        let limiting = self.limiting;
        let spent = Spent {
            tokens: tokens.total(),
            cost,
        };
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

    /// Counts what was spent against the limits, with no row to write beside it.
    ///
    /// The limits are not an observability concern: a gateway keeping no rows still
    /// enforces a quota, and forgetting this when the store became optional would
    /// have let an operator switch quotas off by switching recording off.
    fn spend(self, spent: Spent) {
        let Some(limiting) = self.limiting else {
            return;
        };
        self.pending.writing(async move {
            limiting
                .limiter
                .spend(&limiting.limits, &limiting.asking, spent)
                .await;
        });
    }
}

/// What an answer cost: the winner in both directions, and the prompt once more for every
/// arm that lost, which the upstream read and billed for whether or not it won.
fn priced(price: Price, tokens: Tokens, arms: Hedge) -> Money {
    let lost = u32::from(arms.get().saturating_sub(1));
    price.of(tokens) + price.input.of(tokens.input).times(lost)
}

/// What an answer is attributed to, as the measurement is handed it.
///
/// A struct rather than five arguments: both functions that take them take all five, in the same
/// order, and nothing reads one of them apart from the rest.
pub(super) struct Attributed {
    /// Which endpoint answered.
    pub(super) served_by: Replica,
    /// How many endpoints it was raced across.
    pub(super) raced: Hedge,
    /// What class it was dispatched at, and which level named it.
    pub(super) dispatched: Dispatched,
    /// What the filter took out of the request and the answer together.
    pub(super) filtered: Filtered,
    /// What the filter reported, when anything counts it.
    pub(super) reported: Option<Reported>,
}

/// The arbiter's own call, as the recorder is told of it.
pub(super) struct Arbitrated<'a> {
    /// Whose route it travelled.
    pub(super) api: ApiId,
    /// Which endpoint it reached.
    pub(super) endpoint: &'a ip_core::Endpoint,
    /// When it began.
    pub(super) began: Instant,
    /// What it asked for, unless the answer echoed a model of its own.
    pub(super) asked: ip_core::ModelName,
    /// The answer, measured rather than parsed.
    pub(super) bytes: &'a [u8],
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
    /// What the filter took out of the request and the answer together.
    ///
    /// The pass itself rather than what it recorded, because the row wants a count
    /// with the kinds written out and the meter wants them apart, and two fields
    /// saying the same thing differently is exactly what this column exists to stop.
    filtered: Filtered,
}

/// A body that measures what passes, and records it when the last of it has.
struct Measured {
    inner: GatewayBody,
    measuring: Measuring,
    served_by: Option<Replica>,
    filtered: Filtered,
    reported: Option<Reported>,
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
        // A streamed answer's tally is left behind by the pump that fed this body, so
        // it is read now rather than when the headers went out.
        let mut filtered = std::mem::take(&mut self.filtered);
        if let Some(reported) = self.reported.take() {
            filtered.absorb(&Filtered::redacted(reported.taken()));
        }
        recording.write(
            model,
            tokens,
            Serving {
                source: Served::Upstream,
                endpoint: self.served_by.take(),
                arms: self.raced,
                dispatched: self.dispatched,
                filtered,
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
