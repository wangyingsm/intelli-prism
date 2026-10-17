//! Getting the classifier's artefact onto a machine, and proving it is the right one.
//!
//! The gateway never fetches anything. This product's claim is that enterprise data
//! does not leave, and a process that reaches the internet at startup to pull a binary
//! is the first thing a security review stops at. So fetching is an operator action,
//! taken once, before the gateway runs — and carrying an artefact in on a disk is not
//! a degraded path but the primary one, which is why `verify` touches no network at
//! all and is the half that must never rot.
//!
//! Verification is not a flag. An artefact that fails its checksum is deleted rather
//! than left with a warning beside it: a half-downloaded three hundred megabytes that
//! looks present is worse than nothing being there.

use std::path::{Path, PathBuf};

use ip_classify::runtime::Gate;

use crate::cli::ModelCommand;
use crate::error::StartupError;

/// The files an artefact directory holds. `model.onnx` is named by the manifest's own
/// checksum, so it is fetched last and verified before anything is said to have worked.
const FILES: [&str; 4] = ["gate.json", "config.json", "tokenizer.json", "model.onnx"];

/// How many redirects a download follows. Both hosts answer from a cdn, so one hop is
/// ordinary and five is already generous.
const REDIRECTS: usize = 5;

/// Where an artefact is being fetched from.
///
/// The two hosts are first-class and neither is a fallback: the model is half Chinese
/// and Hugging Face is slow from much of Asia, so a mirror that is only tried second
/// would be a mirror that does not work. An operator may also name their own url or a
/// path, because an enterprise will want this on its own artefact store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// A directory already on this machine, which is the air-gapped path.
    Here(PathBuf),
    /// Anything reachable over http, each file beneath this prefix.
    Over(String),
}

impl Source {
    /// Reads what an operator named: a path, a url, or a repository on either host.
    ///
    /// There is no default repository. The hosts' url shapes are built in — that is
    /// what "a sensible default" can honestly mean here — but which repository holds
    /// the weights is a fact about a release, and a constant guessing at it would be
    /// a lie compiled into the binary.
    pub(crate) fn read(named: &str) -> Result<Self, String> {
        if let Some(repo) = named.strip_prefix("hf:") {
            return Ok(Self::Over(format!(
                "https://huggingface.co/{repo}/resolve/main"
            )));
        }
        if let Some(repo) = named.strip_prefix("ms:") {
            return Ok(Self::Over(format!(
                "https://modelscope.cn/models/{repo}/resolve/master"
            )));
        }
        if named.starts_with("http://") || named.starts_with("https://") {
            return Ok(Self::Over(named.trim_end_matches('/').to_owned()));
        }
        let path = PathBuf::from(named);
        if path.is_dir() {
            return Ok(Self::Here(path));
        }
        Err(format!(
            "{named} is not a directory, a url, or a repository named `hf:owner/name` \
             or `ms:owner/name`"
        ))
    }
}

/// Runs what the operator asked for.
pub(crate) async fn run(command: ModelCommand) -> Result<(), StartupError> {
    match command {
        ModelCommand::Verify { at } => {
            let gate = verify(&at)?;
            said(&at, &gate);
            Ok(())
        }
        ModelCommand::Fetch {
            from,
            into,
            replace,
        } => {
            let source = Source::read(&from).map_err(|detail| StartupError::Artefact { detail })?;
            let gate = fetch(&source, &into, replace).await?;
            said(&into, &gate);
            Ok(())
        }
    }
}

/// Checks an artefact where it already is, reaching nothing.
///
/// This is the air-gapped path and the one most likely to rot untested, so it shares
/// no code with fetching beyond the reading itself: nothing here can grow a network
/// call without the call being visible on this function.
pub(crate) fn verify(at: &Path) -> Result<Gate, StartupError> {
    for name in FILES {
        if !at.join(name).exists() {
            return Err(StartupError::Artefact {
                detail: format!(
                    "{} is missing — weights without a gate are not usable, and an \
                     artefact directory holds {}",
                    at.join(name).display(),
                    FILES.join(", ")
                ),
            });
        }
    }
    // Opening the manifest is what verifies the weights: it refuses a schema it does
    // not read and a checksum that does not match, and that refusal is the whole point.
    Gate::read(&at.join("gate.json"), &at.join("model.onnx")).map_err(|error| {
        StartupError::Artefact {
            detail: error.to_string(),
        }
    })
}

/// What was got, so the threshold decision can be made where the information is.
fn said(at: &Path, gate: &Gate) {
    let size = FILES
        .iter()
        .filter_map(|name| at.join(name).metadata().ok())
        .map(|held| held.len())
        .sum::<u64>();
    println!("verified {}", at.display());
    println!(
        "  build        {}",
        gate.quantization.as_deref().unwrap_or("unnamed")
    );
    println!(
        "  model        {}",
        gate.model_version.as_deref().unwrap_or("unversioned")
    );
    println!(
        "  base         {}",
        gate.base.as_deref().unwrap_or("unnamed")
    );
    println!("  temperature  {}", gate.temperature.value());
    println!("  size         {:.0} MB", size as f64 / 1e6);
    if gate.margin.is_empty() {
        println!("  margin       this artefact carries no table");
        return;
    }
    println!("  margin       threshold   right    kept");
    for row in &gate.margin {
        println!(
            "               {:>9.2} {:>6.1}% {:>6.1}%",
            row.threshold,
            row.accuracy * 100.0,
            row.coverage * 100.0
        );
    }
}

