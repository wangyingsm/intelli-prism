//! A trained encoder behind the cache: text in, one unit vector out, timed.
//!
//! This is the only place the crate meets a model, and it is deliberately the same
//! shape as `ip-classify`'s runtime, down to the failure modes: an artefact directory
//! the operator points at, the ONNX Runtime found by name at open rather than linked
//! at build, and every failure a value so that a gateway whose embedder will not load
//! still serves.
//!
//! # What an artefact is
//!
//! A directory holding `model.onnx`, `tokenizer.json` and `embed.json`. The manifest
//! says what the weights are and how to read their output — the width, the length they
//! were trained at, how the token vectors become one vector, and the prefix some
//! models require of a query. None of it is configuration: it is a property of the
//! weights, it is fitted or chosen at export, and an operator who could set it by hand
//! could silently decalibrate the whole level.
//!
//! # Which end of a long text is kept
//!
//! The classifier keeps the **end** of what it is given, because it reads a whole
//! conversation and the newest turn is the one being asked about. This keeps the
//! **start**, and the difference is not an inconsistency: what reaches here is a
//! single turn by the time the gateway asks, so the question is at the front and
//! whatever follows it is material the question is about. Truncation is left to the
//! tokenizer rather than done to the ids afterwards, so the closing token survives it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{EmbedError, Embedding, Width};

/// The library name the search resolves when nothing better is offered: the SONAME of
/// a release build, as `dlopen` looks it up.
const DEFAULT_LIBRARY: &str = "libonnxruntime.so";

/// The environment variable naming the runtime library, if the operator set one. The
/// same one the classifier reads: one process loads one runtime.
pub const DYLIB_ENV: &str = "ORT_DYLIB_PATH";

/// Manifest schemas this build reads. A version outside this list is refused rather
/// than read for the keys it recognises, because a later shape may mean something
/// different by the same name.
pub const SCHEMAS: [u64; 1] = [1];

/// How a sequence of token vectors becomes the one vector that stands for the text.
///
/// Which of these is right is a property of how the model was trained, never a
/// preference: reading a mean-pooled model's first token gives a confident vector that
/// means nothing much, and no error anywhere says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// The average of every token's vector.
    Mean,
    /// The first token's vector alone, which is `[CLS]` in the models that train it to
    /// carry the sentence.
    First,
}

impl Pooling {
    /// Reads the manifest's word for it.
    pub fn named(name: &str) -> Result<Self, EmbedError> {
        match name {
            "mean" => Ok(Self::Mean),
            "cls" | "first" => Ok(Self::First),
            other => Err(EmbedError::UnknownPooling {
                named: other.to_owned(),
            }),
        }
    }

    /// The word this is written as.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mean => "mean",
            Self::First => "cls",
        }
    }
}

/// What the artefact's `embed.json` carries, beside the weights it speaks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// How many numbers one of this model's vectors has.
    pub width: Width,
    /// The length the model was trained at, which is the length the tokenizer cuts to.
    pub max_length: usize,
    /// How the token vectors become one.
    pub pooling: Pooling,
    /// What some models require in front of a text being searched *with*, as opposed
    /// to one being searched *over*. Both sides of this cache are questions, so one
    /// prefix is enough — but leaving it off a model that wants it moves every vector
    /// in the same wrong direction, which looks like a working cache that never hits.
    pub query_prefix: Option<String>,
    /// Which build these weights are — `fp32`, `int8`, whatever the export called it.
    pub quantization: Option<String>,
    /// The model this came from, for a card and a log line.
    pub base: Option<String>,
    /// The artefact's own version, which the gateway never pins.
    pub model_version: Option<String>,
    /// The digest of the weights as they are on disk.
    ///
    /// Always computed, whether or not the manifest names one, because this is what
    /// tells two models apart in a cache key. A version string is what somebody
    /// remembered to bump; this is what was actually loaded, and an answer found by
    /// one model's geometry must never be served to a question asked of another's.
    pub digest: String,
}

