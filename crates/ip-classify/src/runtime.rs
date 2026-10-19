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
//! # The runtime library, and the artefact directory
//!
//! Neither is this crate's business: `ip-onnx` finds the ONNX Runtime, checks the
//! directory holds what it must, proves the weights are the ones the manifest speaks
//! for, and opens the graph and the tokenizer. What is left here is the part that is
//! about *this* model — the gate's fitted figures, the five logits, the class order.
//! See that crate's module doc for which runtime library is loaded and why only one
//! of them can be.

use std::path::Path;
use std::time::{Duration, Instant};

use ip_core::Difficulty;

use ip_onnx::OnnxError;

use crate::{ClassifyError, Distribution, Temperature};

/// The classifier itself: one loaded model, ready for one call at a time.
///
/// A `&mut` on `classify` is the honest bound: concurrent calls belong one per
/// thread, which is what the gateway's semaphore gives. Cloning is not cheap and not
/// offered: the session holds the weights.
#[derive(Debug)]
pub struct Classifier {
    session: ip_onnx::ort::session::Session,
    tokenizer: ip_onnx::tokenizers::Tokenizer,
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
    /// Every failure leaves the process standing — an absent library, a missing
    /// file, a graph that will not open are each a value from `ip-onnx`. Startup may
    /// fail; it may not abort.
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
        let artifact = ip_onnx::Artifact::at(artifact_dir, MANIFEST)?;
        ip_onnx::load(library)?;
        let gate = Gate::read(artifact.manifest(), artifact.model())?;
        let tokenizer = artifact.tokenizer()?;
        let session = artifact.session()?;
        Ok(Self {
            session,
            tokenizer,
            gate,
        })
    }

    /// The artefact's manifest, as read at open.
    #[must_use]
    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    /// The path the runtime library was searched for under `ORT_DYLIB_PATH`, if set.
    #[must_use]
    pub fn library_from_env() -> Option<std::path::PathBuf> {
        ip_onnx::library_from_env()
    }

    /// The distribution over the five classes for one text, in the order of
    /// [`Difficulty::ALL`], calibrated by the artefact's own temperature.
    pub fn classify(&mut self, text: &str) -> Result<Classification, ClassifyError> {
        let encoded = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| ClassifyError::from(OnnxError::Tokenize(error.to_string())))?;
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
        let input_ids = ip_onnx::ort::value::Tensor::from_array((shape, ids))
            .map_err(|error| ClassifyError::from(OnnxError::Inference(error.to_string())))?;
        let attention_mask = ip_onnx::ort::value::Tensor::from_array((shape, attention))
            .map_err(|error| ClassifyError::from(OnnxError::Inference(error.to_string())))?;
        let outputs = self
            .session
            .run(ip_onnx::ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
            ])
            .map_err(|error| ClassifyError::from(OnnxError::Inference(error.to_string())))?;
        let (shape, logits) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|error| ClassifyError::from(OnnxError::Inference(error.to_string())))?;
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
pub const DYLIB_ENV: &str = ip_onnx::DYLIB_ENV;

/// What this kind of artefact calls its manifest.
const MANIFEST: &str = "gate.json";

/// Manifest schemas this build reads. A version outside this list is refused rather
/// than read for the keys it recognises, because a later shape may mean something
/// different by the same name. Schema 1 is M13's, which carried no checksum and no
/// margin table; schema 2 adds them, and which build the weights are.
pub const SCHEMAS: [u64; 2] = [1, 2];

/// What one threshold was measured to do, on the split this build was tested against.
///
/// Advisory, always. The gateway dispatches at the threshold its operator configured
/// and never at one this table prefers: the table knows a test split, and an operator
/// may know something about their own traffic that it does not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measured {
    /// The margin the top class must hold over the runner-up.
    pub threshold: f64,
    /// How often the class it committed to was right, above that margin.
    pub accuracy: f64,
    /// What share of requests cleared it and were kept local.
    pub coverage: f64,
}