/// Places every file of an artefact and proves it, or leaves nothing behind.
///
/// Carrying and proving are one step on purpose. Verification is not a flag and not a
/// second command an operator might forget: weights that fail their checksum are
/// deleted, because three hundred megabytes that look present are worse than an empty
/// directory — a gateway finds the first and refuses to start on the second.
async fn fetch(source: &Source, into: &Path, replace: bool) -> Result<Gate, StartupError> {
    let standing: Vec<&str> = FILES
        .iter()
        .copied()
        .filter(|name| into.join(name).exists())
        .collect();
    if !standing.is_empty() && !replace {
        return Err(StartupError::Artefact {
            detail: format!(
                "{} already holds {} — a running gateway may be serving from it, so \
                 replacing it is asked for with --replace",
                into.display(),
                standing.join(", ")
            ),
        });
    }
    std::fs::create_dir_all(into).map_err(|source| StartupError::Artefact {
        detail: format!("{} cannot be created: {source}", into.display()),
    })?;

    let got = match carry(source, into).await {
        Ok(()) => verify(into),
        Err(error) => Err(error),
    };
    if got.is_err() {
        for name in FILES {
            let _ = std::fs::remove_file(into.join(name));
        }
    }
    got
}

/// Every file, from wherever the source is.
async fn carry(source: &Source, into: &Path) -> Result<(), StartupError> {
    for name in FILES {
        match source {
            Source::Here(from) => {
                let from = from.join(name);
                eprintln!("copying {}", from.display());
                std::fs::copy(&from, into.join(name)).map_err(|error| StartupError::Artefact {
                    detail: format!("{} cannot be read: {error}", from.display()),
                })?;
            }
            Source::Over(prefix) => {
                let url = format!("{prefix}/{name}");
                eprintln!("fetching {url}");
                let bytes = download(&url).await?;
                std::fs::write(into.join(name), bytes).map_err(|error| StartupError::Artefact {
                    detail: format!("{} cannot be written: {error}", into.join(name).display()),
                })?;
            }
        }
    }
    Ok(())
}