impl Manifest {
    /// Reads a manifest and proves the weights beside it are the ones it speaks for.
    ///
    /// # Errors
    ///
    /// A manifest that will not read, a schema this build does not carry, a pooling it
    /// does not know, or weights whose digest is not the one named.
    pub fn read(manifest: &Path, model: &Path) -> Result<Self, EmbedError> {
        let unreadable = || EmbedError::ArtifactUnreadable(manifest.display().to_string());
        let text = std::fs::read_to_string(manifest).map_err(|_| unreadable())?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| unreadable())?;
        let digest = digest(model)?;
        check_weights(text_at(&value, "checksum").as_deref(), &digest)?;
        Self::from_value(&value, digest).map_err(|error| match error {
            // A field that is missing or mis-shaped names the file it is missing from.
            EmbedError::ArtifactUnreadable(_) => unreadable(),
            other => other,
        })
    }

    /// The manifest's fields out of already-parsed json, so what a manifest means can
    /// be tested without a directory to put one in.
    pub fn from_value(value: &serde_json::Value, digest: String) -> Result<Self, EmbedError> {
        let unreadable = || EmbedError::ArtifactUnreadable("embed.json".to_owned());
        let schema = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        if !SCHEMAS.contains(&schema) {
            return Err(EmbedError::ArtifactSchema {
                found: schema,
                carried: format!("{SCHEMAS:?}"),
            });
        }
        let width = Width::new(
            value
                .get("width")
                .and_then(serde_json::Value::as_u64)
                .and_then(|width| usize::try_from(width).ok())
                .ok_or_else(unreadable)?,
        )?;
        let max_length = value
            .get("max_length")
            .and_then(serde_json::Value::as_u64)
            .and_then(|length| usize::try_from(length).ok())
            // Two tokens is an opening and a closing one with no text between them.
            .filter(|length| *length > 2)
            .ok_or_else(unreadable)?;
        let pooling = Pooling::named(
            value
                .get("pooling")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(unreadable)?,
        )?;
        Ok(Self {
            width,
            max_length,
            pooling,
            query_prefix: text_at(value, "query_prefix"),
            quantization: text_at(value, "quantization"),
            base: text_at(value, "base"),
            model_version: text_at(value, "model_version"),
            digest,
        })
    }
}

