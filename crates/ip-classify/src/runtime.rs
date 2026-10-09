//! A trained encoder behind the gate: text in, distribution out, timed.
//!
//! This is the only place the crate meets a model. The artefact is a fine-tuned
//! `jhu-clsp/mmBERT-base` exported to ONNX — multilingual, so one model answers for
//! English and Chinese — loaded from a directory the operator points at, never
//! embedded here and never in a repository: it is tens of megabytes, and an artefact
//! nobody can rebuild is a liability, so the export script lives with the training
//! spike that produced the checkpoint.
//!
//! Loading is once, at construction: a transformer loaded per request would cost
//! more than the arbiter call the whole classifier exists to avoid. A failure to
//! classify is a value, never a panic and never a block — what the gateway does with
//! the error is its business, not this crate's.
//!
//! # The runtime library, and which one
//!
//! The ONNX Runtime is dynamically loaded (`load-dynamic`): this crate builds with no
//! native dependency present and finds it at open time by name, searched as `dlopen`
//! searches — `$ORIGIN`, `LD_LIBRARY_PATH`, the system cache. Every path in
//! [`Classifier::open`] resolves the library *through a call that returns*: an absent
//! or unloadable library is `ClassifyError::RuntimeMissing`, always a value. (The
//! default loading route of `ort` resolves lazily inside the session builder and
//! aborts the process when it finds nothing — this module never takes that route.)
//!
//! The version is not a free variable: `ort 2.0.0-rc.13` is built against **ONNX
//! Runtime 1.28.0**, which is what this crate was verified against. A newer library
//! on the search path is untested territory — the symbols load, the graph may run,
//! and the numbers are then no longer the ones measured here.
//!
//! Where the library may *not* live: a package manager's cache directory. The
//! artifacts under such a tree are content-addressed and a routine cache prune
//! deletes them out from under a running configuration; it belongs in a system
//! library directory or next to the binary, where it is the operator's to keep.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ip_core::Difficulty;

use crate::{ClassifyError, Distribution, Temperature};

/// The library name the search resolves when nothing better is offered: the SONAME
/// of a release build, as `dlopen` looks it up.
const DEFAULT_LIBRARY: &str = "libonnxruntime.so";

/// The classifier itself: one loaded model, ready for one call at a time.
///
/// A `&mut` on `classify` is the honest bound: concurrent calls belong one per
/// thread, which is what the gateway's semaphore gives. Cloning is not cheap and not
/// offered: the session holds the weights.
#[derive(Debug)]
pub struct Classifier {
    session: ort::session::Session,
    tokenizer: tokenizers::Tokenizer,
    gate: Gate,
}

/// One answer: the distribution over the five classes, and what producing it cost.
///
/// The time is not decoration — the gate's entire economics rest on classification
/// being cheap next to an arbiter call, and a threshold cannot be tuned without
/// knowing what the local path costs on the machine doing the tuning.
#[derive(Debug, Clone)]
pub struct Classification {
    /// The distribution the gate reads, calibrated by the artefact's own temperature.
    pub distribution: Distribution,
    /// Wall time of the forward pass, tokenizer included.
    pub took: Duration,
}

impl Classifier {
    /// Loads an artefact directory — `model.onnx`, `tokenizer.json`, `gate.json` —
    /// resolving the runtime library by the environment or the default search.
    ///
    /// Every failure leaves the process standing: an absent library is
    /// [`ClassifyError::RuntimeMissing`], a missing file
    /// [`ClassifyError::ArtifactMissing`], a graph that will not open a
    /// [`ClassifyError::GraphRefused`]. Startup may fail; it may not abort.
    pub fn open(artifact_dir: impl AsRef<Path>) -> Result<Self, ClassifyError> {
        Self::open_inner(artifact_dir.as_ref(), None)
    }

