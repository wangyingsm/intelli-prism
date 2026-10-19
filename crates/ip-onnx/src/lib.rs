//! The ONNX Runtime, and what an exported model's directory has to hold.
//!
//! Two crates run a model: `ip-classify` judges a turn's difficulty and `ip-embed`
//! recognises a question. What they do with a model differs entirely — a softmax over
//! five logits against a pooled vector of several hundred numbers — and what it takes
//! to *get* to one does not differ at all. This is that part: find the runtime, check
//! the directory holds what it must, prove the weights are the ones the manifest
//! speaks for, open the graph and the tokenizer.
//!
//! # The runtime library, and which one
//!
//! The ONNX Runtime is dynamically loaded (`load-dynamic`): nothing here builds
//! against a native dependency, and the library is found at open time by name,
//! searched as `dlopen` searches — `$ORIGIN`, `LD_LIBRARY_PATH`, the system cache.
//! [`load`] resolves it **through a call that returns**: an absent or unloadable
//! library is [`OnnxError::RuntimeMissing`], always a value. (The default loading
//! route of `ort` resolves lazily inside the session builder and aborts the process
//! when it finds nothing — this crate never takes that route.)
//!
//! The version is not a free variable: `ort 2.0.0-rc.13` is built against **ONNX
//! Runtime 1.28.0**, which is what this was verified against. A newer library on the
//! search path is untested territory — the symbols load, the graph may run, and the
//! numbers are then no longer the ones measured.
//!
//! Where the library may *not* live: a package manager's cache directory. The
//! artifacts under such a tree are content-addressed and a routine cache prune
//! deletes them out from under a running configuration; it belongs in a system
//! library directory or next to the binary, where it is the operator's to keep.
//!
//! # One library for the process, and why that is enforced here
//!
//! `ort` holds the loaded library in a process-global cell, so the first successful
//! load owns the choice and every model afterwards runs against it. That is the
//! runtime's rule and not ours. What *is* ours is what happens when a second caller
//! asks for a different one: `ort` quietly ignores it, which let a configuration name
//! two libraries, honour one and say nothing. [`load`] remembers what it loaded and
//! refuses the disagreement instead.

pub mod error;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub use error::OnnxError;

// Re-exported rather than depended on twice. A crate that runs a model needs the
// session, the tensors and the tokenizer, and the one thing that must not happen is
// two versions of `ort` in one process — its loaded library and its environment are
// process-global, so a second copy would be a second global disagreeing with the
// first. Reaching them through here makes that unrepresentable.
pub use ort;
pub use serde_json;
pub use tokenizers;

/// The environment variable naming the runtime library, if the operator set one.
pub const DYLIB_ENV: &str = "ORT_DYLIB_PATH";

/// The library name the search resolves when nothing better is offered: the SONAME of
/// a release build, as `dlopen` looks it up.
const DEFAULT_LIBRARY: &str = "libonnxruntime.so";

/// What was loaded, and by what name it was asked for.
///
/// A `Mutex` rather than a `OnceLock` so that a *failed* load leaves nothing behind:
/// the next caller, perhaps naming a library that does exist, must still be able to
/// try. Only a load that succeeded settles the choice.
static LOADED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The path the runtime library was searched for under `ORT_DYLIB_PATH`, if set.
#[must_use]
pub fn library_from_env() -> Option<PathBuf> {
    std::env::var_os(DYLIB_ENV).map(PathBuf::from)
}

/// Loads the ONNX Runtime, once for the process.
///
/// `library` is what a configuration named, which outranks the environment, which
/// outranks the plain search. Calling this again with the same answer is free;
/// calling it with a different one is [`OnnxError::RuntimeDisagrees`], because the
/// second cannot be honoured and pretending otherwise is how a model comes to run
/// against a library nobody chose.
pub fn load(library: Option<&Path>) -> Result<(), OnnxError> {
    let candidate = library
        .map(PathBuf::from)
        .or_else(library_from_env)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LIBRARY));
    // One panic in here must not leave the process unable to load a model at all.
    let mut held = LOADED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(already) = held.as_ref() {
        return if *already == candidate {
            Ok(())
        } else {
            Err(OnnxError::RuntimeDisagrees {
                loaded: already.clone(),
                asked: candidate,
            })
        };
    }
    let builder = ort::init_from(&candidate)
        .map_err(|error| OnnxError::RuntimeMissing(format!("{candidate:?}: {error}")))?;
    let _environment = builder.commit();
    *held = Some(candidate);
    Ok(())
}

