// SPDX-License-Identifier: Apache-2.0

//! JSON-RPC 2.0 messages.
//!
//! One frame carries exactly one [`Message`]. Batches (JSON arrays of
//! messages) are not part of the Coqo Protocol and are answered with an
//! `Invalid Request` error.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `jsonrpc` member every message carries.
pub const JSONRPC_VERSION: &str = "2.0";

/// A request id: a number or a string (JSON-RPC 2.0 §4).
///
/// Coqo clients use numbers; strings are accepted from other peers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Id {
    /// A numeric id.
    Number(i64),
    /// A string id.
    String(String),
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Number(n) => write!(f, "{n}"),
            Id::String(s) => write!(f, "{s:?}"),
        }
    }
}

impl From<i64> for Id {
    fn from(n: i64) -> Self {
        Id::Number(n)
    }
}

impl From<String> for Id {
    fn from(s: String) -> Self {
        Id::String(s)
    }
}

impl From<&str> for Id {
    fn from(s: &str) -> Self {
        Id::String(s.to_owned())
    }
}

/// Error codes used in [`ErrorObject::code`].
///
/// `-32768..=-32000` is reserved by JSON-RPC 2.0. The Coqo Protocol defines
/// its own codes in the "server error" range `-32099..=-32000`. Application
/// methods (later issues) use codes outside the reserved range.
pub mod codes {
    /// Invalid JSON / MessagePack was received.
    pub const PARSE_ERROR: i64 = -32700;
    /// The payload is not a valid JSON-RPC 2.0 message.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The params did not match the method's schema.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal error in the handler.
    pub const INTERNAL_ERROR: i64 = -32603;

    /// The peers' protocol versions are incompatible (handshake only).
    pub const INCOMPATIBLE_PROTOCOL: i64 = -32001;
    /// A message other than `coqo/hello` arrived before the handshake.
    pub const NOT_INITIALIZED: i64 = -32002;
    /// A subscription cannot resume from the requested sequence number
    /// (it has been evicted, or is ahead of the source). Re-snapshot and
    /// subscribe again.
    pub const RESUME_GAP: i64 = -32003;
    /// The subscription id or topic is unknown.
    pub const UNKNOWN_SUBSCRIPTION: i64 = -32004;
    /// The request was cancelled (for example, the connection is closing).
    pub const REQUEST_CANCELLED: i64 = -32005;
    /// Handshake: the server requires a session token and the client sent
    /// none, or a wrong one. The server closes the connection.
    pub const UNAUTHORIZED: i64 = -32006;
    /// The connection's token does not grant this call (for example, a
    /// token scoped to another workspace).
    pub const FORBIDDEN: i64 = -32007;
}

/// A JSON-RPC 2.0 error object (§5.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ErrorObject {
    /// One of [`codes`] or an application-defined code.
    pub code: i64,
    /// A short human-readable description.
    pub message: String,
    /// Structured details, if any.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// An error with a code and message and no data.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attach structured data.
    pub fn with_data(mut self, data: impl Serialize) -> Self {
        self.data = serde_json::to_value(data).ok();
        self
    }

    /// [`codes::PARSE_ERROR`].
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(codes::PARSE_ERROR, message)
    }

    /// [`codes::INVALID_REQUEST`].
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_REQUEST, message)
    }

    /// [`codes::METHOD_NOT_FOUND`].
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            codes::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )
    }

    /// [`codes::INVALID_PARAMS`].
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, message)
    }

    /// [`codes::INTERNAL_ERROR`].
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL_ERROR, message)
    }
}

impl fmt::Display for ErrorObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for ErrorObject {}

/// A request: expects exactly one [`Response`] with the same id.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Correlates the response.
    pub id: Id,
    /// Method name, e.g. `coqo/subscribe`.
    pub method: String,
    /// Method parameters; `None` when omitted.
    pub params: Option<Value>,
}

/// A notification: no id, no response.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    /// Method name, e.g. `coqo/event`.
    pub method: String,
    /// Method parameters; `None` when omitted.
    pub params: Option<Value>,
}

/// A response to a [`Request`].
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// The request's id. `None` (serialized as `null`) only for errors about
    /// a request whose id could not be read.
    pub id: Option<Id>,
    /// `Ok(result)` or `Err(error)`.
    pub outcome: Result<Value, ErrorObject>,
}

impl Response {
    /// A success response.
    pub fn ok(id: Id, result: Value) -> Self {
        Self {
            id: Some(id),
            outcome: Ok(result),
        }
    }

    /// An error response.
    pub fn err(id: Option<Id>, error: ErrorObject) -> Self {
        Self {
            id,
            outcome: Err(error),
        }
    }
}

/// Any JSON-RPC 2.0 message.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// A request.
    Request(Request),
    /// A notification.
    Notification(Notification),
    /// A response.
    Response(Response),
}

impl From<Request> for Message {
    fn from(r: Request) -> Self {
        Message::Request(r)
    }
}

impl From<Notification> for Message {
    fn from(n: Notification) -> Self {
        Message::Notification(n)
    }
}

