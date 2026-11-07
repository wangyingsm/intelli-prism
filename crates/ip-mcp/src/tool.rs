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
