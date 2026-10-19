//! What the gateway counts, and the instruments it counts with.
//!
//! Traces answer *what happened to this request*. No number of them answers *what has
//! been happening*, because they are sampled and because nobody reads a million spans
//! to find a rate. That is this module: a small, fixed set of instruments, built once
//! and written to on the path every request already takes.
//!
//! # Names
//!
//! Dotted, as OpenTelemetry names things. A collector exporting to Prometheus turns
//! `ip.requests` into `ip_requests_total` itself, so writing the Prometheus spelling
//! here would produce `ip_requests_total_total` at the far end.
//!
//! # Labels, and what each one costs
//!
//! Every label multiplies the series a backend holds, so each is here because it
//! answers a question and not because it was available:
//!
//! - `tenant` — **unbounded, and the one that costs**. It is here by ruling, because
//!   *which tenant is doing this* is the question a multi-tenant gateway exists to
//!   answer. It goes on counters only: a counter costs one series per tenant, while a
//!   histogram costs one per bucket per tenant, a dozen times more for a question
//!   — how slow is the gateway — that is about the deployment and not about a tenant.
//! - `api`, `model` — bounded by the upstreams somebody declared.
//! - `outcome` — which of the three places an answer came from.
//!
//! The usage rows remain the detailed record: they hold every one of these per
//! request, queryable and audited. Metrics are the dashboard, the rows are the ledger,
//! and where both could answer a question the row is authoritative. That is what stops
//! this label set having to grow to cover every question anyone will ever ask.

use ip_core::{Latency, ModelName, Served, TenantId, Tokens};
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::{KeyValue, Value};

/// Where the latency histogram's buckets fall, in milliseconds.
///
/// The defaults are no use here. They stop at ten seconds, and an llm call routinely
/// runs longer — everything past the last boundary lands in one overflow bucket, so a
/// gateway whose slow requests are the interesting ones would have a histogram that
/// says only "slow". These run from a cache hit to a long stream:
///
/// - **1 to 25 ms** is an answer the gateway gave itself. A cache hit is well under a
///   millisecond of work plus whatever the backend costs, and seeing that separately
///   from an upstream call is most of why the cache levels are worth having.
/// - **50 to 1000 ms** is a fast model, or a refusal, or a request that never left.
/// - **2.5 to 60 s** is the ordinary range of a real completion. A minute is the last
///   boundary because past it the interesting fact is that it happened at all, which
///   the overflow bucket says perfectly well.
///
/// Milliseconds rather than the seconds OpenTelemetry prefers, so that this and the
/// `latency_ms` column of a usage row are the same number: an operator correlating a
/// spike on a dashboard with the rows behind it should not have to convert.
const LATENCY_MS: &[f64] = &[
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0,
    60_000.0,
];

/// Every instrument the gateway writes to, built once.
///
/// Built once because building one is not free — it goes through the provider to be
/// deduplicated against instruments of the same name — and a request must not pay for
/// that. Handed to the gateway rather than reached for globally, so a test can hold
/// the provider that made them and read back what was written.
#[derive(Debug, Clone)]
pub struct Meters {
    requests: Counter<u64>,
    tokens: Counter<u64>,
    latency: Histogram<u64>,
    lookups: Counter<u64>,
}