/// A string field of the manifest, when it carries one that is not empty.
fn text_at(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The digest of the weights, read in pieces so a file of any size costs one buffer.
fn digest(path: &Path) -> Result<String, EmbedError> {
    use sha2::Digest;
    use std::io::Read;
    let unreadable = || EmbedError::ArtifactUnreadable(path.display().to_string());
    let mut file = std::fs::File::open(path).map_err(|_| unreadable())?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|_| unreadable())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Refuses weights that are not the weights the manifest was written for, when the
/// manifest says which those were.
fn check_weights(named: Option<&str>, found: &str) -> Result<(), EmbedError> {
    let Some(named) = named else {
        return Ok(());
    };
    let named = named.strip_prefix("sha256:").unwrap_or(named);
    if found != named {
        return Err(EmbedError::ArtifactChecksum {
            named: named.to_owned(),
            found: found.to_owned(),
        });
    }
    Ok(())
}

/// One answer: the vector, and what producing it cost.
///
/// The time is not decoration. This runs on the request path, in front of an upstream
/// call it is trying to avoid, and a threshold cannot be chosen without knowing what
/// asking costs the requests that miss.
#[derive(Debug, Clone)]
pub struct Embedded {
    /// The text's meaning, one unit long.
    pub embedding: Embedding,
    /// Wall time of the forward pass, tokenizer included.
    pub took: Duration,
}

/// The encoder itself: one loaded model, ready for one call at a time.
///
/// A `&mut` on [`Self::embed`] is the honest bound — the session holds the weights and
/// they are a machine, not read-mostly state. Cloning is not cheap and is not offered.
#[derive(Debug)]
pub struct Embedder {
    session: ort::session::Session,
    tokenizer: tokenizers::Tokenizer,
    manifest: Manifest,
    /// Whether the graph asks for `token_type_ids`. BERT-shaped exports do and
    /// XLM-R-shaped ones do not, and a graph handed an input it never declared
    /// refuses the whole run.
    wants_token_types: bool,
}

impl Embedder {
    /// Loads an artefact directory — `model.onnx`, `tokenizer.json`, `embed.json` —
    /// resolving the runtime library by the environment or the default search.
    ///
    /// Every failure leaves the process standing, as the classifier's does.
    pub fn open(artifact_dir: impl AsRef<Path>) -> Result<Self, EmbedError> {
        Self::open_inner(artifact_dir.as_ref(), None)
    }

    /// As [`Self::open`], with the runtime library's path stated rather than searched
    /// for.
    pub fn open_with_library(
        artifact_dir: impl AsRef<Path>,
        library: impl AsRef<Path>,
    ) -> Result<Self, EmbedError> {
        Self::open_inner(artifact_dir.as_ref(), Some(library.as_ref()))
    }

    fn open_inner(artifact_dir: &Path, library: Option<&Path>) -> Result<Self, EmbedError> {
        // Files first: their absence is news no installed library changes.
        let model = artifact_dir.join("model.onnx");
        let tokenizer_path = artifact_dir.join("tokenizer.json");
        let manifest_path = artifact_dir.join("embed.json");
        for required in [&model, &tokenizer_path, &manifest_path] {
            if !required.exists() {
                return Err(EmbedError::ArtifactMissing(format!(
                    "{} — an artefact directory holds model.onnx, tokenizer.json \
                     and embed.json",
                    required.display()
                )));
            }
        }
        // `init_from` is the one loading route that returns rather than aborting the
        // process when it finds nothing, which is why this never takes ort's default.
        let candidate = library
            .map(PathBuf::from)
            .or_else(Self::library_from_env)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_LIBRARY));
        let builder = ort::init_from(&candidate)
            .map_err(|error| EmbedError::RuntimeMissing(format!("{candidate:?}: {error}")))?;
        let _environment = builder.commit();
        let manifest = Manifest::read(&manifest_path, &model)?;
        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|error| {
            EmbedError::ArtifactUnreadable(format!("{}: {error}", tokenizer_path.display()))
        })?;
        // Cut in the tokenizer rather than in the ids, so the closing token the model
        // was trained to see is still there after a long text is shortened.
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                direction: tokenizers::TruncationDirection::Right,
                max_length: manifest.max_length,
                strategy: tokenizers::TruncationStrategy::LongestFirst,
                stride: 0,
            }))
            .map_err(|error| {
                EmbedError::ArtifactUnreadable(format!("{}: {error}", tokenizer_path.display()))
            })?;
        let session = ort::session::Session::builder()
            .map_err(|error| EmbedError::Inference(error.to_string()))?
            .commit_from_file(&model)
            .map_err(|error| EmbedError::GraphRefused(format!("{}: {error}", model.display())))?;
        let wants_token_types = session
            .inputs()
            .iter()
            .any(|input| input.name() == TOKEN_TYPE_IDS);
        Ok(Self {
            session,
            tokenizer,
            manifest,
            wants_token_types,
        })
    }

    /// The artefact's manifest, as read at open.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The path the runtime library was searched for under `ORT_DYLIB_PATH`, if set.
    #[must_use]
    pub fn library_from_env() -> Option<PathBuf> {
        std::env::var_os(DYLIB_ENV).map(PathBuf::from)
    }

    /// What one text means, as a unit vector of the artefact's width.
    pub fn embed(&mut self, text: &str) -> Result<Embedded, EmbedError> {
        let started = Instant::now();
        let asked = match &self.manifest.query_prefix {
            Some(prefix) => format!("{prefix}{text}"),
            None => text.to_owned(),
        };
        let encoded = self
            .tokenizer
            .encode(asked.as_str(), true)
            .map_err(|error| EmbedError::Tokenize(error.to_string()))?;
        let ids: Vec<i64> = encoded.get_ids().iter().map(|&id| i64::from(id)).collect();
        let width = ids.len();
        if width == 0 {
            return Err(EmbedError::Tokenize(
                "the text is no tokens at all".to_owned(),
            ));
        }
        let shape = [1usize, width];
        let tensor = |numbers: Vec<i64>| {
            ort::value::Tensor::from_array((shape, numbers))
                .map_err(|error| EmbedError::Inference(error.to_string()))
        };
        // One text at a time, so nothing is padded and every position is attended to.
        let mut inputs = ort::inputs![
            "input_ids" => tensor(ids)?,
            "attention_mask" => tensor(vec![1i64; width])?,
        ];
        if self.wants_token_types {
            inputs.push((TOKEN_TYPE_IDS.into(), tensor(vec![0i64; width])?.into()));
        }
        let outputs = self
            .session
            .run(inputs)
            .map_err(|error| EmbedError::Inference(error.to_string()))?;
        let (shape, numbers) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|error| EmbedError::Inference(error.to_string()))?;
        let pooled = pool(
            shape.as_ref(),
            numbers,
            self.manifest.pooling,
            &self.manifest,
        )?;
        Ok(Embedded {
            embedding: Embedding::new(&pooled)?,
            took: started.elapsed(),
        })
    }
}

