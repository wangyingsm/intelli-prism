//! The tools a server offers, as this gateway can use them.

use std::fmt;

use crate::error::McpError;

/// The longest a tool name may be, in bytes.
const NAME_MAX_BYTES: usize = 64;

/// What a server calls one of its tools.
///
/// The rule is the one the model providers apply to a function they are told about — letters,
/// digits, `-` and `_`, up to 64 bytes — and it is written here rather than borrowed from
/// another crate for the reason [`crate::McpId`]'s rule is. **It carries no dot**: a dot is what
/// separates a server from its tool where a template names one, so a tool whose own name held
/// one could not be addressed.
///
/// A server offering a tool outside this is offering one that could not be declared to a model,
/// which is why such a tool is dropped from an offering rather than refusing the whole of it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpToolName(Box<str>);

impl McpToolName {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "mcp tool name";

    /// Validates and wraps a name.
    pub fn new(raw: &str) -> Result<Self, McpError> {
        if raw.is_empty() {
            return Err(McpError::Empty { kind: Self::KIND });
        }
        if raw.len() > NAME_MAX_BYTES {
            return Err(McpError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: NAME_MAX_BYTES,
            });
        }
        match raw
            .chars()
            .find(|ch| !ch.is_ascii_alphanumeric() && *ch != '-' && *ch != '_')
        {
            Some(ch) => Err(McpError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(raw.to_owned().into_boxed_str())),
        }
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for McpToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One tool a server offers.
///
/// The three fields a model is told about, and nothing else a server may have sent: an output
/// schema, annotations and a title are all read by nothing here.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// What the server calls it.
    pub name: McpToolName,
    /// What it does, where the server said.
    pub description: Option<String>,
    /// The json schema of its arguments, as the server declared it.
    pub schema: serde_json::Value,
}

/// A tool a server offered that this gateway will not declare to a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTool {
    /// What the server called it, as the server spelled it.
    pub name: String,
    /// Why it was left out.
    pub why: McpError,
}

/// What a server offers: the tools that can be used, and the ones that cannot.
///
/// Both halves, because a tool silently missing from a listing is a run that fails later for a
/// reason nobody can see, while a listing refused wholesale is one bad tool stopping every
/// good one beside it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Offering {
    /// The tools a model can be told about.
    pub tools: Vec<McpTool>,
    /// The ones it cannot, each with what was wrong.
    pub dropped: Vec<DroppedTool>,
}

impl Offering {
    /// Reads one page of a `tools/list` answer into what this can use and what it cannot.
    pub(crate) fn extend_from(&mut self, listed: &[serde_json::Value]) {
        for one in listed {
            match tool(one) {
                Ok(tool) => self.tools.push(tool),
                Err((name, why)) => self.dropped.push(DroppedTool { name, why }),
            }
        }
    }
}

/// One listed tool, or what it was called and why it is unusable.
fn tool(listed: &serde_json::Value) -> Result<McpTool, (String, McpError)> {
    let Some(name) = listed.get("name").and_then(serde_json::Value::as_str) else {
        return Err((
            String::new(),
            McpError::NotATool {
                detail: "no name".to_owned(),
            },
        ));
    };
    let held = McpToolName::new(name).map_err(|why| (name.to_owned(), why))?;
    Ok(McpTool {
        name: held,
        description: listed
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        // A tool that declared no arguments takes none, which is a schema rather than an
        // absence: every family needs one to declare the tool at all.
        schema: listed
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
    })
}

/// What a tool produced when it was run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Produced {
    /// What it returned, as the text a model is given.
    pub text: String,
    /// Whether the tool itself called this a failure.
    ///
    /// A tool reports its own failure *in a result* rather than as a protocol error, because
    /// the model is what must read it and may try something else; a protocol error means the
    /// call itself was wrong, which is this gateway's problem and not the model's. The two are
    /// kept apart for that reason: this is a field, and the other is an error.
    pub failed: bool,
}

/// Reads a `tools/call` answer into the text a model can be given.
///
/// A model is handed one string, so every block a server sent is rendered into it in order —
/// including the ones that are not text, which are named rather than dropped. A tool that
/// answered with a picture answered with something, and a model told nothing at all would read
/// that as a tool that did nothing.
pub(crate) fn produced(result: &serde_json::Value) -> Result<Produced, McpError> {
    let failed = result
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    match result.get("content").and_then(serde_json::Value::as_array) {
        Some(blocks) => Ok(Produced {
            text: blocks
                .iter()
                .map(rendered)
                .collect::<Vec<String>>()
                .join("\n"),
            failed,
        }),
        // A revision of the protocol added a structured answer beside the content, and a server
        // speaking it may send that alone.
        None => match result.get("structuredContent") {
            Some(structured) => Ok(Produced {
                text: structured.to_string(),
                failed,
            }),
            None => Err(McpError::NotAResult {
                detail: "neither content nor a structured answer".to_owned(),
            }),
        },
    }
}