impl Meters {
    /// Builds the instruments against `meter`.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests: meter
                .u64_counter("ip.requests")
                .with_description("Requests answered, by who asked and what answered.")
                .with_unit("{request}")
                .build(),
            tokens: meter
                .u64_counter("ip.tokens")
                .with_description("Tokens an answer was measured at, in each direction.")
                .with_unit("{token}")
                .build(),
            latency: meter
                .u64_histogram("ip.latency")
                .with_description("How long a request took, end to end.")
                .with_unit("ms")
                .with_boundaries(LATENCY_MS.to_vec())
                .build(),
            lookups: meter
                .u64_counter("ip.semantic.lookups")
                .with_description(
                    "Turns at the semantic cache, by what came of them, including the \
                     ones never eligible to be asked.",
                )
                .with_unit("{lookup}")
                .build(),
        }
    }

    /// Counts one answered request: that it happened, what it spent, how long it took.
    ///
    /// Called for every answer the gateway gives, including the ones no endpoint was
    /// reached for — a cache hit is a request served, and a dashboard that counted
    /// only upstream calls would report a falling request rate as a cache warmed.
    pub(crate) fn answered(&self, asked: &Answered<'_>) {
        let whose = asked.labels();
        self.requests.add(1, &whose);
        // Recorded even when it is nothing, because nothing is the interesting number:
        // a cache hit spending zero tokens against an upstream call spending a
        // thousand is the whole of what the cache levels are worth, and a series that
        // appeared only when tokens were spent could not show it.
        for (direction, spent) in [
            ("input", u64::from(asked.tokens.input.get())),
            ("output", u64::from(asked.tokens.output.get())),
        ] {
            let mut labels = whose.clone();
            labels.push(KeyValue::new("direction", direction));
            self.tokens.add(spent, &labels);
        }
        self.latency
            .record(u64::from(asked.latency.millis()), &asked.bounded());
    }

    /// Counts what came of one request's turn at the semantic cache.
    ///
    /// Counted here and not from `Recording`, unlike everything above, and the
    /// difference is real: `Recording` knows what a request *was*, while this is what
    /// happened at one gate inside it. A request that was never eligible has no
    /// semantic outcome to carry to the end, and folding this into the record would
    /// mean teaching it about a level it has no other business knowing.
    pub(crate) fn looked(&self, outcome: Looked, tenant: &TenantId, api: &str) {
        self.lookups.add(
            1,
            &[
                KeyValue::new("tenant", tenant.as_str().to_owned()),
                KeyValue::new("api", api.to_owned()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }
}

/// What came of asking the semantic cache, or of not asking it.
///
/// The hit rate alone cannot say whether the level is worth running, because it
/// cannot tell a question that was asked and missed from one that was never eligible
/// to be asked. Those want opposite responses: near-misses say *lower the threshold*,
/// and ineligible traffic says *this workload will never suit this level* — and in
/// `ip.requests` the two are indistinguishable, since both were answered upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Looked {
    /// A question was asked and answered from the cache.
    Hit,
    /// A question was asked and nothing was near enough.
    Miss,
    /// Never asked: the body was not a single unanswered question. An agent loop and
    /// an embeddings call both land here, and for both the answer is the same — this
    /// level is not for this traffic.
    Ineligible,
    /// Never asked: the encoder was busy, too slow, or would not commit to a vector.
    /// Unlike the others this is a capacity problem rather than a property of the
    /// traffic, which is why it is not folded in with them.
    Unembedded,
}

impl Looked {
    /// What this is called on a dashboard.
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Ineligible => "ineligible",
            Self::Unembedded => "unembedded",
        }
    }
}

/// What one answered request is counted under.
///
/// A struct rather than an argument list because every instrument wants a different
/// subset of the same facts, and because the labels have to be built the same way
/// every time: two spellings of one request would be two series of half the traffic
/// each.
pub(crate) struct Answered<'a> {
    /// Whose request it was.
    pub tenant: &'a TenantId,
    /// Which declared upstream served it.
    pub api: &'a str,
    /// The model the answer named, when it named one.
    pub model: Option<&'a ModelName>,
    /// Which of the three places the answer came from.
    pub served: Served,
    /// What the answer was measured at, which is nothing for a cache hit.
    pub tokens: Tokens,
    /// How long the whole thing took.
    pub latency: Latency,
}

impl Answered<'_> {
    /// The labels a counter carries, the tenant among them.
    ///
    /// One series per tenant, which is what the ruling costs and buys.
    fn labels(&self) -> Vec<KeyValue> {
        let mut labels = self.bounded();
        labels.push(KeyValue::new("tenant", self.tenant.as_str().to_owned()));
        labels
    }

    /// The labels a histogram carries: only those a configuration file bounds.
    ///
    /// No tenant here, deliberately. A counter with a tenant costs one series per
    /// tenant; a histogram costs one per bucket per tenant, which with these
    /// boundaries is fourteen times more — for a question that is about the
    /// deployment rather than about any one tenant. What a particular tenant waited
    /// is in their usage rows, to the millisecond, per request.
    fn bounded(&self) -> Vec<KeyValue> {
        vec![
            KeyValue::new("api", self.api.to_owned()),
            // A request whose answer named no model is still a request, so the label
            // is present and empty rather than absent: a missing label would put it in
            // a different series from its own kind.
            KeyValue::new(
                "model",
                self.model.map_or(Value::from(""), |named| {
                    Value::from(named.as_str().to_owned())
                }),
            ),
            KeyValue::new("outcome", outcome(self.served)),
        ]
    }
}