impl From<Response> for Message {
    fn from(r: Response) -> Self {
        Message::Response(r)
    }
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("jsonrpc", JSONRPC_VERSION)?;
        match self {
            Message::Request(r) => {
                map.serialize_entry("id", &r.id)?;
                map.serialize_entry("method", &r.method)?;
                if let Some(p) = &r.params {
                    map.serialize_entry("params", p)?;
                }
            }
            Message::Notification(n) => {
                map.serialize_entry("method", &n.method)?;
                if let Some(p) = &n.params {
                    map.serialize_entry("params", p)?;
                }
            }
            Message::Response(r) => {
                map.serialize_entry("id", &r.id)?;
                match &r.outcome {
                    Ok(v) => map.serialize_entry("result", v)?,
                    Err(e) => map.serialize_entry("error", e)?,
                }
            }
        }
        map.end()
    }
}

/// Deserialize a field that is present (even if `null`) into `Some`.
/// Combined with `#[serde(default)]`, absent fields become `None`.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// The wire shape of every message, before classification.
#[derive(Deserialize)]
pub(crate) struct RawMessage {
    #[serde(default)]
    jsonrpc: Option<String>,
    /// Absent → `None`; `null` → `Some(None)`.
    #[serde(default, deserialize_with = "present")]
    pub(crate) id: Option<Option<Id>>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default, deserialize_with = "present")]
    params: Option<Value>,
    /// Absent → `None`; `null` → `Some(Value::Null)`.
    #[serde(default, deserialize_with = "present")]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ErrorObject>,
}

impl RawMessage {
    /// Classify into a [`Message`], or explain why it isn't one.
    pub(crate) fn classify(self) -> Result<Message, ErrorObject> {
        if self.jsonrpc.as_deref() != Some(JSONRPC_VERSION) {
            return Err(ErrorObject::invalid_request(
                "`jsonrpc` must be the string \"2.0\"",
            ));
        }
        match (self.method, self.id) {
            (Some(method), Some(Some(id))) => {
                if self.result.is_some() || self.error.is_some() {
                    return Err(ErrorObject::invalid_request(
                        "a request must not carry `result` or `error`",
                    ));
                }
                Ok(Message::Request(Request {
                    id,
                    method,
                    params: self.params,
                }))
            }
            (Some(_), Some(None)) => Err(ErrorObject::invalid_request(
                "a request id must be a number or a string, not null",
            )),
            (Some(method), None) => {
                if self.result.is_some() || self.error.is_some() {
                    return Err(ErrorObject::invalid_request(
                        "a notification must not carry `result` or `error`",
                    ));
                }
                Ok(Message::Notification(Notification {
                    method,
                    params: self.params,
                }))
            }
            (None, Some(id)) => {
                let outcome = match (self.result, self.error) {
                    (Some(result), None) => Ok(result),
                    (None, Some(error)) => Err(error),
                    _ => {
                        return Err(ErrorObject::invalid_request(
                            "a response must carry exactly one of `result` and `error`",
                        ));
                    }
                };
                Ok(Message::Response(Response { id, outcome }))
            }
            (None, None) => Err(ErrorObject::invalid_request(
                "message has neither `method` nor `id`",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        RawMessage::deserialize(d)?
            .classify()
            .map_err(|e| de::Error::custom(e.message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Result<Message, ErrorObject> {
        serde_json::from_value::<RawMessage>(v)
            .map_err(|e| ErrorObject::invalid_request(e.to_string()))?
            .classify()
    }

    #[test]
    fn request_wire_shape() {
        let m = Message::Request(Request {
            id: Id::Number(7),
            method: "a/b".into(),
            params: Some(json!({"x": 1})),
        });
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"jsonrpc": "2.0", "id": 7, "method": "a/b", "params": {"x": 1}})
        );
    }

    #[test]
    fn notification_omits_id_and_absent_params() {
        let m = Message::Notification(Notification {
            method: "n".into(),
            params: None,
        });
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"jsonrpc": "2.0", "method": "n"})
        );
    }

    #[test]
    fn null_result_is_a_success() {
        let m = parse(json!({"jsonrpc": "2.0", "id": 1, "result": null})).unwrap();
        assert_eq!(
            m,
            Message::Response(Response::ok(Id::Number(1), Value::Null))
        );
    }

    #[test]
    fn error_with_null_id() {
        let m = parse(json!({"jsonrpc": "2.0", "id": null,
            "error": {"code": -32700, "message": "bad"}}))
        .unwrap();
        assert_eq!(
            m,
            Message::Response(Response::err(None, ErrorObject::parse_error("bad")))
        );
    }

    #[test]
    fn string_ids() {
        let m = parse(json!({"jsonrpc": "2.0", "id": "abc", "method": "m"})).unwrap();
        assert!(matches!(m, Message::Request(Request { id: Id::String(s), .. }) if s == "abc"));
    }

    #[test]
    fn rejects_invalid_shapes() {
        for bad in [
            json!({"id": 1, "method": "m"}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "m"}),
            json!({"jsonrpc": "2.0"}),
            json!({"jsonrpc": "2.0", "id": null, "method": "m"}),
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({"jsonrpc": "2.0", "id": 1, "result": 1, "error": {"code": 1, "message": ""}}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "m", "result": 1}),
            json!({"jsonrpc": "2.0", "method": "m", "error": {"code": 1, "message": ""}}),
        ] {
            let err = parse(bad.clone()).expect_err(&bad.to_string());
            assert_eq!(err.code, codes::INVALID_REQUEST, "{bad}");
        }
    }
}