/// One file over http, following the redirect both hosts answer with.
///
/// Built on the same client stack the gateway forwards with, so this adds no second
/// way of speaking tls to the binary.
async fn download(url: &str) -> Result<Vec<u8>, StartupError> {
    use http_body_util::BodyExt;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    let client: Client<_, String> = Client::builder(TokioExecutor::new()).build(connector);

    let mut at = url.to_owned();
    for _ in 0..=REDIRECTS {
        let asked = hyper::Request::get(&at)
            .header(hyper::header::USER_AGENT, "intelli-prism")
            .body(String::new())
            .map_err(|error| StartupError::Artefact {
                detail: format!("{at} is not a url this can ask for: {error}"),
            })?;
        let answered = client
            .request(asked)
            .await
            .map_err(|error| StartupError::Artefact {
                detail: format!("{at} could not be reached: {error}"),
            })?;
        let status = answered.status();
        if status.is_redirection() {
            let Some(next) = answered
                .headers()
                .get(hyper::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                return Err(StartupError::Artefact {
                    detail: format!("{at} answered {status} and named nowhere to go"),
                });
            };
            at = next.to_owned();
            continue;
        }
        if !status.is_success() {
            return Err(StartupError::Artefact {
                detail: format!("{at} answered {status}"),
            });
        }
        let body =
            answered
                .into_body()
                .collect()
                .await
                .map_err(|error| StartupError::Artefact {
                    detail: format!("{at} could not be read to its end: {error}"),
                })?;
        return Ok(body.to_bytes().to_vec());
    }
    Err(StartupError::Artefact {
        detail: format!("{url} redirected more than {REDIRECTS} times"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real artefact in miniature: a manifest whose checksum is of the weights
    /// beside it, so verifying is a true check rather than a shape check.
    fn artefact(at: &Path, weights: &[u8]) {
        use sha2::Digest;
        std::fs::create_dir_all(at).unwrap();
        let digest = hex::encode(sha2::Sha256::digest(weights));
        std::fs::write(at.join("model.onnx"), weights).unwrap();
        std::fs::write(at.join("tokenizer.json"), b"{}").unwrap();
        std::fs::write(at.join("config.json"), b"{}").unwrap();
        std::fs::write(
            at.join("gate.json"),
            format!(
                r#"{{"version": 2, "temperature": 1.3, "max_length": 256,
                   "classes": ["easy", "routine", "median", "hard", "research"],
                   "quantization": "embed8", "checksum": "sha256:{digest}",
                   "margin": [{{"threshold": 0.4, "accuracy": 0.79, "coverage": 0.59}}],
                   "base": "jhu-clsp/mmBERT-base", "model_version": "1"}}"#
            ),
        )
        .unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let at = std::env::temp_dir().join(format!("ip-model-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        at
    }

    /// The air-gapped path, which is the primary one: an operator carried the artefact
    /// in and this machine has no route out. Nothing here may reach anything, and this
    /// test is the guard on that, since it is the half most likely to rot untested.
    #[test]
    fn an_artefact_carried_in_by_hand_is_checked_with_no_source_at_all() {
        let at = scratch("carried");
        artefact(&at, b"the weights, such as they are");
        let gate = verify(&at).expect("an artefact that is what it says");
        assert_eq!(gate.quantization.as_deref(), Some("embed8"));
        assert_eq!(gate.margin.len(), 1);
        // Reading it asked for no source, so there was nothing to reach.
        said(&at, &gate);
    }

    #[test]
    fn weights_that_are_not_what_the_manifest_says_are_refused() {
        let at = scratch("swapped");
        artefact(&at, b"the weights, such as they are");
        std::fs::write(at.join("model.onnx"), b"something else entirely").unwrap();
        let refused = verify(&at).expect_err("a digest that does not match");
        assert!(
            refused.to_string().contains("do not match the manifest"),
            "{refused}"
        );
    }

    #[test]
    fn weights_without_a_gate_are_not_an_artefact() {
        let at = scratch("gateless");
        artefact(&at, b"weights");
        std::fs::remove_file(at.join("gate.json")).unwrap();
        let refused = verify(&at).expect_err("weights alone");
        assert!(
            refused.to_string().contains("gate.json is missing"),
            "{refused}"
        );
        assert!(refused.to_string().contains("not usable"), "{refused}");
    }

    #[tokio::test]
    async fn a_fetch_from_a_directory_places_the_artefact_and_proves_it() {
        let from = scratch("source");
        let into = scratch("placed");
        artefact(&from, b"the weights, such as they are");

        run(ModelCommand::Fetch {
            from: from.display().to_string(),
            into: into.clone(),
            replace: false,
        })
        .await
        .expect("a fetch from a directory");

        for name in FILES {
            assert!(into.join(name).exists(), "{name} was not placed");
        }
        verify(&into).expect("and what was placed is what it claims");
    }

    /// Half an artefact that looks present is worse than none, so a fetch that fails
    /// its check leaves nothing for a gateway to find.
    #[tokio::test]
    async fn a_fetch_that_cannot_be_proved_leaves_nothing_behind() {
        let from = scratch("corrupt-source");
        let into = scratch("corrupt-placed");
        artefact(&from, b"the weights, such as they are");
        // The manifest now speaks for weights that are not these.
        std::fs::write(from.join("model.onnx"), b"truncated").unwrap();

        let refused = run(ModelCommand::Fetch {
            from: from.display().to_string(),
            into: into.clone(),
            replace: false,
        })
        .await
        .expect_err("weights that are not what the manifest says");
        assert!(refused.to_string().contains("do not match"), "{refused}");

        for name in FILES {
            assert!(
                !into.join(name).exists(),
                "{name} was left behind for a gateway to find"
            );
        }
    }

    /// A running gateway may be serving from the directory being written.
    #[tokio::test]
    async fn an_artefact_already_there_is_not_replaced_unasked() {
        let from = scratch("replace-source");
        let into = scratch("replace-placed");
        artefact(&from, b"the new weights");
        artefact(&into, b"the weights already serving");

        let refused = run(ModelCommand::Fetch {
            from: from.display().to_string(),
            into: into.clone(),
            replace: false,
        })
        .await
        .expect_err("an artefact already there");
        assert!(refused.to_string().contains("--replace"), "{refused}");
        assert_eq!(
            std::fs::read(into.join("model.onnx")).unwrap(),
            b"the weights already serving",
            "what was serving was overwritten"
        );

        run(ModelCommand::Fetch {
            from: from.display().to_string(),
            into: into.clone(),
            replace: true,
        })
        .await
        .expect("asked for, it replaces");
        assert_eq!(
            std::fs::read(into.join("model.onnx")).unwrap(),
            b"the new weights"
        );
    }

    #[test]
    fn a_source_is_a_path_a_url_or_a_repository_on_either_host() {
        let here = scratch("source-read");
        std::fs::create_dir_all(&here).unwrap();
        assert_eq!(
            Source::read(&here.display().to_string()).unwrap(),
            Source::Here(here)
        );
        assert_eq!(
            Source::read("hf:owner/name").unwrap(),
            Source::Over("https://huggingface.co/owner/name/resolve/main".to_owned())
        );
        assert_eq!(
            Source::read("ms:owner/name").unwrap(),
            Source::Over("https://modelscope.cn/models/owner/name/resolve/master".to_owned())
        );
        assert_eq!(
            Source::read("https://files.corp.example/difficulty/").unwrap(),
            Source::Over("https://files.corp.example/difficulty".to_owned()),
            "a trailing slash does not become a double one"
        );
        let refused = Source::read("not-a-source").expect_err("neither a path nor a url");
        assert!(refused.contains("hf:owner/name"), "{refused}");
    }
}