/// The input a BERT-shaped graph declares and an XLM-R-shaped one does not.
const TOKEN_TYPE_IDS: &str = "token_type_ids";

/// One vector out of whatever shape the graph answered with.
///
/// Two shapes are accepted because both are exported in practice. `[1, width]` is a
/// graph that pooled for itself, and is taken as it stands — the manifest's pooling
/// does not get a second say over a decision already baked into the weights.
/// `[1, tokens, width]` is the token vectors, and is pooled here. Anything else is
/// refused rather than reinterpreted: a width laid over a token axis reads as a
/// perfectly confident vector, and only the dimensions ever tell.
fn pool(
    shape: &[i64],
    numbers: &[f32],
    pooling: Pooling,
    manifest: &Manifest,
) -> Result<Vec<f32>, EmbedError> {
    let want = manifest.width.get();
    let wrong = || EmbedError::WrongOutput {
        expected: want,
        got: format!("{shape:?}"),
    };
    match *shape {
        [1, width] if width as usize == want && numbers.len() == want => Ok(numbers.to_vec()),
        [1, tokens, width]
            if width as usize == want && tokens > 0 && numbers.len() == tokens as usize * want =>
        {
            let tokens = tokens as usize;
            Ok(match pooling {
                Pooling::First => numbers[..want].to_vec(),
                Pooling::Mean => (0..want)
                    .map(|at| {
                        let total: f32 = (0..tokens).map(|token| numbers[token * want + at]).sum();
                        total / tokens as f32
                    })
                    .collect(),
            })
        }
        _ => Err(wrong()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_of(value: &serde_json::Value) -> Result<Manifest, EmbedError> {
        Manifest::from_value(value, "deadbeef".to_owned())
    }

    fn whole() -> serde_json::Value {
        json!({
            "version": 1,
            "width": 384,
            "max_length": 512,
            "pooling": "mean",
            "query_prefix": "query: ",
            "quantization": "int8",
            "base": "intfloat/multilingual-e5-small",
            "model_version": "1",
        })
    }

    #[test]
    fn a_whole_manifest_reads_as_what_it_says() {
        let read = manifest_of(&whole()).unwrap();
        assert_eq!(read.width, Width::new(384).unwrap());
        assert_eq!(read.max_length, 512);
        assert_eq!(read.pooling, Pooling::Mean);
        assert_eq!(read.query_prefix.as_deref(), Some("query: "));
        assert_eq!(read.quantization.as_deref(), Some("int8"));
        assert_eq!(read.base.as_deref(), Some("intfloat/multilingual-e5-small"));
        assert_eq!(read.model_version.as_deref(), Some("1"));
        assert_eq!(read.digest, "deadbeef");
    }

    #[test]
    fn the_fields_a_manifest_may_leave_out_are_left_out() {
        let spare = json!({"width": 768, "max_length": 128, "pooling": "cls"});
        let read = manifest_of(&spare).unwrap();
        assert_eq!(read.pooling, Pooling::First);
        assert_eq!(read.query_prefix, None);
        assert_eq!(
            (read.quantization, read.base, read.model_version),
            (None, None, None)
        );
    }

    #[test]
    fn a_field_written_empty_is_the_same_as_one_left_out() {
        // An export that writes "" for a field it has nothing for would otherwise give
        // a log line reading `base=` and a prefix that prepends nothing.
        let blank = json!({
            "width": 384, "max_length": 512, "pooling": "mean",
            "query_prefix": "", "base": "",
        });
        let read = manifest_of(&blank).unwrap();
        assert_eq!(read.query_prefix, None);
        assert_eq!(read.base, None);
    }

    #[test]
    fn a_schema_this_build_does_not_read_is_refused_whole() {
        let mut later = whole();
        later["version"] = json!(2);
        assert_eq!(
            manifest_of(&later),
            Err(EmbedError::ArtifactSchema {
                found: 2,
                carried: format!("{SCHEMAS:?}")
            })
        );
        // Absent, the schema is the first one, so an export written before the key
        // existed still reads.
        let mut unsaid = whole();
        unsaid.as_object_mut().unwrap().remove("version");
        assert!(manifest_of(&unsaid).is_ok());
    }

    #[test]
    fn a_pooling_this_build_does_not_know_is_refused() {
        let mut odd = whole();
        odd["pooling"] = json!("attentive");
        assert_eq!(
            manifest_of(&odd),
            Err(EmbedError::UnknownPooling {
                named: "attentive".to_owned()
            })
        );
        assert_eq!(Pooling::named("mean").unwrap(), Pooling::Mean);
        assert_eq!(Pooling::named("cls").unwrap(), Pooling::First);
        assert_eq!(Pooling::named("first").unwrap(), Pooling::First);
        assert_eq!(Pooling::Mean.as_str(), "mean");
        assert_eq!(Pooling::First.as_str(), "cls");
    }

    #[test]
    fn a_manifest_missing_what_cannot_be_guessed_is_refused() {
        for missing in ["width", "max_length", "pooling"] {
            let mut short = whole();
            short.as_object_mut().unwrap().remove(missing);
            assert!(
                matches!(manifest_of(&short), Err(EmbedError::ArtifactUnreadable(_))),
                "a manifest with no {missing} should be refused"
            );
        }
    }

    #[test]
    fn a_width_no_model_has_is_refused_as_a_width() {
        let mut absurd = whole();
        absurd["width"] = json!(0);
        assert_eq!(manifest_of(&absurd), Err(EmbedError::NoWidth));
        absurd["width"] = json!(crate::WIDEST + 1);
        assert!(matches!(
            manifest_of(&absurd),
            Err(EmbedError::TooWide { .. })
        ));
    }

    #[test]
    fn a_length_with_no_room_for_text_in_it_is_refused() {
        // Two tokens is an opening and a closing one with nothing between them.
        let mut tiny = whole();
        for length in [0, 1, 2] {
            tiny["max_length"] = json!(length);
            assert!(
                matches!(manifest_of(&tiny), Err(EmbedError::ArtifactUnreadable(_))),
                "a max_length of {length} should be refused"
            );
        }
        tiny["max_length"] = json!(3);
        assert!(manifest_of(&tiny).is_ok());
    }

    #[test]
    fn weights_that_are_not_the_weights_the_manifest_names_are_refused() {
        assert_eq!(
            check_weights(None, "abc"),
            Ok(()),
            "nothing to disagree with"
        );
        assert_eq!(check_weights(Some("abc"), "abc"), Ok(()));
        assert_eq!(
            check_weights(Some("sha256:abc"), "abc"),
            Ok(()),
            "the prefix an export writes is not part of the digest"
        );
        assert_eq!(
            check_weights(Some("abc"), "def"),
            Err(EmbedError::ArtifactChecksum {
                named: "abc".to_owned(),
                found: "def".to_owned()
            })
        );
    }

    fn pooling_manifest(width: usize) -> Manifest {
        Manifest {
            width: Width::new(width).unwrap(),
            max_length: 512,
            pooling: Pooling::Mean,
            query_prefix: None,
            quantization: None,
            base: None,
            model_version: None,
            digest: "deadbeef".to_owned(),
        }
    }

    #[test]
    fn a_graph_that_pooled_for_itself_is_taken_as_it_stands() {
        let held = pooling_manifest(3);
        let got = pool(&[1, 3], &[1.0, 2.0, 3.0], Pooling::Mean, &held).unwrap();
        assert_eq!(got, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn token_vectors_average_into_one() {
        let held = pooling_manifest(2);
        // Two tokens of two numbers: [1, 2] and [3, 6].
        let got = pool(&[1, 2, 2], &[1.0, 2.0, 3.0, 6.0], Pooling::Mean, &held).unwrap();
        assert_eq!(got, vec![2.0, 4.0]);
    }

    #[test]
    fn the_first_token_alone_is_the_other_way_to_read_them() {
        let held = pooling_manifest(2);
        let got = pool(&[1, 2, 2], &[1.0, 2.0, 3.0, 6.0], Pooling::First, &held).unwrap();
        assert_eq!(got, vec![1.0, 2.0], "the first token, not the average");
    }

    /// The failure the shape check exists for: token vectors read as a pooled one
    /// would be a perfectly plausible vector of the wrong thing, and nothing but the
    /// dimensions can tell.
    #[test]
    fn an_answer_of_the_wrong_shape_is_refused_rather_than_reinterpreted() {
        let held = pooling_manifest(3);
        for (shape, numbers) in [
            (vec![1i64, 4], vec![1.0f32, 2.0, 3.0, 4.0]),
            (vec![1, 2, 2], vec![1.0, 2.0, 3.0, 4.0]),
            (vec![3], vec![1.0, 2.0, 3.0]),
            (vec![1, 0, 3], vec![]),
            (vec![2, 1, 3], vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        ] {
            assert!(
                matches!(
                    pool(&shape, &numbers, Pooling::Mean, &held),
                    Err(EmbedError::WrongOutput { .. })
                ),
                "shape {shape:?} should be refused"
            );
        }
    }

    /// A shape whose dimensions agree but whose buffer does not: the one that would
    /// read past the end if it were trusted.
    #[test]
    fn an_answer_shorter_than_the_shape_it_claims_is_refused() {
        let held = pooling_manifest(3);
        assert!(matches!(
            pool(&[1, 2, 3], &[1.0, 2.0, 3.0], Pooling::Mean, &held),
            Err(EmbedError::WrongOutput { .. })
        ));
    }

    /// The whole opening path — artefact, library, tokenizer, manifest — against a
    /// real export, for whoever runs with `IP_EMBED_ARTIFACT` and a runtime library in
    /// reach. Without both it says so and passes, as the postgres tests do.
    #[test]
    fn a_real_artifact_opens_and_answers() {
        let Ok(dir) = std::env::var("IP_EMBED_ARTIFACT") else {
            eprintln!("IP_EMBED_ARTIFACT unset: no artefact to open");
            return;
        };
        let mut embedder = match std::env::var("ORT_DYLIB_PATH") {
            Ok(library) => Embedder::open_with_library(&dir, library),
            Err(_) => Embedder::open(&dir),
        }
        .expect("the artefact opens");
        let width = embedder.manifest().width;
        let once = embedder.embed("what did we spend last quarter?").unwrap();
        assert_eq!(once.embedding.width(), width);
        // The same text twice is the same vector: the property a shared-session or a
        // leaked-state bug would break, and the one the cache rests on.
        let twice = embedder.embed("what did we spend last quarter?").unwrap();
        assert_eq!(
            once.embedding.similarity(&twice.embedding).unwrap(),
            Nearness::SAME
        );
        // And two texts that mean different things are not the same vector.
        let other = embedder.embed("write me a poem about the sea").unwrap();
        assert!(
            once.embedding.similarity(&other.embedding).unwrap() < Nearness::SAME,
            "two different questions should not embed identically"
        );
        eprintln!("embedded {} numbers in {:?}", width, once.took);
    }

    use crate::Nearness;
}
