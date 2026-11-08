//! The JSON-RPC 2.0 envelope every call to a plane travels in.

use crate::error::DrsgError;

/// The only version of JSON-RPC this speaks, in both directions.
pub const VERSION: &str = "2.0";

/// One call to a plane.
///
/// Every method this client needs is a call a store answers, so there is no notification shape
/// here: a request without an id would be one nothing could report a refusal for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RpcRequest {
    /// Always [`VERSION`].
    pub jsonrpc: &'static str,
    /// Which call this is.
    pub id: u64,
    /// What is being asked for.
    pub method: String,
    /// What it is being asked with.
    pub params: serde_json::Value,
}

impl RpcRequest {
    /// A call, which a store answers.
    pub fn call(id: u64, method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: VERSION,
            id,
            method: method.to_owned(),
            params,
        }
    }
}

/// What a store answered with.
///
/// Every field is optional although the protocol requires some of them, because this type exists
/// to *refuse* an answer that is not one: a missing field has to arrive before it can be
/// reported, and a struct that will not deserialize reports only that the json was wrong.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RpcResponse {
    /// What the store claims to speak.
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

/// An error a store reported in place of a result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RpcFailure {
    /// The code the store chose.
    pub code: i64,
    /// What it said about it.
    pub message: String,
}

impl RpcResponse {
    /// What the store answered the call with, or why that is not an answer to it.
    ///
    /// The id is checked rather than assumed: one pooled connection carries many calls, and a
    /// result taken from the wrong one would be one graph's rows read as another's.
    pub fn answer_to(self, id: u64) -> Result<serde_json::Value, DrsgError> {
        match self.jsonrpc.as_deref() {
            Some(VERSION) => {}
            other => {
                return Err(DrsgError::Malformed {
                    detail: match other {
                        Some(version) => format!("jsonrpc is {version:?}"),
                        None => "no jsonrpc version".to_owned(),
                    },
                });
            }
        }
        // Before the id: a refusal carries the id of the call it refuses, and a store that could
        // not read the call does not know which one that was.
        if let Some(failure) = self.error {
            return Err(DrsgError::Refused {
                code: failure.code,
                message: failure.message,
            });
        }
        if self.id.as_ref().and_then(serde_json::Value::as_u64) != Some(id) {
            return Err(DrsgError::Malformed {
                detail: "an answer to another call".to_owned(),
            });
        }
        self.result.ok_or(DrsgError::Malformed {
            detail: "neither a result nor an error".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(raw: &str) -> Result<serde_json::Value, DrsgError> {
        serde_json::from_str::<RpcResponse>(raw)
            .expect("the fixture's own json parses")
            .answer_to(7)
    }

    #[test]
    fn a_call_carries_the_id_a_store_answers_it_by() {
        let written = serde_json::to_value(RpcRequest::call(
            7,
            "plane.query",
            serde_json::json!({"plane": "graphs"}),
        ))
        .unwrap();
        assert_eq!(written["jsonrpc"], "2.0");
        assert_eq!(written["id"], 7);
        assert_eq!(written["method"], "plane.query");
        assert_eq!(written["params"]["plane"], "graphs");
    }

    #[test]
    fn a_result_is_read_out_of_the_answer_to_the_call_that_asked() {
        let answer = answered(r#"{"jsonrpc":"2.0","id":7,"result":{"rows":[]}}"#).unwrap();
        assert_eq!(answer, serde_json::json!({"rows": []}));
    }

    #[test]
    fn an_answer_to_another_call_is_not_an_answer_to_this_one() {
        assert_eq!(
            answered(r#"{"jsonrpc":"2.0","id":8,"result":{}}"#),
            Err(DrsgError::Malformed {
                detail: "an answer to another call".to_owned(),
            })
        );
        assert_eq!(
            answered(r#"{"jsonrpc":"2.0","result":{}}"#),
            Err(DrsgError::Malformed {
                detail: "an answer to another call".to_owned(),
            }),
            "an answer to nothing is an answer to nothing in particular"
        );
    }

    #[test]
    fn a_store_that_will_not_answer_says_why_and_that_is_kept() {
        assert_eq!(
            answered(r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"no plane"}}"#),
            Err(DrsgError::Refused {
                code: -32602,
                message: "no plane".to_owned(),
            })
        );
    }

    /// Reported before the id is looked at, which is why a refusal to `null` is still a refusal.
    #[test]
    fn a_refusal_is_a_refusal_whatever_it_answers_to() {
        assert!(matches!(
            answered(
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#
            ),
            Err(DrsgError::Refused { .. })
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
                matches!(answered(raw), Err(DrsgError::Malformed { .. })),
                "{raw} was taken for an answer"
            );
        }
    }
}
