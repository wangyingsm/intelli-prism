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

use ip_core::{ModelName, Served, TenantId};
use opentelemetry::metrics::{Counter, Meter};
use opentelemetry::{KeyValue, Value};

/// Every instrument the gateway writes to, built once.
///
/// Built once because building one is not free — it goes through the provider to be
/// deduplicated against instruments of the same name — and a request must not pay for
/// that. Handed to the gateway rather than reached for globally, so a test can hold
/// the provider that made them and read back what was written.
#[derive(Debug, Clone)]
pub struct Meters {
    requests: Counter<u64>,
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
        }
    }

    /// Counts one answered request.
    ///
    /// Called for every answer the gateway gives, including the ones no endpoint was
    /// reached for — a cache hit is a request served, and a dashboard that counted
    /// only upstream calls would report a falling request rate as a cache warmed.
    pub(crate) fn answered(&self, asked: &Answered<'_>) {
        self.requests.add(1, &asked.labels());
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
}

impl Answered<'_> {
    /// The labels, built in one place so every instrument agrees about them.
    fn labels(&self) -> Vec<KeyValue> {
        vec![
            KeyValue::new("tenant", self.tenant.as_str().to_owned()),
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
        };
        let labels: Vec<(String, String)> = asked
            .labels()
            .iter()
            .map(|at| (at.key.to_string(), at.value.to_string()))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("tenant".to_owned(), "acme".to_owned()),
                ("api".to_owned(), "anthropic".to_owned()),
                ("model".to_owned(), "claude-opus-5".to_owned()),
                ("outcome".to_owned(), "semantic".to_owned()),
            ]
        );
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
        };
        let model = asked
            .labels()
            .into_iter()
            .find(|at| at.key.as_str() == "model")
            .expect("the label is there even with nothing to put in it");
        assert_eq!(model.value.to_string(), "");
    }
}