/// What the artefact's `gate.json` carries: figures fitted at training that belong
/// beside the weights, never configured, never guessed in code.
///
/// Two artefacts of the same weights quantized differently are two different gates —
/// their temperatures are fitted separately and their margin tables measured
/// separately — so everything here is per build and none of it transfers.
#[derive(Debug, Clone)]
pub struct Gate {
    /// The fitted divisor between logits and exponentials.
    pub temperature: Temperature,
    /// The length the model was trained at, which is the length the runtime cuts to.
    pub max_length: usize,
    /// The class order the graph's outputs are in, as named.
    pub classes: Vec<String>,
    /// Which build these weights are — `fp32`, `int8`, whatever the export called it.
    /// Absent in a manifest written before builds were told apart.
    pub quantization: Option<String>,
    /// What this build was measured to do at each threshold, worst threshold first or
    /// in whatever order the export wrote them. Empty when the manifest carries none.
    pub margin: Vec<Measured>,
    /// The model this was fine-tuned from, for a card and a log line.
    pub base: Option<String>,
    /// The artefact's own version, which is not the gateway's and is never pinned by
    /// it: a better model drops in without a release.
    pub model_version: Option<String>,
}

impl Gate {
    /// Reads a manifest and proves the weights beside it are the ones it speaks for.
    ///
    /// One entry point for both callers that need it: the runtime opening an artefact
    /// to serve from, and the CLI checking one an operator carried in by hand. They
    /// must agree about what a valid artefact is, and the way to guarantee that is for
    /// there to be one answer.
    ///
    /// # Errors
    ///
    /// A manifest that will not read, a schema this build does not carry, or weights
    /// whose digest is not the one named.
    pub fn read(manifest: &Path, model: &Path) -> Result<Self, ClassifyError> {
        let value = ip_onnx::read_json(manifest)?;
        let gate = read_gate(manifest, &value)?;
        ip_onnx::check_weights(
            ip_onnx::text_at(&value, "checksum").as_deref(),
            &ip_onnx::digest(model)?,
        )?;
        Ok(gate)
    }
}

/// Reads the figures out of an already-parsed manifest: refuse a missing one as what
/// it is, naming the file it was missing from.
fn read_gate(path: &Path, value: &ip_onnx::serde_json::Value) -> Result<Gate, ClassifyError> {
    let unreadable = || ip_onnx::OnnxError::ArtifactUnreadable(path.display().to_string());
    let temperature = Temperature::new(
        value
            .get("temperature")
            .and_then(ip_onnx::serde_json::Value::as_f64)
            .ok_or_else(unreadable)?,
    )
    .map_err(|_| unreadable())?;
    let max_length = value
        .get("max_length")
        .and_then(ip_onnx::serde_json::Value::as_u64)
        .and_then(|length| usize::try_from(length).ok())
        .filter(|length| *length > 2)
        .ok_or_else(unreadable)?;
    let classes = value
        .get("classes")
        .and_then(ip_onnx::serde_json::Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(ip_onnx::serde_json::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<String>>()
        })
        .filter(|names| !names.is_empty())
        .ok_or_else(unreadable)?;
    // A schema this build does not read is refused whole; see `SCHEMAS`.
    ip_onnx::schema(value, &SCHEMAS)?;
    let margin = value
        .get("margin")
        .and_then(ip_onnx::serde_json::Value::as_array)
        .map(|rows| rows.iter().filter_map(measured).collect())
        .unwrap_or_default();
    Ok(Gate {
        temperature,
        max_length,
        classes,
        quantization: ip_onnx::text_at(value, "quantization"),
        margin,
        base: ip_onnx::text_at(value, "base"),
        model_version: ip_onnx::text_at(value, "model_version"),
    })
}