/// The three files an exported model's directory holds.
///
/// The manifest's name differs between models — a classifier's gate, an encoder's
/// description — and nothing else does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    model: PathBuf,
    tokenizer: PathBuf,
    manifest: PathBuf,
}

impl Artifact {
    /// Names the files of an artefact directory, refusing one that is not there.
    ///
    /// Checked before anything else is attempted, because a missing file is news that
    /// no installed library changes: it is reported identically on a machine with a
    /// runtime and one without.
    pub fn at(dir: &Path, manifest: &str) -> Result<Self, OnnxError> {
        let found = Self {
            model: dir.join("model.onnx"),
            tokenizer: dir.join("tokenizer.json"),
            manifest: dir.join(manifest),
        };
        for required in [&found.model, &found.tokenizer, &found.manifest] {
            if !required.exists() {
                return Err(OnnxError::ArtifactMissing(format!(
                    "{} — an artefact directory holds model.onnx, tokenizer.json and {manifest}",
                    required.display()
                )));
            }
        }
        Ok(found)
    }

    /// Where the weights are.
    #[must_use]
    pub fn model(&self) -> &Path {
        &self.model
    }

    /// Where the manifest is.
    #[must_use]
    pub fn manifest(&self) -> &Path {
        &self.manifest
    }

    /// The manifest as json, and the digest of the weights beside it, with the
    /// manifest's own checksum already checked against them.
    pub fn read(&self) -> Result<Read, OnnxError> {
        let manifest = read_json(&self.manifest)?;
        let digest = digest(&self.model)?;
        check_weights(text_at(&manifest, "checksum").as_deref(), &digest)?;
        Ok(Read { manifest, digest })
    }

    /// The tokenizer, as exported. Truncation is the caller's to set, because which
    /// end of a long text to keep is a property of what the model is being asked.
    pub fn tokenizer(&self) -> Result<tokenizers::Tokenizer, OnnxError> {
        tokenizers::Tokenizer::from_file(&self.tokenizer).map_err(|error| {
            OnnxError::ArtifactUnreadable(format!("{}: {error}", self.tokenizer.display()))
        })
    }

    /// The graph, opened.
    pub fn session(&self) -> Result<ort::session::Session, OnnxError> {
        ort::session::Session::builder()
            .map_err(|error| OnnxError::Inference(error.to_string()))?
            .commit_from_file(&self.model)
            .map_err(|error| OnnxError::GraphRefused(format!("{}: {error}", self.model.display())))
    }
}

/// A manifest and the weights it was read beside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    /// The manifest, as json for its own crate to make sense of.
    pub manifest: serde_json::Value,
    /// The digest of the weights as they are on disk.
    ///
    /// Always computed, whether or not the manifest names one, because this is what
    /// tells two models apart downstream. A version string is what somebody
    /// remembered to bump; this is what was actually loaded.
    pub digest: String,
}

/// Reads a json file, refusing one that is not as itself.
pub fn read_json(path: &Path) -> Result<serde_json::Value, OnnxError> {
    let unreadable = || OnnxError::ArtifactUnreadable(path.display().to_string());
    let text = std::fs::read_to_string(path).map_err(|_| unreadable())?;
    serde_json::from_str(&text).map_err(|_| unreadable())
}

/// Refuses a manifest written to a shape this build does not read.
///
/// A manifest with no version at all is the first schema, so an export written before
/// the key existed still reads.
pub fn schema(manifest: &serde_json::Value, carried: &[u64]) -> Result<(), OnnxError> {
    let found = manifest
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    if carried.contains(&found) {
        return Ok(());
    }
    Err(OnnxError::ArtifactSchema {
        found,
        carried: format!("{carried:?}"),
    })
}

