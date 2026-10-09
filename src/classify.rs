//! The difficulty judge the gateway consults: the trained encoder, run locally.
//!
//! The gateway asks a `Classifier` behind an option; this is the implementation that
//! answers when the binary carries one — built from an artefact directory and a
//! confidence threshold, and otherwise never present. The build feature is the door:
//! without `classifier` the binary never loads a model, and a config asking for one
//! is refused at startup, loudly, rather than serving silently unclassified.

#[cfg(feature = "classifier")]
mod judge {
    use ip_classify::runtime::Classifier as Encoder;
    use ip_classify::{Classifier, Confidence, Gate};
    use ip_config::DifficultyConfig;
    use ip_core::Difficulty;

    /// Refuse an artefact whose class list this gateway cannot dispatch at — wrong
    /// order, wrong length, or a name no class of ours answers to. The message carries
    /// both lists: what the manifest said, what the gateway is, so the operator sees
    /// the mismatch rather than a panic.
    fn check_class_order(named: &[String]) -> Result<(), String> {
        let expected: Vec<String> = Difficulty::ALL
            .iter()
            .map(|class| class.name().to_owned())
            .collect();
        if *named == expected {
            return Ok(());
        }
        Err(format!(
            "the artefact answers in a class order this gateway does not carry: it names {named:?}, this gateway dispatches at {expected:?}"
        ))
    }

    /// A trained encoder behind its gate. Weights are not read-mostly state, they are
    /// a machine: the judge runs one inference at a time, and one that is busy answers
    /// nothing rather than queueing — the arriving request dispatches unclassified,
    /// now. Bounding the work at the judge is what keeps the flow's deadline guarding
    /// a single classification instead of a backlog, since a dropped `spawn_blocking`
    /// future abandons the result, not the run.
    pub struct RuntimeJudge {
        encoder: std::sync::Mutex<Encoder>,
        /// The margin rule the artefact is judged through: its own temperature, the
        /// operator's threshold.
        gate: Classifier,
    }