/// One row of the margin table, or nothing where a row is not one.
fn measured(row: &ip_onnx::serde_json::Value) -> Option<Measured> {
    Some(Measured {
        threshold: row.get("threshold")?.as_f64()?,
        accuracy: row.get("accuracy")?.as_f64()?,
        coverage: row.get("coverage")?.as_f64()?,
    })
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use std::path::PathBuf;

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

    const FULL: &str = r#"{
        "version": 2, "temperature": 1.2, "max_length": 256,
        "classes": ["easy", "routine", "median", "hard", "research"],
        "quantization": "int8", "checksum": "sha256:deadbeef",
        "margin": [
            {"threshold": 0.2, "accuracy": 0.71, "coverage": 0.80},
            {"threshold": 0.4, "accuracy": 0.75, "coverage": 0.59}
        ],
        "base": "jhu-clsp/mmBERT-base", "dataset": "sha256:cafe", "model_version": "3"
    }"#;

    /// A manifest written to this build's own shape reads back whole. Every field is
    /// one an operator or a log line needs; none of it transfers between builds.
    #[test]
    fn a_manifest_of_this_schema_round_trips() {
        let dir = artifact_with(
            "full-gate",
            [("gate.json", FULL.as_bytes()), ("a", b""), ("b", b"")],
        );
        let gate = gate_in(&dir).expect("a manifest");
        assert_eq!(gate.temperature.value(), 1.2);
        assert_eq!(gate.max_length, 256);
        assert_eq!(gate.quantization.as_deref(), Some("int8"));
        assert_eq!(gate.base.as_deref(), Some("jhu-clsp/mmBERT-base"));
        assert_eq!(gate.model_version.as_deref(), Some("3"));
        assert_eq!(gate.margin.len(), 2);
        assert_eq!(gate.margin[0].threshold, 0.2);
        assert_eq!(gate.margin[1].accuracy, 0.75);
        assert_eq!(gate.margin[1].coverage, 0.59);
    }

    /// M13's manifest carried none of this and must still open: a gateway that refused
    /// every artefact built before it could check them would refuse the one on disk.
    #[test]
    fn a_manifest_from_before_any_of_this_still_reads() {
        let old = br#"{"temperature": 1.3, "max_length": 256, "version": 1,
            "classes": ["easy", "routine", "median", "hard", "research"]}"#;
        let dir = artifact_with("old-gate", [("gate.json", old), ("a", b""), ("b", b"")]);
        let gate = gate_in(&dir).expect("a manifest");
        assert_eq!(gate.quantization, None);
        assert!(gate.margin.is_empty(), "no table is not a broken table");
        assert_eq!(gate.model_version, None);
    }

    /// A schema this build does not read is refused whole rather than read for the
    /// keys it happens to recognise.
    #[test]
    fn a_manifest_schema_this_build_does_not_read_is_refused() {
        let ahead = br#"{"version": 99, "temperature": 1.3, "max_length": 256,
            "classes": ["easy", "routine", "median", "hard", "research"]}"#;
        let dir = artifact_with("ahead-gate", [("gate.json", ahead), ("a", b""), ("b", b"")]);
        assert!(matches!(
            gate_in(&dir),
            Err(ClassifyError::Onnx(OnnxError::ArtifactSchema {
                found: 99,
                ..
            }))
        ));
    }

    /// The manifest of an artefact directory, read the way `Gate::read` reads it but
    /// without the weights beside it, which these cases do not have.
    fn gate_in(dir: &Path) -> Result<Gate, ClassifyError> {
        let manifest = dir.join("gate.json");
        let value = ip_onnx::read_json(&manifest)?;
        read_gate(&manifest, &value)
    }

    // The digest and the checksum themselves are `ip-onnx`'s now, and are tested
    // there against files written for the purpose. What stays here is what this
    // crate still decides: its own manifest's fields, and the whole opening path.

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
            Some(ClassifyError::Onnx(OnnxError::ArtifactMissing(message))) => {
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
            matches!(error, ClassifyError::Onnx(OnnxError::RuntimeMissing(_))),
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
            matches!(
                error,
                Some(ClassifyError::Onnx(OnnxError::ArtifactUnreadable(_)))
            ),
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
            matches!(error, ClassifyError::Onnx(OnnxError::ArtifactUnreadable(_))),
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
            matches!(error, Some(ClassifyError::Onnx(OnnxError::GraphRefused(_)))),
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
