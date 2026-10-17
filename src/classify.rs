//! The difficulty judge the gateway consults: the trained encoder, run locally.
//!
//! The gateway asks a `Classifier` behind an option; this is the implementation that
//! answers when the binary carries one — built from an artefact directory and a
//! confidence threshold, and otherwise never present. The build feature is the door:
//! without `classifier` the binary never loads a model, and a config asking for one
//! is refused at startup, loudly, rather than serving silently unclassified.

#[cfg(feature = "classifier")]
mod judge {
    use ip_classify::runtime::{Classifier as Encoder, Gate as Manifest, Measured};
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
            said(encoder.gate());
            advise(encoder.gate(), config.confidence);
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

    /// What was loaded, in the one line an operator reads when a gateway comes up.
    ///
    /// "Classifier loaded" says nothing worth saying: with two quantizations of one
    /// model in circulation, each with its own fitted temperature, which one came up
    /// is the thing an operator needs to be able to check against what they meant to
    /// deploy.
    fn said(gate: &Manifest) {
        let loaded = loaded(gate);
        tracing::info!(
            build = loaded.build,
            temperature = loaded.temperature,
            model = loaded.model,
            base = loaded.base,
            "classifier loaded"
        );
    }

    /// What that line says, as values a test can read: an artefact from before builds
    /// were named still has to produce a line, and a word standing in for an absent
    /// field is better than a blank an operator reads as a bug.
    struct Loaded<'a> {
        build: &'a str,
        temperature: f64,
        model: &'a str,
        base: &'a str,
    }

    fn loaded(gate: &Manifest) -> Loaded<'_> {
        Loaded {
            build: gate.quantization.as_deref().unwrap_or("unnamed"),
            temperature: gate.temperature.value(),
            model: gate.model_version.as_deref().unwrap_or("unversioned"),
            base: gate.base.as_deref().unwrap_or("unnamed"),
        }
    }

    /// How far below the best the table knows a threshold may be rated before it is
    /// worth saying anything. Under this, the difference is a test split's noise.
    const WORTH_SAYING: f64 = 0.05;

    /// The row nearest the configured threshold and the best row in the table, when
    /// the first is rated materially worse than the second — and nothing otherwise.
    ///
    /// Separated from the saying so it can be tested: a warning nothing asserts is a
    /// warning that can stop firing without anyone noticing.
    fn poorly_rated(gate: &Manifest, confidence: f64) -> Option<(&Measured, &Measured)> {
        let nearest = gate.margin.iter().min_by(|one, two| {
            (one.threshold - confidence)
                .abs()
                .total_cmp(&(two.threshold - confidence).abs())
        })?;
        let best = gate
            .margin
            .iter()
            .max_by(|one, two| one.accuracy.total_cmp(&two.accuracy))?;
        (nearest.accuracy + WORTH_SAYING < best.accuracy).then_some((nearest, best))
    }

    /// Says so when the artefact's own measurements rate the configured threshold
    /// poorly, and then dispatches at it anyway.
    ///
    /// The table is advisory by ruling. It was measured on a test split; an operator
    /// may know something about their traffic that the split does not, and a gateway
    /// that overrode them would be substituting a stranger's data for their judgement.
    /// Saying nothing, though, leaves a threshold nobody checked looking deliberate.
    fn advise(gate: &Manifest, confidence: f64) {
        if let Some((nearest, best)) = poorly_rated(gate, confidence) {
            tracing::warn!(
                threshold = confidence,
                accuracy = nearest.accuracy,
                coverage = nearest.coverage,
                better = best.threshold,
                better_accuracy = best.accuracy,
                "the configured margin is one this artefact was measured to do poorly at; \
                 dispatching at it as configured"
            );
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

        fn manifest(margin: Vec<ip_classify::runtime::Measured>) -> Manifest {
            Manifest {
                temperature: ip_classify::Temperature::new(1.3).unwrap(),
                max_length: 256,
                classes: all_classes(),
                quantization: Some("embed8".to_owned()),
                margin,
                base: Some("jhu-clsp/mmBERT-base".to_owned()),
                model_version: Some("1".to_owned()),
            }
        }

        fn measured(threshold: f64, accuracy: f64) -> ip_classify::runtime::Measured {
            ip_classify::runtime::Measured {
                threshold,
                accuracy,
                coverage: 0.5,
            }
        }

        /// "Classifier loaded" says nothing worth reading. With two quantizations of
        /// one model in circulation, each with its own fitted temperature, which one
        /// came up is the thing an operator checks against what they meant to deploy.
        #[test]
        fn the_line_an_operator_reads_names_the_build_and_its_temperature() {
            let gate = manifest(Vec::new());
            let named = loaded(&gate);
            assert_eq!(named.build, "embed8");
            assert_eq!(named.temperature, 1.3);
            assert_eq!(named.model, "1");
            assert_eq!(named.base, "jhu-clsp/mmBERT-base");

            // An artefact from before any of this still produces a line, with a word
            // where a field would be rather than a blank that reads as a fault.
            let mut old = manifest(Vec::new());
            old.quantization = None;
            old.model_version = None;
            old.base = None;
            let named = loaded(&old);
            assert_eq!(
                (named.build, named.model, named.base),
                ("unnamed", "unversioned", "unnamed")
            );
            assert_eq!(named.temperature, 1.3, "the temperature is always there");
        }

        /// A warning on every startup is a warning nobody reads, so what the table
        /// rates well is passed over in silence.
        #[test]
        fn a_threshold_the_artefact_rates_well_is_passed_over_in_silence() {
            let table = vec![
                measured(0.2, 0.71),
                measured(0.4, 0.79),
                measured(0.6, 0.84),
            ];
            let gate = manifest(table);
            assert!(
                poorly_rated(&gate, 0.6).is_none(),
                "the best row is not poor"
            );
            assert!(
                poorly_rated(&gate, 0.4).is_none(),
                "0.79 against a best of 0.84 is inside a test split's noise"
            );
        }

        /// A table the artefact does not carry cannot rate anything, and its absence
        /// is not a fault: the gateway runs on the operator's threshold either way.
        #[test]
        fn a_threshold_no_table_speaks_for_is_passed_over_too() {
            assert!(poorly_rated(&manifest(Vec::new()), 0.01).is_none());
        }

        /// A threshold measured far below the best the table knows is named, with the
        /// row that would do better — and the dispatch still happens at the one the
        /// operator configured, which is the ruling.
        #[test]
        fn a_threshold_the_artefact_rates_poorly_is_said_out_loud() {
            let gate = manifest(vec![measured(0.2, 0.60), measured(0.6, 0.84)]);
            let (nearest, best) = poorly_rated(&gate, 0.2).expect("0.60 against 0.84 is poor");
            assert_eq!(
                nearest.threshold, 0.2,
                "the row nearest what was configured"
            );
            assert_eq!(best.threshold, 0.6, "and the row that would do better");

            // Advising is not enforcing. That the gateway then runs at the threshold
            // it was configured with is what `a_real_artifact_opens_and_answers_
            // through_the_judge` shows, against the artefact's own manifest.
            advise(&gate, 0.2);
        }

        /// The nearest row is the nearest, not the one below: a threshold between two
        /// is rated by whichever it actually sits closest to.
        #[test]
        fn the_row_a_threshold_is_rated_by_is_the_nearest_one() {
            let gate = manifest(vec![measured(0.2, 0.60), measured(0.9, 0.95)]);
            let (nearest, _) = poorly_rated(&gate, 0.4).expect("0.4 is nearer 0.2 than 0.9");
            assert_eq!(nearest.threshold, 0.2);
            assert!(
                poorly_rated(&gate, 0.8).is_none(),
                "0.8 is nearest 0.9, which is the best row there is"
            );
        }

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
        ///
        /// It runs only when `IP_CLASSIFY_TIMING` asks for it, because it is a
        /// measurement and not an assertion about the code. A forward pass takes 20 ms
        /// on an idle machine and far longer on one running a test per core, or on an
        /// instrumented build counting every branch of it — so run as part of an
        /// ordinary suite the clock would be measuring the load, and a deadline it
        /// failed there would say nothing about the deadline the flow keeps. Set the
        /// variable, with an artefact, on a quiet machine, when the number is what you
        /// want to know.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_real_classification_answers_inside_the_deadline() {
            if std::env::var("IP_CLASSIFY_TIMING").is_err() {
                eprintln!("IP_CLASSIFY_TIMING unset: not measuring a clock no one asked for");
                return;
            }
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