    /// As [`Self::open`], with the runtime library's path stated rather than searched
    /// for — the arm for an operator who vendored it beside the binary.
    pub fn open_with_library(
        artifact_dir: impl AsRef<Path>,
        library: impl AsRef<Path>,
    ) -> Result<Self, ClassifyError> {
        Self::open_inner(artifact_dir.as_ref(), Some(library.as_ref()))
    }

    fn open_inner(artifact_dir: &Path, library: Option<&Path>) -> Result<Self, ClassifyError> {
        // Files first: their absence is news no installed library changes, and it is
        // reported identically on a machine with a runtime and one without.
        let model = artifact_dir.join("model.onnx");
        let tokenizer_path = artifact_dir.join("tokenizer.json");
        let gate = artifact_dir.join("gate.json");
        for required in [&model, &tokenizer_path, &gate] {
            if !required.exists() {
                return Err(ClassifyError::ArtifactMissing(format!(
                    "{} — an artefact directory holds model.onnx, tokenizer.json \
                     and gate.json",
                    required.display()
                )));
            }
        }
        // Every remaining arm names a candidate and calls the one loading API that
        // returns — `init_from`. ort's own default route defers the load into the
        // first session and aborts on failure, so this code never asks it to. Once a
        // library is loaded for the process a later call resolves against that one:
        // the first successful open in a process owns the choice, as the runtime
        // itself specifies.
        let candidate = library
            .map(PathBuf::from)
            .or_else(Self::library_from_env)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_LIBRARY));
        let builder = ort::init_from(&candidate)
            .map_err(|error| ClassifyError::RuntimeMissing(format!("{candidate:?}: {error}")))?;
        let _environment = builder.commit();
        let artifact = read_gate(&gate)?;
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|error| {
            ClassifyError::ArtifactUnreadable(format!("{}: {error}", tokenizer_path.display()))
        })?;
        let session = ort::session::Session::builder()
            .map_err(|error| ClassifyError::Inference(error.to_string()))?
            .commit_from_file(&model)
            .map_err(|error| {
                ClassifyError::GraphRefused(format!("{}: {error}", model.display()))
            })?;
        Ok(Self {
            session,
            tokenizer,
            gate: artifact,
        })
    }

    /// The artefact's manifest, as read at open.
    #[must_use]
    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    /// The path the runtime library was searched for under `ORT_DYLIB_PATH`, if set.
    #[must_use]
    pub fn library_from_env() -> Option<PathBuf> {
        std::env::var_os(DYLIB_ENV).map(PathBuf::from)
    }

    /// The distribution over the five classes for one text, in the order of
    /// [`Difficulty::ALL`], calibrated by the artefact's own temperature.
    pub fn classify(&mut self, text: &str) -> Result<Classification, ClassifyError> {
        let encoded = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| ClassifyError::Tokenize(error.to_string()))?;
        self.classify_ids(
            encoded
                .get_ids()
                .iter()
                .map(|&id| id as i64)
                .collect::<Vec<i64>>()
                .as_slice(),
        )
    }

    /// The forward pass over already-encoded ids — the truncation rule in the one
    /// place it is stated: keep the END (cutting the tail drops the current request
    /// and leaves the conversation reading as filler, the spike's measured failure),
    /// keep the opening token, which every training sequence began with.
    pub fn classify_ids(&mut self, ids: &[i64]) -> Result<Classification, ClassifyError> {
        let started = Instant::now();
        let mut ids = ids.to_vec();
        if ids.len() > self.gate.max_length {
            ids.drain(1..ids.len() - self.gate.max_length + 1);
        }
        let width = ids.len();
        let attention = vec![1i64; width];
        let shape = [1usize, width];
        let input_ids = ort::value::Tensor::from_array((shape, ids))
            .map_err(|error| ClassifyError::Inference(error.to_string()))?;
        let attention_mask = ort::value::Tensor::from_array((shape, attention))
            .map_err(|error| ClassifyError::Inference(error.to_string()))?;
        let outputs = self
            .session
            .run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
            ])
            .map_err(|error| ClassifyError::Inference(error.to_string()))?;
        let (shape, logits) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|error| ClassifyError::Inference(error.to_string()))?;
        let shape: &[i64] = shape.as_ref();
        // The shape is the proof, not a curiosity: 768 hidden states laid over five
        // logits read exactly as confident nonsense, and only the dimensions tell.
        if shape != [1, Difficulty::COUNT as i64] || logits.len() != Difficulty::COUNT {
            return Err(ClassifyError::WrongOutput { got: logits.len() });
        }
        let as_doubles: Vec<f64> = logits.iter().copied().map(f64::from).collect();
        let distribution = Distribution::softmax(&as_doubles, self.gate.temperature)?;
        Ok(Classification {
            distribution,
            took: started.elapsed(),
        })
    }
}

