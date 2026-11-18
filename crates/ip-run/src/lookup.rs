//! Running one look-up a model asked for.
//!
//! The four steps between a tool call and the text a model reads: what it passed, read into what
//! the tool takes; the words, embedded by this deployment's own artefact; the backend, asked; and
//! the rows, rendered.
//!
//! **Nothing here ends a run**, which is [`Toolshed::run`](crate::Toolshed::run)'s rule and the
//! reason every refusal in this module is a [`Produced`] with `failed` set rather than an error:
//! a model told its look-up was wrong can fix the arguments and ask again, and often does. The
//! one refusal that is *not* here is the deployment having no embedder at all — that is settled
//! before the first turn, where a run can still be refused with a status code.

use std::sync::Arc;

use ip_embed::Embedding;
use ip_gateway::Embedder;
use ip_mcp::Produced;
use ip_retrieve::{LookUp, Looked, Parsed, RetrieverTool};

use crate::tools::Held;

/// What a look-up that found nothing tells the model.
///
/// Said in words rather than as an empty list, because an empty list reads to a model as a tool
/// that broke: this is the answer that there is nothing there, and the run goes on.
const NOTHING: &str = "nothing matched";

/// Runs one look-up and answers what the model is shown.
pub(crate) async fn looked(
    held: &Held,
    embedder: Option<&Arc<dyn Embedder>>,
    tool: RetrieverTool,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Produced {
    let asked = match held.retriever.parse(tool, arguments) {
        Ok(Parsed::Ready(asked)) => asked,
        Ok(Parsed::Searching {
            words,
            hits,
            filter,
        }) => {
            let Some(embedder) = embedder else {
                // Unreachable through a run that opened, which refuses a search it cannot embed
                // for before its first turn; said rather than unwrapped.
                return failed("this deployment embeds nothing, so it cannot search".to_owned());
            };
            match vector_for(embedder, words).await {
                Some(query) => LookUp::Search {
                    query: query.numbers().to_vec(),
                    hits,
                    filter,
                },
                None => return failed(
                    "the encoder would not answer for those words; try asking in fewer or plainer \
                     ones"
                        .to_owned(),
                ),
            }
        }
        Err(said) => return failed(said),
    };
    match held.lookup.look(&asked).await {
        Ok(found) if found.is_empty() => Produced {
            text: NOTHING.to_owned(),
            failed: false,
        },
        Ok(found) => Produced {
            text: rendered(&found),
            failed: false,
        },
        Err(said) => failed(said),
    }
}

/// What one text means, or nothing where the encoder would not commit.
///
/// Off the task, as the gateway's own embedder call is and for the same reason: a forward pass
/// that blocked the runtime would cost every other request in flight. **Under no deadline of its
/// own**, though, which is where this differs — the gateway embeds in front of a request it could
/// serve anyway, while a look-up is what the model asked for and has nothing to fall back on, so
/// what bounds it is the run's own clock rather than a ceiling that would make a tool flaky on a
/// loaded box.
async fn vector_for(embedder: &Arc<dyn Embedder>, words: String) -> Option<Embedding> {
    let embedder = Arc::clone(embedder);
    tokio::task::spawn_blocking(move || embedder.embed(&words))
        .await
        .ok()?
}

/// What was found, as the text a model reads: one row per line.
///
/// One json object per line rather than prose, because what a payload holds is the operator's own
/// and prose would mean inventing a shape for it. The id is first on every line, since that is
/// what a further look-up is addressed by — which is also why the line is written rather than
/// serialized from a map, whose keys would come back in alphabetical order.
///
/// **A score is written from the `f32` it is** and not widened on the way: `0.83` serialized
/// through an `f64` reads as `0.8299999833106995`, which is a dozen tokens of noise per row and
/// says nothing a model can use. One that is not a number at all is left out rather than
/// written, since `NaN` is not json.
fn rendered(found: &[Looked]) -> String {
    found
        .iter()
        .map(|one| {
            let id = serde_json::Value::String(one.id.clone());
            let score = match one.score {
                Some(score) if score.is_finite() => format!(",\"score\":{score}"),
                _ => String::new(),
            };
            format!("{{\"id\":{id}{score},\"found\":{}}}", one.payload)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tool result the model is told failed, carrying why.
fn failed(text: String) -> Produced {
    Produced { text, failed: true }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use ip_retrieve::Retrieval;

    use super::*;
    use crate::fixtures::retrieving;

    /// An embedder that answers the same vector for anything.
    #[derive(Debug)]
    struct Encoder;

    impl Embedder for Encoder {
        fn embed(&self, _text: &str) -> Option<Embedding> {
            Embedding::new(&[1.0, 0.0, 0.0]).ok()
        }
    }

    /// One that will not commit, which is the fourth finding's shape.
    #[derive(Debug)]
    struct Silent;

    impl Embedder for Silent {
        fn embed(&self, _text: &str) -> Option<Embedding> {
            None
        }
    }

    /// A backend answering whatever it was built with.
    #[derive(Debug)]
    struct Answering(Result<Vec<Looked>, String>);

    #[async_trait]
    impl Retrieval for Answering {
        async fn look(&self, _asked: &LookUp) -> Result<Vec<Looked>, String> {
            self.0.clone()
        }
    }

    fn encoder() -> Arc<dyn Embedder> {
        Arc::new(Encoder)
    }

    fn asking(words: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::json!({"query": words})
            .as_object()
            .expect("an object")
            .clone()
    }

    fn one(id: &str, score: Option<f32>, payload: serde_json::Value) -> Looked {
        Looked {
            id: id.to_owned(),
            score,
            payload,
        }
    }

    /// What the model reads: the id it can walk from, how near it was, and what it carries.
    #[tokio::test]
    async fn what_was_found_is_rendered_a_row_at_a_time() {
        let held = retrieving(
            "kb",
            Answering(Ok(vec![
                one("7", Some(0.83), serde_json::json!({"title": "Leave"})),
                one("9", None, serde_json::json!({"title": "Pay"})),
            ])),
        );

        let produced = looked(
            &held,
            Some(&encoder()),
            RetrieverTool::Search,
            &asking("leave policy"),
        )
        .await;

        assert!(!produced.failed, "{produced:?}");
        let rows: Vec<&str> = produced.text.lines().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            r#"{"id":"7","score":0.83,"found":{"title":"Leave"}}"#
        );
        assert_eq!(
            rows[1], r#"{"id":"9","found":{"title":"Pay"}}"#,
            "an unscored row carries no score rather than a null one"
        );
    }

    /// Nothing there is an answer to what the model asked, so the run goes on.
    #[tokio::test]
    async fn nothing_found_is_an_answer_and_not_a_failure() {
        let held = retrieving("kb", Answering(Ok(Vec::new())));

        let produced = looked(
            &held,
            Some(&encoder()),
            RetrieverTool::Search,
            &asking("leave policy"),
        )
        .await;

        assert!(!produced.failed, "an empty answer was reported as broken");
        assert_eq!(produced.text, NOTHING);
    }

    /// The fourth finding: an encoder that will not commit is a failed result carrying why,
    /// never an empty hit list that reads as "there is nothing about that".
    #[tokio::test]
    async fn an_encoder_that_will_not_commit_is_a_failed_result() {
        let held = retrieving(
            "kb",
            Answering(Ok(vec![one("7", None, serde_json::json!({}))])),
        );
        let silent: Arc<dyn Embedder> = Arc::new(Silent);

        let produced = looked(
            &held,
            Some(&silent),
            RetrieverTool::Search,
            &asking("leave policy"),
        )
        .await;

        assert!(produced.failed);
        assert!(produced.text.contains("encoder"), "{produced:?}");
        assert_ne!(
            produced.text, NOTHING,
            "an encoder that said nothing read as a corpus that holds nothing"
        );
    }

    /// A backend that refuses tells the model what it said, which is what names the thing to fix.
    #[tokio::test]
    async fn a_backend_that_refuses_is_a_failed_result_carrying_why() {
        let held = retrieving(
            "kb",
            Answering(Err("no such collection: handbook".to_owned())),
        );

        let produced = looked(
            &held,
            Some(&encoder()),
            RetrieverTool::Search,
            &asking("leave policy"),
        )
        .await;

        assert!(produced.failed);
        assert!(produced.text.contains("handbook"), "{produced:?}");
    }

    /// A tool called wrongly is the model's to correct, and it is told which parameter.
    #[tokio::test]
    async fn a_tool_called_wrongly_is_a_failed_result_naming_the_parameter() {
        let held = retrieving("kb", Answering(Ok(Vec::new())));

        let produced = looked(
            &held,
            Some(&encoder()),
            RetrieverTool::Search,
            serde_json::json!({}).as_object().expect("an object"),
        )
        .await;

        assert!(produced.failed);
        assert!(produced.text.contains("`query`"), "{produced:?}");
    }

    /// Unreachable through a run that opened, since one naming a search it cannot embed for is
    /// refused before its first turn — so what is checked is that the arm says so.
    #[tokio::test]
    async fn a_search_with_no_embedder_at_all_says_so_rather_than_panicking() {
        let held = retrieving("kb", Answering(Ok(Vec::new())));

        let produced = looked(&held, None, RetrieverTool::Search, &asking("leave")).await;

        assert!(produced.failed);
        assert!(produced.text.contains("embeds nothing"), "{produced:?}");
    }

    /// A tool that walks edges needs no encoder at all, which is why a deployment without one
    /// may still hold a retriever.
    #[tokio::test]
    async fn a_walk_needs_no_encoder() {
        let held = retrieving(
            "kb",
            Answering(Ok(vec![one(
                "9",
                None,
                serde_json::json!({"edge": "CITES"}),
            )])),
        );

        let produced = looked(
            &held,
            None,
            RetrieverTool::Neighbors,
            serde_json::json!({"node": 7})
                .as_object()
                .expect("an object"),
        )
        .await;

        assert!(!produced.failed, "{produced:?}");
        assert!(produced.text.contains("CITES"));
    }
}
