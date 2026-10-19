//! The encoder the semantic cache recognises a question by: run locally, never remote.
//!
//! The gateway asks an `Embedder` behind an option; this is the implementation that
//! answers when the binary carries one. The build feature is the door: without
//! `semantic` the binary never loads an embedding model, and a config asking for one
//! is refused at startup, loudly, rather than serving with the level silently off.

#[cfg(feature = "semantic")]
mod encoder {
    use ip_config::SemanticConfig;
    use ip_embed::runtime::{Embedder as Encoder, Manifest};
    use ip_embed::{Embedding, Width};

    /// A loaded embedding model behind a lock.
    ///
    /// Weights are not read-mostly state, they are a machine: one forward pass at a
    /// time, and one that is busy answers nothing rather than queueing. An arriving
    /// request goes upstream now, which is what it would have done anyway — the whole
    /// level is an optimisation, and an optimisation that makes requests wait for each
    /// other has stopped being one.
    pub struct RuntimeEmbedder {
        encoder: std::sync::Mutex<Encoder>,
        width: Width,
        model: String,
    }

    impl std::fmt::Debug for RuntimeEmbedder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("RuntimeEmbedder")
                .field("width", &self.width)
                .field("model", &self.model)
                .finish_non_exhaustive()
        }
    }

    impl RuntimeEmbedder {
        /// Opens the encoder from the operator's stanza: the artefact directory, and
        /// the library where the config names one — the loader's own search answers
        /// where it stays silent. Any failure is the string the caller refuses
        /// startup with.
        pub fn open(config: &SemanticConfig) -> Result<Self, String> {
            let encoder = match &config.library {
                Some(library) => Encoder::open_with_library(&config.artifact, library),
                None => Encoder::open(&config.artifact),
            }
            .map_err(|error| error.to_string())?;
            said(encoder.manifest());
            let mut embedder = Self {
                width: encoder.manifest().width,
                model: encoder.manifest().digest.clone(),
                encoder: std::sync::Mutex::new(encoder),
            };
            embedder.warm()?;
            Ok(embedder)
        }

        /// One forward pass at open, its answer dropped and its failure not: a graph
        /// that loads but will not run must refuse startup, never come up and miss
        /// every lookup in silence.
        ///
        /// It is also where the runtime's first pass is paid for, so the bill lands
        /// here rather than on a real request — and where an artefact whose manifest
        /// promises a width its graph does not answer with is found out, because
        /// that disagreement is a refusal from `pool` and not a bad vector.
        fn warm(&mut self) -> Result<(), String> {
            self.encoder
                .get_mut()
                .map_err(|_| "the encoder was poisoned before its first pass".to_owned())?
                .embed("warm-up")
                .map(|_| ())
                .map_err(|error| error.to_string())
        }

        /// How many numbers this model's vectors have, which the cache is built to
        /// hold and will refuse anything else.
        #[must_use]
        pub fn width(&self) -> Width {
            self.width
        }

        /// What identifies these weights in a cache key: the digest of the file, so
        /// two models can never share an entry.
        #[must_use]
        pub fn model(&self) -> &str {
            &self.model
        }
    }

    /// What was loaded, in the one line an operator reads when a gateway comes up.
    ///
    /// "Embedder loaded" says nothing worth saying. Which model came up decides every
    /// hit and every miss this level will ever have, and the digest is what an
    /// operator checks against what they meant to deploy — a version string is what
    /// somebody remembered to bump.
    fn said(manifest: &Manifest) {
        tracing::info!(
            width = manifest.width.get(),
            pooling = manifest.pooling.as_str(),
            build = manifest.quantization.as_deref().unwrap_or("unnamed"),
            model = manifest.model_version.as_deref().unwrap_or("unversioned"),
            base = manifest.base.as_deref().unwrap_or("unnamed"),
            // Enough to tell two artefacts apart in a log, not the whole digest.
            digest = &manifest.digest[..manifest.digest.len().min(12)],
            "embedding model loaded"
        );
    }

    impl ip_gateway::Embedder for RuntimeEmbedder {
        fn embed(&self, text: &str) -> Option<Embedding> {
            // Busy means now, not eventually; the type says why.
            let mut encoder = match self.encoder.try_lock() {
                Ok(encoder) => encoder,
                Err(std::sync::TryLockError::WouldBlock) => return None,
                // One panic must not switch the level off for the process's life.
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    tracing::warn!("the encoder recovered from a poisoned inference lock");
                    poisoned.into_inner()
                }
            };
            match encoder.embed(text) {
                Ok(embedded) => Some(embedded.embedding),
                // A silent degradation is indistinguishable from a cache that never
                // hits, which is exactly how this would be misread.
                Err(error) => {
                    let kind = match &error {
                        ip_embed::EmbedError::Onnx(onnx) => match onnx {
                            ip_onnx::OnnxError::Inference(_) => "inference",
                            ip_onnx::OnnxError::Tokenize(_) => "tokenize",
                            _ => "artefact error",
                        },
                        _ => "embedder error",
                    };
                    tracing::warn!(kind, "embedding failed; the question is not recognised");
                    None
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use ip_gateway::Embedder;

        /// The whole opening path against a real export, for whoever runs with
        /// `IP_EMBED_ARTIFACT` and a runtime library in reach. Without both it says so
        /// and passes, as the postgres tests do.
        #[test]
        fn a_real_artifact_opens_and_answers_through_the_embedder() {
            let Ok(dir) = std::env::var("IP_EMBED_ARTIFACT") else {
                eprintln!("IP_EMBED_ARTIFACT unset: no artefact to open");
                return;
            };
            let config = SemanticConfig {
                artifact: std::path::PathBuf::from(dir),
                nearness: 0.97,
                ttl: ip_config::Seconds::new(3_600),
                capacity: 4_096,
                library: std::env::var("ORT_DYLIB_PATH")
                    .ok()
                    .map(std::path::PathBuf::from),
            };
            let embedder = RuntimeEmbedder::open(&config).expect("the artefact opens");
            assert!(!embedder.model().is_empty(), "the weights have a digest");
            // Whatever the model says of this text, it says it twice: the property a
            // lock-poisoning or shared-session bug would break, and the one the cache
            // rests on — a question that embedded differently each time would never
            // hit its own answer.
            let once = embedder.embed("what did we spend last quarter?");
            let twice = embedder.embed("what did we spend last quarter?");
            assert_eq!(once, twice);
            assert_eq!(
                once.expect("a vector").width(),
                embedder.width(),
                "the manifest's width is what comes back"
            );
        }
    }
}

#[cfg(feature = "semantic")]
pub use encoder::RuntimeEmbedder;
