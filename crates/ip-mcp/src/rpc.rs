//! The JSON-RPC 2.0 envelope every message to a server travels in.

use crate::error::McpError;

/// The only version of JSON-RPC this speaks, in both directions.
pub const VERSION: &str = "2.0";

/// One message to a server: a call when it carries an id, a notification when it does not.
///
/// The distinction is JSON-RPC's own — a notification is a request without an `id`, and a server
/// answers it with nothing — so it is one type here rather than two that differ by a field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RpcRequest {
    /// Always [`VERSION`].
    pub jsonrpc: &'static str,
    /// Which call this is, absent where nothing is expected back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// What is being asked for.
    pub method: String,
    /// What it is being asked with.
    pub params: serde_json::Value,
}

impl RpcRequest {
    /// A call, which a server answers.
    pub fn call(id: u64, method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: VERSION,
            id: Some(id),
            method: method.to_owned(),
            params,
        }
    }

    /// A notification, which a server does not answer.
    pub fn notify(method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: VERSION,
            id: None,
            method: method.to_owned(),
            params,
        }
    }
}

/// What a server answered with.
///
/// Every field is optional here although the protocol requires some of them, because this type
/// exists to *refuse* an answer that is not one: a missing field has to arrive before it can be
/// reported, and a struct that will not deserialize reports only that the json was wrong.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RpcResponse {
    /// What the server claims to speak.
    #[serde(default)]
    pub jsonrpc: Option<String>,
    /// Which call it is answering.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    /// What it answered, where it answered.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// Why it would not, where it would not.
    #[serde(default)]
    pub error: Option<RpcFailure>,
}

/// An error a server reported in place of a result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RpcFailure {
    /// The code the server chose.
    pub code: i64,
    /// What it said about it.
    pub message: String,
}

impl RpcResponse {
    /// What the server answered the call with, or why that is not an answer to it.
    ///
    /// The id is checked rather than assumed: one connection carries many calls, and a result
    /// taken from the wrong one would be a tool's answer read as another tool's.
    pub fn answer_to(self, id: u64) -> Result<serde_json::Value, McpError> {
        match self.jsonrpc.as_deref() {
            Some(VERSION) => {}
            other => {
                return Err(McpError::NotJsonRpc {
                    detail: match other {
                        Some(version) => format!("jsonrpc is {version:?}"),
                        None => "no jsonrpc version".to_owned(),
                    },
                });
            }
        }
        if let Some(failure) = self.error {
            return Err(McpError::ServerRefused {
                code: failure.code,
                message: failure.message,
            });
        }
        if self.id.as_ref().and_then(serde_json::Value::as_u64) != Some(id) {
            return Err(McpError::AnsweredSomethingElse);
        }
        self.result.ok_or(McpError::NotJsonRpc {
            detail: "neither a result nor an error".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(raw: &str) -> Result<serde_json::Value, McpError> {
        serde_json::from_str::<RpcResponse>(raw)
            .unwrap()
            .answer_to(7)
    }

    #[test]
    fn a_call_carries_the_id_a_server_answers_it_by() {
        let written =
            serde_json::to_value(RpcRequest::call(7, "tools/list", serde_json::json!({}))).unwrap();
        assert_eq!(written["jsonrpc"], "2.0");
        assert_eq!(written["id"], 7);
        assert_eq!(written["method"], "tools/list");
    }

    #[test]
    fn a_notification_carries_no_id_at_all() {
        let written = serde_json::to_value(RpcRequest::notify(
            "notifications/initialized",
            serde_json::json!({}),
        ))
        .unwrap();
        assert_eq!(written["jsonrpc"], "2.0");
        assert!(written.get("id").is_none(), "{written}");
    }

    #[test]
    fn a_result_is_read_out_of_the_answer_to_the_call_that_asked() {
        let answer = answered(r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[]}}"#).unwrap();
        assert_eq!(answer, serde_json::json!({"tools": []}));
    }

    #[test]
    fn an_answer_to_another_call_is_not_an_answer_to_this_one() {
        assert_eq!(
            answered(r#"{"jsonrpc":"2.0","id":8,"result":{}}"#),
            Err(McpError::AnsweredSomethingElse)
        );
        assert_eq!(
            answered(r#"{"jsonrpc":"2.0","result":{}}"#),
            Err(McpError::AnsweredSomethingElse),
            "an answer to nothing is an answer to nothing in particular"
        );
    }

    #[test]
    fn a_server_that_will_not_answer_says_why_and_that_is_kept() {
        assert_eq!(
            answered(
                r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"no such method"}}"#
            ),
            Err(McpError::ServerRefused {
                code: -32601,
                message: "no such method".to_owned(),
            })
        );
    }

    /// Reported before the id is looked at: a refusal carries the id of the call it refuses, and
    /// a server that could not read the call may not know which one that was.
    #[test]
    fn a_refusal_is_a_refusal_whatever_it_answers_to() {
        assert!(matches!(
            answered(
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#
            ),
            Err(McpError::ServerRefused { .. })
        ));
    }

    #[test]
    fn something_that_is_not_json_rpc_is_not_an_answer() {
        for raw in [
            r#"{"id":7,"result":{}}"#,
            r#"{"jsonrpc":"1.0","id":7,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":7}"#,
        ] {
            assert!(
                matches!(answered(raw), Err(McpError::NotJsonRpc { .. })),
                "{raw} was taken for an answer"
            );
        }
    }
}