    impl std::fmt::Debug for RuntimeJudge {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("RuntimeJudge")
                .field("gate", &self.gate)
                .finish_non_exhaustive()
        }
    }

    impl RuntimeJudge {
        /// Opens the judge from the operator's stanza: the artefact directory and the
        /// library the loader found on its walk come as the config states, and the
        /// library's own search answers where the config stays silent. Any failure is
        /// returned as the string the caller logs and refuses startup with.
        pub fn open(config: &DifficultyConfig) -> Result<Self, String> {
            let encoder = match &config.library {
                Some(library) => Encoder::open_with_library(&config.artifact, library),
                None => Encoder::open(&config.artifact),
            }
            .map_err(|error| error.to_string())?;
            let gate = Classifier::new(
                encoder.gate().temperature,
                Confidence::new(config.confidence).map_err(|error| error.to_string())?,
            );
            let mut judge = Self::new(encoder, gate)?;
            judge.warm()?;
            Ok(judge)
        }

        /// One forward pass at open, its answer dropped and its failure not: a graph
        /// that loads but will not run must refuse startup, never come up and answer
        /// every request unclassified in silence.
        ///
        /// It is also where the runtime's first pass is paid for. That pass costs
        /// ~100 ms wherever in the process it happens — measured on this build's
        /// pair, every pass after it costing the figure the flow's deadline is set
        /// from — so the bill lands here rather than on a real request.
        fn warm(&mut self) -> Result<(), String> {
            self.encoder
                .get_mut()
                .map_err(|_| "the classifier was poisoned before its first pass".to_owned())?
                .classify("warm-up")
                .map(|_| ())
                .map_err(|error| error.to_string())
        }

        /// Wraps an encoder and the gate it answers through, after the class list the
        /// exported model speaks — exact names, exact order — has been checked against
        /// the classes the gateway can dispatch at. An artefact for a different
        /// taxonomy is refused here and never runs.
        pub fn new(encoder: Encoder, gate: Classifier) -> Result<Self, String> {
            check_class_order(
                &encoder
                    .gate()
                    .classes
                    .iter()
                    .map(String::from)
                    .collect::<Vec<String>>(),
            )?;
            Ok(Self {
                encoder: std::sync::Mutex::new(encoder),
                gate,
            })
        }
    }

    impl ip_gateway::Classifier for RuntimeJudge {
        fn classify(&self, text: &str) -> Option<Difficulty> {
            // Busy means now, not eventually; the type says why.
            let mut encoder = match self.encoder.try_lock() {
                Ok(encoder) => encoder,
                Err(std::sync::TryLockError::WouldBlock) => return None,
                // One panic must not switch classification off for the process's life.
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    tracing::warn!("the classifier recovered from a poisoned inference lock");
                    poisoned.into_inner()
                }
            };
            // A silent degradation is indistinguishable from a model that never matches.
            let judged = match encoder.classify(text) {
                Ok(judged) => judged,
                Err(error) => {
                    let kind = match &error {
                        ip_classify::ClassifyError::Inference(_) => "inference",
                        ip_classify::ClassifyError::Tokenize(_) => "tokenize",
                        _ => "classifier error",
                    };
                    tracing::warn!(kind, "classification failed; dispatching unclassified");
                    return None;
                }
            };
            match self.gate.gate_distribution(&judged.distribution) {
                Gate::Dispatch(class) => Some(class),
                Gate::Refer => None,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn all_classes() -> Vec<String> {
            Difficulty::ALL
                .iter()
                .map(|class| class.name().to_owned())
                .collect()
        }

        #[test]
        fn a_gate_at_the_top_margin_dispatches_and_below_it_refers() {
            // The judgement is the classify crate's; what this wrapper owes the gateway is
            // that its two answers are the two the flow can ride, and nothing else. The
            // seam is exercised end to end against a real artefact in the api suite, and
            // unit-tested where a distribution can be held still: gate arithmetic lives in
            // `ip-classify`, and these lines check only the wiring around it.
            let threshold = Confidence::new(0.2).expect("a threshold in reach");
            let temperature = ip_classify::Temperature::new(1.3).expect("a positive divisor");
            let judged = Classifier::new(temperature, threshold);
            // A distribution whose top stands a full point over the field clears any
            // threshold in range; `RuntimeJudge` adds no arithmetic of its own between
            // this table and the gateway's `Classifier`.
            let rest = 0.000_1f64;
            let wide: Vec<f64> = Difficulty::ALL
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    if index == 0 {
                        1.0 - rest * (Difficulty::COUNT - 1) as f64
                    } else {
                        rest
                    }
                })
                .collect();
            assert_eq!(
                judged.gate_distribution(
                    &ip_classify::Distribution::new(&wide).expect("probabilities that sum")
                ),
                Gate::Dispatch(Difficulty::Easy)
            );
            // And the same gate on a near-flat field — every margin under the 0.2 the
            // judge was built with — refers rather than committing to its runner-up.
            let tie = 1.0 / Difficulty::COUNT as f64;
            let flat = vec![tie; Difficulty::COUNT];
            assert_eq!(
                judged.gate_distribution(
                    &ip_classify::Distribution::new(&flat).expect("probabilities that sum")
                ),
                Gate::Refer
            );
        }

        /// The whole opening path — artefact, library, tokenizer, gate — against
        /// the model itself, for whoever runs with `IP_CLASSIFY_ARTIFACT` and a
        /// runtime library in reach. Without both it says so and passes; the flow of
        /// a request through a live judge is the gateway suite's subject, not this.
        #[test]
        fn a_real_artifact_opens_and_answers_through_the_judge() {
            let Some(dir) = std::env::var("IP_CLASSIFY_ARTIFACT").ok() else {
                eprintln!("IP_CLASSIFY_ARTIFACT unset: no artefact to open");
                return;
            };
            let config = DifficultyConfig {
                artifact: std::path::PathBuf::from(dir),
                confidence: 0.2,
                library: std::env::var("ORT_DYLIB_PATH")
                    .ok()
                    .map(std::path::PathBuf::from),
            };
            let judge = RuntimeJudge::open(&config).expect("the artefact opens");
            // Whatever the model says about this text, it says it twice: the wrapper
            // owns its lock and its session such that a second classification of the
            // same text answers the same way — the property a lock-poisoning or
            // shared-session bug would break, and the judge owes the flow.
            let first = ip_gateway::Classifier::classify(&judge, "Write a haiku about rain.");
            let second = ip_gateway::Classifier::classify(&judge, "Write a haiku about rain.");
            assert_eq!(first, second);
        }

        /// The commit's central number, pinned against the model it was set from: a
        /// classification the flow would attempt must answer inside
        /// [`CLASSIFY_DEADLINE`], run exactly the way the flow runs it. Without an
        /// artefact it says so and passes.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_real_classification_answers_inside_the_deadline() {
            let Some(dir) = std::env::var("IP_CLASSIFY_ARTIFACT").ok() else {
                eprintln!("IP_CLASSIFY_ARTIFACT unset: no artefact to time");
                return;
            };
            let config = DifficultyConfig {
                artifact: std::path::PathBuf::from(dir),
                confidence: 0.2,
                library: std::env::var("ORT_DYLIB_PATH")
                    .ok()
                    .map(std::path::PathBuf::from),
            };
            let judge =
                std::sync::Arc::new(RuntimeJudge::open(&config).expect("the artefact opens"));
            let started = std::time::Instant::now();
            let timed = std::sync::Arc::clone(&judge);
            let answered = tokio::time::timeout(
                ip_gateway::CLASSIFY_DEADLINE,
                tokio::task::spawn_blocking(move || {
                    ip_gateway::Classifier::classify(timed.as_ref(), "Summarise this quarter")
                }),
            )
            .await
            .expect("a real classification answers inside the deadline");
            eprintln!(
                "classification answered in {:?} of {:?}",
                started.elapsed(),
                ip_gateway::CLASSIFY_DEADLINE
            );
            // What it answered is the model's business; that it answered at all,
            // inside the deadline, is the assertion.
            let _ = answered.expect("the judge task survives to its answer");
        }

        fn names(classes: &[&str]) -> Vec<String> {
            classes.iter().map(|name| (*name).to_owned()).collect()
        }

        #[test]
        fn the_artefacts_own_class_order_is_accepted() {
            assert_eq!(check_class_order(&all_classes()), Ok(()));
        }

        #[test]
        fn a_reordered_class_list_is_refused() {
            // The same five names in the wrong order would mis-dispatch every
            // confident classification: the check that runs where the judge is built
            // must say so, naming both lists, before a single forward pass runs.
            let mut shuffled = all_classes();
            shuffled.swap(0, 1);
            let error = check_class_order(&shuffled).expect_err("a swap is a mismatch");
            // The error must render the artefact's own list in the artefact's own
            // order, or it is not a report of what was refused.
            assert!(error.contains(&format!("{shuffled:?}")), "{error}");
        }

        #[test]
        fn a_short_class_list_is_refused() {
            let short = all_classes()[..Difficulty::COUNT - 1].to_vec();
            let error = check_class_order(&short).expect_err("four is not five");
            assert!(error.contains("class order"), "{error}");
        }

        #[test]
        fn a_class_this_gateway_does_not_carry_is_refused() {
            let bogus = names(&["easy", "routine", "median", "hard", "exascalerating"]);
            let error = check_class_order(&bogus).expect_err("no class by that name");
            assert!(error.contains("exascalerating"), "{error}");
        }
    }
}

#[cfg(feature = "classifier")]
pub use judge::RuntimeJudge;