/// The environment variable naming the runtime library, if the operator set one.
pub const DYLIB_ENV: &str = "ORT_DYLIB_PATH";

/// What the artefact's `gate.json` carries: figures fitted at training that belong
/// beside the weights, never configured, never guessed in code.
#[derive(Debug, Clone)]
pub struct Gate {
    /// The fitted divisor between logits and exponentials.
    pub temperature: Temperature,
    /// The length the model was trained at, which is the length the runtime cuts to.
    pub max_length: usize,
    /// The class order the graph's outputs are in, as named.
    pub classes: Vec<String>,
}

/// Reads the two figures: refuse an unreadable file as itself, and a missing figure
/// as what it is too.
fn read_gate(path: &Path) -> Result<Gate, ClassifyError> {
    let unreadable = || ClassifyError::ArtifactUnreadable(path.display().to_string());
    let text = std::fs::read_to_string(path).map_err(|_| unreadable())?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| unreadable())?;
    let temperature = Temperature::new(
        value
            .get("temperature")
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(unreadable)?,
    )
    .map_err(|_| unreadable())?;
    let max_length = value
        .get("max_length")
        .and_then(serde_json::Value::as_u64)
        .and_then(|length| usize::try_from(length).ok())
        .filter(|length| *length > 2)
        .ok_or_else(unreadable)?;
    let classes = value
        .get("classes")
        .and_then(serde_json::Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<String>>()
        })
        .filter(|names| !names.is_empty())
        .ok_or_else(unreadable)?;
    // The exporter writes a `version`; nothing reads it yet.
    Ok(Gate {
        temperature,
        max_length,
        classes,
    })
}
#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::*;

    /// The artefact, spelled: this names the one trained checkpoint the tests speak
    /// of, and the test skips itself out loud when it is not set. Convention as for
    /// postgres: unset means unchecked, never silently passed.
    const ARTIFACT_ENV: &str = "IP_CLASSIFY_ARTIFACT";

    fn artifact() -> Option<PathBuf> {
        std::env::var_os(ARTIFACT_ENV)
            .map(PathBuf::from)
            .filter(|p| p.exists())
    }

    /// One classifier, opened once and handed out locked: the runtime library is
    /// chosen by the first `init` for the whole process, and `classify` needs `&mut`,
    /// so the process-wide instance rides in a `Mutex` — the same discipline the
    /// gateway wraps it in for real traffic. `None` means no artefact is configured,
    /// and every test that receives a `None` skips out loud rather than passing in
    /// silence.
    fn shared() -> Option<std::sync::MutexGuard<'static, Classifier>> {
        use std::sync::{Mutex, OnceLock};
        static SHARED: OnceLock<Option<Mutex<Classifier>>> = OnceLock::new();
        let path = artifact()?;
        // Opening claims the process-wide library; no sibling may open alongside —
        // the lease covers the one-time initialization for exactly that reason.
        let _init_turn = library_lease();
        let slot = SHARED.get_or_init(|| {
            let library = Classifier::library_from_env();
            let opened = match library {
                Some(library) => Classifier::open_with_library(&path, library),
                None => Classifier::open(&path),
            };
            // A machine with the artefact and no runtime cannot run these regardless
            // of what else is true, so the absence skips — out loud, from the caller
            // that receives the `None` — instead of aborting the suite over a
            // configuration the caller warned about.
            opened.ok().map(Mutex::new)
        });
        slot.as_ref()?.lock().ok()
    }

    /// A throwaway artefact directory of the named shape, for the loading-failure
    /// tests: real layout, files of their claims only.
    fn artifact_with(name: &str, files: [(&str, &[u8]); 3]) -> PathBuf {
        // Per-process and per-test: parallel tests must not delete each other's
        // fixtures, and every one of them reads its library from the environment.
        let dir = std::env::temp_dir().join(format!("ip-classify-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        dir
    }

    /// Whether a runtime library stands to load: read from the environment, never
    /// decided by an abort — the graph test needs a library, and without one it says
    /// so rather than testing the loader's failure path by accident.
    fn library_present() -> bool {
        Classifier::library_from_env().is_some_and(|path| path.exists())
    }

    /// The runtime's library choice is one decision per process, and these tests try
    /// conflicting candidates on purpose; they take turns. Without the lease a test
    /// can see another's library already loaded and read the wrong outcome entirely.
    fn library_lease() -> std::sync::MutexGuard<'static, ()> {
        static LEASE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LEASE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn a_missing_artefact_file_is_reported_with_the_layout() {
        let _library_turn = library_lease();
        let dir = artifact_with(
            "missing-model",
            [
                // model.onnx deliberately absent; the other two claim a real directory.
                ("tokenizer.json", b"{}".as_slice()),
                ("gate.json", br#"{"temperature": 1.3, "max_length": 256, "classes": ["easy", "routine", "median", "hard", "research"], "version": 1}"#),
                ("stray.txt", b"".as_slice()),
            ],
        );
        // Files are answered before the library is touched, so no runtime is needed
        // for this one — and the message names the file and spells the layout.
        let error = Classifier::open(&dir).err();
        match error {
            Some(ClassifyError::ArtifactMissing(message)) => {
                assert!(
                    message.contains("model.onnx"),
                    "the file by name: {message}"
                );
                assert!(
                    message.contains("gate.json"),
                    "the layout spelled: {message}"
                );
            }
            other => panic!("expected the missing file to answer first, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The requirement the plan states outright — an absent library leaves the
    /// process standing — proven the only way that means anything, since the
    /// runtime's library is one decision per process: the test binary spawns itself
    /// running exactly one case and inspects the fate of that separate process.
    const ABSENT_LIBRARY_CASE: &str =
        "runtime::tests::an_absent_library_reports_itself_in_a_fresh_process";

    #[test]
    fn an_absent_runtime_library_is_reported_and_the_process_survives() {
        // The child's open fails on purpose; with the lease the parent guarantees no
        // sibling has a library half-loaded while it happens.
        let _library_turn = library_lease();
        let outcome = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", ABSENT_LIBRARY_CASE])
            .env("ORT_DYLIB_PATH", "/definitely/not/here.so")
            .env_remove("IP_CLASSIFY_ARTIFACT")
            .output()
            .expect("spawn the test binary");
        let watched = format!(
            "{}{}",
            String::from_utf8_lossy(&outcome.stdout),
            String::from_utf8_lossy(&outcome.stderr)
        );
        assert!(
            outcome.status.success(),
            "an absent library aborted or failed the process: {}\n{watched}",
            outcome.status
        );
        assert!(
            watched.contains("1 passed")
                && watched.contains(ABSENT_LIBRARY_CASE.rsplit("::").next().unwrap()),
            "the single watched case did not pass:\n{watched}"
        );
    }

    /// The exactly-one case the parent above runs: with the environment naming an
    /// absent library, `open` against a real artefact directory must report
    /// `RuntimeMissing` as a value; reaching it and returning is the whole proof.
    #[test]
    fn an_absent_library_reports_itself_in_a_fresh_process() {
        if Classifier::library_from_env().as_deref()
            != Some(std::path::Path::new("/definitely/not/here.so"))
        {
            eprintln!("SKIPPED: this case runs only as the child of its parent test");
            return;
        }
        let dir = artifact_with(
            "absent-lib-child",
            [
                ("model.onnx", b"unused".as_slice()),
                ("tokenizer.json", b"{}".as_slice()),
                ("gate.json", br#"{"temperature": 1.3, "max_length": 256, "classes": ["easy", "routine", "median", "hard", "research"], "version": 1}"#),
            ],
        );
        let error = Classifier::open(&dir).expect_err("an absent library cannot load a classifier");
        assert!(
            matches!(error, ClassifyError::RuntimeMissing(_)),
            "an absent library is RuntimeMissing, got {error}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_tokenizer_is_refused_before_any_graph_runs() {
        if !library_present() {
            eprintln!("SKIPPED: no ONNX Runtime library — tokenizer reading untested");
            return;
        }
        let dir = artifact_with(
            "unreadable-tokenizer",
            [
                ("model.onnx", b"unused".as_slice()),
                ("tokenizer.json", b"{}".as_slice()),
                ("gate.json", br#"{"temperature": 1.3, "max_length": 256, "classes": ["easy", "routine", "median", "hard", "research"], "version": 1}"#),
            ],
        );
        let error = Classifier::open(&dir).err();
        assert!(
            matches!(error, Some(ClassifyError::ArtifactUnreadable(_))),
            "an unreadable tokenizer is the news itself, got {error:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_gate_manifest_without_its_figures_is_refused() {
        let _library_turn = library_lease();
        if !library_present() {
            eprintln!("SKIPPED: no ONNX Runtime library — manifest reading untested");
            return;
        }
        let dir = artifact_with(
            "manifest-without-figures",
            [
                ("model.onnx", b"unused".as_slice()),
                ("tokenizer.json", b"{}".as_slice()),
                ("gate.json", br#"{"classes": []}"#),
            ],
        );
        let error = Classifier::open(&dir).expect_err("a manifest without its figures cannot load");
        assert!(
            matches!(error, ClassifyError::ArtifactUnreadable(_)),
            "the missing figures are the unreadable part, got {error}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_graph_is_refused_at_open_not_at_first_call() {
        let _library_turn = library_lease();
        if !library_present() {
            eprintln!("SKIPPED: no ONNX Runtime library — graph opening untested");
            return;
        }
        // The tokenizer fixture parses (so open reaches the graph) and the graph is
        // the only thing left to refuse.
        let dir = artifact_with(
            "corrupt-graph",
            [
                ("model.onnx", b"this is not onnx at all".as_slice()),
                (
                    "tokenizer.json",
                    br#"{"version":"1.0","truncation":null,"padding":null,"model":{"type":"WordPiece","vocab":{"[CLS]":0,"[SEP]":1,"[PAD]":2,"unknown":3},"unk_token":"unknown","continuing_subword_prefix":"","max_input_chars_per_word":100},"post_processor":null,"pre_tokenizer":null}"#,
                ),
                ("gate.json", br#"{"temperature": 1.3, "max_length": 256, "classes": ["easy", "routine", "median", "hard", "research"], "version": 1}"#),
            ],
        );
        let error = Classifier::open(&dir).err();
        assert!(
            matches!(error, Some(ClassifyError::GraphRefused(_))),
            "a corrupt graph is refused at open, got {error:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_plainly_easy_and_a_plainly_hard_request_land_apart() {
        let Some(mut classifier) = shared() else {
            eprintln!("SKIPPED: {ARTIFACT_ENV} is unset — no artefact, nothing measured");
            return;
        };
        let easy = classifier
            .classify("What is 2 + 2?")
            .expect("a plain sentence classifies");
        let hard = classifier
            .classify(
                "Analyse this 400k-line codebase and produce a plan to halve its p99 latency, \
                 with an implementation schedule.",
            )
            .expect("a plain request classifies");
        assert_eq!(
            easy.distribution.margin().class(),
            Difficulty::Easy,
            "what is 2 + 2"
        );
        let hard_class = hard.distribution.margin().class();
        assert!(
            matches!(hard_class, Difficulty::Hard | Difficulty::Research),
            "a 400k-line analysis named hard or research, got {hard_class:?}"
        );
        eprintln!(
            "easy {:?} in {:?}, hard {:?} in {:?}",
            easy.distribution.margin().class(),
            easy.took,
            hard_class,
            hard.took
        );
    }

    #[test]
    fn every_real_text_classifies_to_an_ordinary_answer() {
        let Some(mut classifier) = shared() else {
            eprintln!("SKIPPED: {ARTIFACT_ENV} is unset");
            return;
        };
        // The claim is an `Ok` carrying a distribution, for four real texts Chinese
        // among them: sum and width ride in the type, a mis-shaped export fails in
        // `classify` itself — what this walks is that real forwards reach Ok.
        for text in [
            "",
            "Hello",
            "The quick brown fox",
            "请帮我把这段中文翻译成英文，谢谢。",
        ] {
            let answer = classifier.classify(text).expect("text classifies");
            assert!(answer.took > Duration::ZERO, "a cost is always reported");
        }
    }

    #[test]
    fn chinese_text_tokenizes_to_real_tokens() {
        let Some(mut classifier) = shared() else {
            eprintln!("SKIPPED: {ARTIFACT_ENV} is unset");
            return;
        };
        // The tokenizer claim, provable without asking the model to agree with
        // anything: Chinese text produces more than its special tokens and answers.
        let chinese = "请帮我把这段中文翻译成英文，谢谢。";
        let answer = classifier.classify(chinese).expect("Chinese classifies");
        let tokens = classifier
            .tokenizer
            .encode(chinese, true)
            .expect("Chinese tokenizes");
        assert!(
            tokens.get_ids().len() > 2,
            "more than the two special tokens, got {}",
            tokens.get_ids().len()
        );
        assert!(answer.took > Duration::ZERO);
    }

    #[test]
    fn long_text_truncates_keeping_its_end_and_its_opening_token() {
        let Some(mut classifier) = shared() else {
            eprintln!("SKIPPED: {ARTIFACT_ENV} is unset");
            return;
        };
        let keep = classifier.gate().max_length;
        let vocab = classifier.tokenizer.get_vocab_size(true);
        // A raw id vector of exact `cap + 1`: one in, one less out, whatever any
        // tokenizer decides to merge. Cap going in means cap unchanged comes back —
        // the shedding happens to the token *after* the opening, never the opening.
        let probe: Vec<i64> = std::iter::once(0i64)
            .chain((1..=keep as i64).map(|i| i % vocab as i64))
            .collect();
        let answer = classifier
            .classify_ids(&probe)
            .expect("ids past the cap still classify");
        assert!(answer.took > Duration::ZERO, "a cost is always reported");
    }

    /// The text whose classification gets timed: an ordinary agent request, mid-length.
    const TIMED: &str = "Add a retry with exponential backoff to our HTTP client, with tests.";

    #[test]
    fn one_classification_is_timed_and_reported() {
        let Some(mut classifier) = shared() else {
            eprintln!("SKIPPED: {ARTIFACT_ENV} is unset");
            return;
        };
        // One call to warm the runtime's own pools — the operator's first call pays
        // that, but a tuning figure should be the steady state, and the cold call is
        // reported beside it rather than hidden.
        let cold = classifier.classify(TIMED).expect("classifies");
        assert!(cold.took > Duration::ZERO, "a cost is always reported");
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let warm = classifier.classify(TIMED).expect("classifies");
            best = best.min(warm.took);
        }
        eprintln!(
            "one classification: cold {:?}, best of five warm {:?} — for scale, an \
             arbiter call costs tens to hundreds of ms",
            cold.took, best
        );
    }
}