/// What a `Served` is called on a dashboard.
///
/// Spelled here rather than taken from the storage codec: the two happen to agree
/// today, and a label an operator has built a dashboard on must not move because a
/// column was renamed.
fn outcome(served: Served) -> &'static str {
    match served {
        Served::Upstream => "upstream",
        Served::Cache => "cache",
        Served::Semantic => "semantic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_place_an_answer_comes_from_has_a_name() {
        // A `Served` with no name here would silently take another's label and merge
        // two kinds of answer into one series.
        let named: Vec<&str> = [Served::Upstream, Served::Cache, Served::Semantic]
            .into_iter()
            .map(outcome)
            .collect();
        assert_eq!(named, vec!["upstream", "cache", "semantic"]);
        let unique: std::collections::BTreeSet<&str> = named.iter().copied().collect();
        assert_eq!(unique.len(), named.len(), "two answers share a label");
    }

    #[test]
    fn a_request_is_labelled_by_who_asked_and_what_answered() {
        let tenant = TenantId::new("acme").unwrap();
        let model = ModelName::new("claude-opus-5").unwrap();
        let asked = Answered {
            tenant: &tenant,
            api: "anthropic",
            model: Some(&model),
            served: Served::Semantic,
            tokens: Tokens::new(ip_core::TokenCount::new(120), ip_core::TokenCount::new(30)),
            latency: Latency::from_millis(42),
        };
        // Sorted, because labels are a set: the order they are built in is not part
        // of what a series is, and asserting it would break on a harmless rewrite.
        let mut labels: Vec<(String, String)> = asked
            .labels()
            .iter()
            .map(|at| (at.key.to_string(), at.value.to_string()))
            .collect();
        labels.sort();
        assert_eq!(
            labels,
            vec![
                ("api".to_owned(), "anthropic".to_owned()),
                ("model".to_owned(), "claude-opus-5".to_owned()),
                ("outcome".to_owned(), "semantic".to_owned()),
                ("tenant".to_owned(), "acme".to_owned()),
            ]
        );
    }

    /// The ruling's cost, pinned where it is paid: the tenant is on a counter and not
    /// on the histogram, because a histogram multiplies by its buckets.
    #[test]
    fn a_histogram_carries_no_tenant_and_a_counter_does() {
        let tenant = TenantId::new("acme").unwrap();
        let asked = Answered {
            tenant: &tenant,
            api: "anthropic",
            model: None,
            served: Served::Upstream,
            tokens: Tokens::ZERO,
            latency: Latency::from_millis(1),
        };
        let named = |labels: Vec<KeyValue>| -> Vec<String> {
            labels.iter().map(|at| at.key.to_string()).collect()
        };
        assert!(named(asked.labels()).contains(&"tenant".to_owned()));
        assert!(
            !named(asked.bounded()).contains(&"tenant".to_owned()),
            "a tenant on the histogram would cost {} series each instead of one",
            LATENCY_MS.len() + 1
        );
        // And everything else is on both, or the two would not be comparable.
        for label in named(asked.bounded()) {
            assert!(named(asked.labels()).contains(&label), "{label}");
        }
    }

    /// The boundaries have to span what this gateway actually serves — a cache hit
    /// under a millisecond and a long completion of a minute — or the interesting
    /// requests all land in one bucket and the histogram says nothing.
    #[test]
    fn the_latency_buckets_span_a_cache_hit_and_a_long_completion() {
        assert_eq!(
            LATENCY_MS.first(),
            Some(&1.0),
            "a cache hit is milliseconds"
        );
        assert_eq!(
            LATENCY_MS.last(),
            Some(&60_000.0),
            "past a minute the fact that it happened is the whole news"
        );
        let mut rising = LATENCY_MS.to_vec();
        rising.sort_by(f64::total_cmp);
        rising.dedup();
        assert_eq!(rising, LATENCY_MS, "boundaries rise, and none is repeated");
    }

    /// An answer that named no model is still an answer, and belongs in the same
    /// series as the rest of its kind rather than in one of its own.
    #[test]
    fn an_answer_naming_no_model_still_carries_the_label() {
        let tenant = TenantId::new("acme").unwrap();
        let asked = Answered {
            tenant: &tenant,
            api: "anthropic",
            model: None,
            served: Served::Upstream,
            tokens: Tokens::ZERO,
            latency: Latency::from_millis(1),
        };
        let model = asked
            .labels()
            .into_iter()
            .find(|at| at.key.as_str() == "model")
            .expect("the label is there even with nothing to put in it");
        assert_eq!(model.value.to_string(), "");
    }
}