/// A string field of a manifest, when it carries one that is not empty.
#[must_use]
pub fn text_at(manifest: &serde_json::Value, key: &str) -> Option<String> {
    manifest
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The digest of a file, read in pieces so a file of any size costs one buffer.
pub fn digest(path: &Path) -> Result<String, OnnxError> {
    use sha2::Digest;
    use std::io::Read;
    let unreadable = || OnnxError::ArtifactUnreadable(path.display().to_string());
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

/// Refuses weights that are not the ones the manifest was written for, when the
/// manifest says which those were.
///
/// Only then: one written before checksums existed has nothing to disagree with, and
/// refusing it would refuse every artefact built before this could be checked.
pub fn check_weights(named: Option<&str>, found: &str) -> Result<(), OnnxError> {
    let Some(named) = named else {
        return Ok(());
    };
    let named = named.strip_prefix("sha256:").unwrap_or(named);
    if found != named {
        return Err(OnnxError::ArtifactChecksum {
            named: named.to_owned(),
            found: found.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A directory of this test's own, distinct per test and per process.
    struct Scratch(PathBuf);

    impl Scratch {
        fn holding(tag: &str, files: &[(&str, &[u8])]) -> Self {
            let at = std::env::temp_dir().join(format!("ip-onnx-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&at);
            std::fs::create_dir_all(&at).expect("a scratch directory");
            for (name, bytes) in files {
                std::fs::write(at.join(name), bytes).expect("a scratch file");
            }
            Self(at)
        }

        fn at(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const WHOLE: [(&str, &[u8]); 3] = [
        ("model.onnx", b"weights"),
        ("tokenizer.json", b"{}"),
        ("gate.json", b"{}"),
    ];

    /// The bug this crate exists to make impossible.
    ///
    /// `ort` holds the library in a process-global cell and silently ignores a second
    /// load naming another path, so a configuration could ask for two and be told
    /// nothing. Each of these runs in a process of its own, which is what lets a
    /// global be tested at all.
    #[test]
    fn asking_for_a_second_runtime_is_refused_rather_than_ignored() {
        let Some(library) = library_from_env() else {
            eprintln!("ORT_DYLIB_PATH unset: nothing to load a first time");
            return;
        };
        load(Some(&library)).expect("the library loads");
        load(Some(&library)).expect("the same library again costs nothing");
        let other = PathBuf::from("/nonexistent/libonnxruntime.so");
        let Err(OnnxError::RuntimeDisagrees { loaded, asked }) = load(Some(&other)) else {
            panic!("a second, different library must be refused and not ignored");
        };
        assert_eq!(loaded, library, "what is loaded is what loaded first");
        assert_eq!(asked, other);
    }

    /// And a load that failed settles nothing: the next caller, perhaps naming a
    /// library that does exist, must still be able to try. This is why the choice is
    /// held in a `Mutex` and not a `OnceLock`.
    #[test]
    fn a_library_that_will_not_load_leaves_the_choice_open() {
        let missing = PathBuf::from("/nonexistent/libonnxruntime.so");
        assert!(matches!(
            load(Some(&missing)),
            Err(OnnxError::RuntimeMissing(_))
        ));
        let Some(library) = library_from_env() else {
            eprintln!("ORT_DYLIB_PATH unset: no good library to try afterwards");
            return;
        };
        load(Some(&library)).expect("a failed load must not have settled the choice");
    }

    #[test]
    fn an_artefact_names_the_files_it_was_built_from() {
        let dir = Scratch::holding("whole", &WHOLE);
        let found = Artifact::at(dir.at(), "gate.json").expect("all three are there");
        assert_eq!(found.model(), dir.at().join("model.onnx"));
        assert_eq!(found.manifest(), dir.at().join("gate.json"));
    }

    /// A half-copied export is the likeliest way this goes wrong, so each file's
    /// absence is reported as itself and names what is missing.
    #[test]
    fn a_directory_missing_a_file_is_refused_and_says_which() {
        for missing in ["model.onnx", "tokenizer.json", "gate.json"] {
            let kept: Vec<(&str, &[u8])> = WHOLE
                .iter()
                .filter(|(name, _)| *name != missing)
                .copied()
                .collect();
            let dir = Scratch::holding(&format!("without-{missing}"), &kept);
            let Err(OnnxError::ArtifactMissing(said)) = Artifact::at(dir.at(), "gate.json") else {
                panic!("an artefact with no {missing} must be refused");
            };
            assert!(said.contains(missing), "{said} should name {missing}");
        }
    }

    /// The manifest's own name travels with it, so the message tells an operator
    /// which file this kind of artefact wants rather than some other kind's.
    #[test]
    fn the_manifest_is_named_by_whoever_is_opening_the_artefact() {
        let dir = Scratch::holding(
            "embed-shaped",
            &[("model.onnx", b"weights"), ("tokenizer.json", b"{}")],
        );
        let Err(OnnxError::ArtifactMissing(said)) = Artifact::at(dir.at(), "embed.json") else {
            panic!("a missing embed.json must be refused");
        };
        assert!(said.contains("embed.json"), "{said}");
        assert!(!said.contains("gate.json"), "{said}");
    }

    #[test]
    fn a_manifest_and_its_weights_are_read_together() {
        let dir = Scratch::holding(
            "read",
            &[
                ("model.onnx", b"weights"),
                ("tokenizer.json", b"{}"),
                ("gate.json", br#"{"version":1,"temperature":1.3}"#),
            ],
        );
        let read = Artifact::at(dir.at(), "gate.json").unwrap().read().unwrap();
        assert_eq!(read.manifest["temperature"], json!(1.3));
        assert_eq!(read.digest, digest(&dir.at().join("model.onnx")).unwrap());
    }

    #[test]
    fn a_manifest_naming_the_wrong_weights_is_refused_when_they_are_read() {
        let dir = Scratch::holding(
            "wrong-digest",
            &[
                ("model.onnx", b"weights"),
                ("tokenizer.json", b"{}"),
                ("gate.json", br#"{"checksum":"sha256:0000"}"#),
            ],
        );
        let found = Artifact::at(dir.at(), "gate.json").unwrap().read();
        assert!(matches!(found, Err(OnnxError::ArtifactChecksum { .. })));
    }

    #[test]
    fn a_manifest_that_is_not_json_is_refused_as_the_file_it_is() {
        let dir = Scratch::holding(
            "not-json",
            &[
                ("model.onnx", b"weights"),
                ("tokenizer.json", b"{}"),
                ("gate.json", b"temperature = 1.3"),
            ],
        );
        let Err(OnnxError::ArtifactUnreadable(said)) =
            Artifact::at(dir.at(), "gate.json").unwrap().read()
        else {
            panic!("a manifest that is not json must be refused");
        };
        assert!(said.contains("gate.json"), "{said}");
    }

    #[test]
    fn a_schema_this_build_does_not_read_is_refused_whole() {
        assert_eq!(schema(&json!({"version": 1}), &[1, 2]), Ok(()));
        assert_eq!(schema(&json!({"version": 2}), &[1, 2]), Ok(()));
        assert_eq!(
            schema(&json!({"version": 99}), &[1, 2]),
            Err(OnnxError::ArtifactSchema {
                found: 99,
                carried: "[1, 2]".to_owned()
            })
        );
        // Absent, it is the first schema: an export written before the key existed
        // still reads.
        assert_eq!(schema(&json!({}), &[1]), Ok(()));
    }

    #[test]
    fn weights_that_are_not_the_ones_the_manifest_names_are_refused() {
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
            Err(OnnxError::ArtifactChecksum {
                named: "abc".to_owned(),
                found: "def".to_owned()
            })
        );
    }

    #[test]
    fn a_field_written_empty_is_the_same_as_one_left_out() {
        let manifest = json!({"base": "", "quantization": "int8"});
        assert_eq!(text_at(&manifest, "base"), None);
        assert_eq!(text_at(&manifest, "quantization"), Some("int8".to_owned()));
        assert_eq!(text_at(&manifest, "absent"), None);
    }

    #[test]
    fn a_digest_is_of_the_bytes_and_nothing_else() {
        let dir = Scratch::holding("digest", &[("model.onnx", b"weights")]);
        // sha256 of "weights", so the reading in a megabyte at a time is checked
        // against a figure from outside this code rather than against itself.
        assert_eq!(
            digest(&dir.at().join("model.onnx")).unwrap(),
            "9a129038d9a00aed0cf6a7ea059ca50a813449061ab87848cf1a13eafdf33b2c"
        );
    }

    #[test]
    fn a_file_that_is_not_there_has_no_digest() {
        let dir = Scratch::holding("no-digest", &[]);
        assert!(matches!(
            digest(&dir.at().join("model.onnx")),
            Err(OnnxError::ArtifactUnreadable(_))
        ));
    }
}