/// One content block as the text a model is given for it.
fn rendered(block: &serde_json::Value) -> String {
    let text = |key: &str| {
        block
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    match block.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => text("text"),
        Some("resource_link") => format!("[resource {}]", text("uri")),
        Some("resource") => match block.get("resource") {
            Some(held) => match held.get("text").and_then(serde_json::Value::as_str) {
                Some(held) => held.to_owned(),
                None => format!("[resource {}]", rendered_uri(held)),
            },
            None => "[resource]".to_owned(),
        },
        Some(kind) => match text("mimeType") {
            mime if mime.is_empty() => format!("[{kind} omitted]"),
            mime => format!("[{kind} {mime} omitted]"),
        },
        None => "[omitted]".to_owned(),
    }
}

/// What a resource says it is, where it says.
fn rendered_uri(resource: &serde_json::Value) -> &str {
    resource
        .get("uri")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("of no uri")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_takes_what_a_model_can_be_told_about() {
        for raw in ["search", "get_stock", "list-items", "v2", "A"] {
            assert_eq!(McpToolName::new(raw).unwrap().as_str(), raw);
        }
        assert_eq!(
            McpToolName::new("get_stock").unwrap().to_string(),
            "get_stock"
        );
    }

    #[test]
    fn a_name_is_refused_empty_and_past_its_ceiling() {
        assert_eq!(
            McpToolName::new(""),
            Err(McpError::Empty {
                kind: McpToolName::KIND
            })
        );
        assert!(matches!(
            McpToolName::new(&"a".repeat(NAME_MAX_BYTES + 1)),
            Err(McpError::TooLong { .. })
        ));
        assert!(McpToolName::new(&"a".repeat(NAME_MAX_BYTES)).is_ok());
    }

    /// The dot above all: it is the separator, so a tool carrying one could not be addressed.
    #[test]
    fn a_name_carrying_what_no_provider_takes_is_refused() {
        for (raw, ch) in [
            ("stock.get", '.'),
            ("get stock", ' '),
            ("stock/get", '/'),
            ("stock:get", ':'),
            ("库存", '库'),
        ] {
            assert_eq!(
                McpToolName::new(raw),
                Err(McpError::IllegalChar {
                    kind: McpToolName::KIND,
                    ch,
                }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_listed_tool_is_read_field_for_field() {
        let mut offering = Offering::default();
        offering.extend_from(&[serde_json::json!({
            "name": "get_stock",
            "description": "What is in the warehouse",
            "inputSchema": {"type": "object", "properties": {"sku": {"type": "string"}}},
        })]);

        assert_eq!(offering.dropped, []);
        assert_eq!(
            offering.tools,
            [McpTool {
                name: McpToolName::new("get_stock").unwrap(),
                description: Some("What is in the warehouse".to_owned()),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"sku": {"type": "string"}},
                }),
            }]
        );
    }

    #[test]
    fn a_tool_that_declared_no_arguments_takes_none_rather_than_having_no_schema() {
        let mut offering = Offering::default();
        offering.extend_from(&[serde_json::json!({"name": "ping"})]);

        assert_eq!(offering.tools[0].description, None);
        assert_eq!(
            offering.tools[0].schema,
            serde_json::json!({"type": "object", "properties": {}})
        );
    }

    fn ran(result: serde_json::Value) -> Produced {
        produced(&result).unwrap()
    }

    #[test]
    fn what_a_tool_said_is_what_the_model_is_given() {
        assert_eq!(
            ran(serde_json::json!({"content": [{"type": "text", "text": "41 crates in bay 3"}]})),
            Produced {
                text: "41 crates in bay 3".to_owned(),
                failed: false,
            }
        );
    }

    #[test]
    fn every_block_a_tool_answered_with_is_read_in_order() {
        assert_eq!(
            ran(serde_json::json!({"content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ]}))
            .text,
            "first\nsecond"
        );
    }

    /// A tool that failed still answered, and the model is what reads it.
    #[test]
    fn a_tool_that_calls_itself_failed_says_so_and_is_still_read() {
        assert_eq!(
            ran(serde_json::json!({
                "content": [{"type": "text", "text": "no such sku"}],
                "isError": true,
            })),
            Produced {
                text: "no such sku".to_owned(),
                failed: true,
            }
        );
    }

    /// Named, not dropped: a tool that answered with a picture answered with something, and a
    /// model given nothing would read that as a tool that did nothing.
    #[test]
    fn what_a_model_cannot_be_given_is_named_in_its_place() {
        for (block, rendered) in [
            (
                serde_json::json!({"type": "image", "mimeType": "image/png", "data": "iVBOR"}),
                "[image image/png omitted]",
            ),
            (
                serde_json::json!({"type": "audio", "mimeType": "audio/wav"}),
                "[audio audio/wav omitted]",
            ),
            (serde_json::json!({"type": "image"}), "[image omitted]"),
            (
                serde_json::json!({"type": "resource_link", "uri": "file:///stock.csv"}),
                "[resource file:///stock.csv]",
            ),
            (
                serde_json::json!({"nothing": "of the protocol"}),
                "[omitted]",
            ),
        ] {
            assert_eq!(
                ran(serde_json::json!({"content": [block.clone()]})).text,
                rendered,
                "{block}"
            );
        }
    }

    #[test]
    fn an_embedded_resource_is_read_as_its_text_where_it_carries_one() {
        assert_eq!(
            ran(serde_json::json!({"content": [{
                "type": "resource",
                "resource": {"uri": "file:///stock.csv", "text": "sku,count"},
            }]}))
            .text,
            "sku,count"
        );
        assert_eq!(
            ran(serde_json::json!({"content": [{
                "type": "resource",
                "resource": {"uri": "file:///stock.png", "blob": "iVBOR"},
            }]}))
            .text,
            "[resource file:///stock.png]"
        );
        assert_eq!(
            ran(serde_json::json!({"content": [{"type": "resource"}]})).text,
            "[resource]"
        );
        assert_eq!(
            ran(serde_json::json!({"content": [{"type": "resource", "resource": {}}]})).text,
            "[resource of no uri]"
        );
    }

    /// A later revision of the protocol lets a server answer with this and no content at all.
    #[test]
    fn a_structured_answer_is_read_where_there_is_no_content() {
        assert_eq!(
            ran(serde_json::json!({"structuredContent": {"count": 41}, "isError": false})),
            Produced {
                text: r#"{"count":41}"#.to_owned(),
                failed: false,
            }
        );
    }

    #[test]
    fn an_answer_holding_nothing_a_model_can_read_is_refused() {
        for result in [
            serde_json::json!({}),
            serde_json::json!({"content": "not a list"}),
            serde_json::json!({"isError": true}),
            serde_json::json!(null),
        ] {
            assert!(
                matches!(produced(&result), Err(McpError::NotAResult { .. })),
                "{result} was taken for a result"
            );
        }
    }

    #[test]
    fn a_tool_that_answered_with_nothing_answered_with_nothing() {
        assert_eq!(
            ran(serde_json::json!({"content": []})),
            Produced {
                text: String::new(),
                failed: false,
            }
        );
    }

    /// One unusable tool leaves every usable one beside it, and says what was wrong with it.
    #[test]
    fn what_cannot_be_declared_is_dropped_and_the_rest_is_kept() {
        let mut offering = Offering::default();
        offering.extend_from(&[
            serde_json::json!({"name": "get_stock"}),
            serde_json::json!({"name": "stock.get"}),
            serde_json::json!({"name": ""}),
            serde_json::json!({"description": "nameless"}),
            serde_json::json!("not even an object"),
        ]);

        assert_eq!(offering.tools.len(), 1);
        assert_eq!(offering.tools[0].name.as_str(), "get_stock");
        assert_eq!(
            offering.dropped,
            [
                DroppedTool {
                    name: "stock.get".to_owned(),
                    why: McpError::IllegalChar {
                        kind: McpToolName::KIND,
                        ch: '.',
                    },
                },
                DroppedTool {
                    name: String::new(),
                    why: McpError::Empty {
                        kind: McpToolName::KIND
                    },
                },
                DroppedTool {
                    name: String::new(),
                    why: McpError::NotATool {
                        detail: "no name".to_owned()
                    },
                },
                DroppedTool {
                    name: String::new(),
                    why: McpError::NotATool {
                        detail: "no name".to_owned()
                    },
                },
            ]
        );
    }
}
