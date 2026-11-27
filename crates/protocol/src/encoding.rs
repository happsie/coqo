// SPDX-License-Identifier: Apache-2.0

//! Payload encodings: JSON (always available) and MessagePack (negotiated).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::message::{ErrorObject, Id, Message, RawMessage};

/// How a frame's payload is encoded.
///
/// JSON is mandatory and is always used for the handshake. MessagePack is
/// optional and is switched on for the rest of the connection when both
/// sides list it in the handshake; it is intended for hot paths (buffer ops,
/// trace streams).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub enum Encoding {
    /// UTF-8 JSON.
    #[serde(rename = "json")]
    Json,
    /// MessagePack, with structs encoded as maps keyed by field name.
    #[serde(rename = "msgpack")]
    MessagePack,
}

impl Encoding {
    /// Every encoding this implementation supports, in preference order.
    pub const ALL: [Encoding; 2] = [Encoding::MessagePack, Encoding::Json];

    /// The wire name used in the handshake.
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Json => "json",
            Encoding::MessagePack => "msgpack",
        }
    }

    /// Encode a message.
    pub fn encode(self, message: &Message) -> Result<Vec<u8>, EncodeError> {
        match self {
            Encoding::Json => serde_json::to_vec(message).map_err(|e| EncodeError(e.to_string())),
            Encoding::MessagePack => {
                rmp_serde::to_vec_named(message).map_err(|e| EncodeError(e.to_string()))
            }
        }
    }

    /// Decode a message.
    ///
    /// The error says whether the bytes were unreadable
    /// ([`DecodeErrorKind::Parse`]) or readable but not a JSON-RPC 2.0
    /// message ([`DecodeErrorKind::Invalid`]), and carries the request id
    /// when it could be recovered, so the receiver can answer with the right
    /// JSON-RPC error.
    pub fn decode(self, bytes: &[u8]) -> Result<Message, DecodeError> {
        let raw: RawMessage = match self {
            Encoding::Json => serde_json::from_slice(bytes).map_err(|e| {
                let kind = match e.classify() {
                    serde_json::error::Category::Data => DecodeErrorKind::Invalid,
                    _ => DecodeErrorKind::Parse,
                };
                DecodeError {
                    kind,
                    id: None,
                    message: e.to_string(),
                }
            })?,
            Encoding::MessagePack => rmp_serde::from_slice(bytes).map_err(|e| {
                use rmp_serde::decode::Error as E;
                let kind = match e {
                    E::InvalidMarkerRead(_) | E::InvalidDataRead(_) | E::LengthMismatch(_) => {
                        DecodeErrorKind::Parse
                    }
                    _ => DecodeErrorKind::Invalid,
                };
                DecodeError {
                    kind,
                    id: None,
                    message: e.to_string(),
                }
            })?,
        };
        let id = raw.id.clone().flatten();
        raw.classify().map_err(|e| DecodeError {
            kind: DecodeErrorKind::Invalid,
            id,
            message: e.message,
        })
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A message could not be encoded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot encode message: {0}")]
pub struct EncodeError(pub String);

/// Why a payload could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeErrorKind {
    /// Not valid JSON / MessagePack (JSON-RPC `Parse error`).
    Parse,
    /// Valid data, but not a JSON-RPC 2.0 message (`Invalid Request`).
    Invalid,
}

/// A payload could not be decoded into a [`Message`].
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("cannot decode message ({kind:?}): {message}")]
pub struct DecodeError {
    /// Parse or structure error.
    pub kind: DecodeErrorKind,
    /// The request id, if it could be read.
    pub id: Option<Id>,
    /// Details.
    pub message: String,
}

impl DecodeError {
    /// The JSON-RPC error to send back for this decode failure.
    pub fn to_error_object(&self) -> ErrorObject {
        match self.kind {
            DecodeErrorKind::Parse => ErrorObject::parse_error(self.message.clone()),
            DecodeErrorKind::Invalid => ErrorObject::invalid_request(self.message.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Notification, Request, Response, codes};
    use proptest::prelude::*;
    use serde_json::{Value, json};

    fn samples() -> Vec<Message> {
        vec![
            Request {
                id: Id::Number(1),
                method: "coqo/hello".into(),
                params: Some(json!({"protocol": {"major": 0, "minor": 0}})),
            }
            .into(),
            Request {
                id: Id::String("x-1".into()),
                method: "m".into(),
                params: None,
            }
            .into(),
            Request {
                id: Id::Number(-5),
                method: "m".into(),
                params: Some(Value::Null),
            }
            .into(),
            Notification {
                method: "coqo/event".into(),
                params: Some(json!({"subscription": 3, "seq": 42, "event": [1, "two", null]})),
            }
            .into(),
            Notification {
                method: "n".into(),
                params: None,
            }
            .into(),
            Response::ok(Id::Number(9), json!({"nested": {"a": [1.5, true]}})).into(),
            Response::ok(Id::Number(10), Value::Null).into(),
            Response::err(
                Some(Id::Number(11)),
                ErrorObject::method_not_found("nope").with_data(json!({"hint": "x"})),
            )
            .into(),
            Response::err(None, ErrorObject::parse_error("garbage")).into(),
        ]
    }

    #[test]
    fn every_message_type_round_trips_in_every_encoding() {
        for enc in Encoding::ALL {
            for m in samples() {
                let bytes = enc.encode(&m).unwrap();
                assert_eq!(enc.decode(&bytes).unwrap(), m, "{enc}: {m:?}");
            }
        }
    }

    #[test]
    fn json_parse_vs_invalid() {
        let e = Encoding::Json.decode(b"{not json").unwrap_err();
        assert_eq!(e.kind, DecodeErrorKind::Parse);
        assert_eq!(e.to_error_object().code, codes::PARSE_ERROR);

        let e = Encoding::Json.decode(b"[]").unwrap_err();
        assert_eq!(e.kind, DecodeErrorKind::Invalid, "batches are rejected");

        let e = Encoding::Json
            .decode(br#"{"jsonrpc":"2.0","id":4,"method":"m","result":1}"#)
            .unwrap_err();
        assert_eq!(e.kind, DecodeErrorKind::Invalid);
        assert_eq!(e.id, Some(Id::Number(4)));
        assert_eq!(e.to_error_object().code, codes::INVALID_REQUEST);
    }

    #[test]
    fn msgpack_garbage_is_an_error() {
        assert!(Encoding::MessagePack.decode(&[0xc1]).is_err());
        assert!(Encoding::MessagePack.decode(&[]).is_err());
    }

    #[test]
    fn encoding_wire_names() {
        assert_eq!(serde_json::to_value(Encoding::Json).unwrap(), json!("json"));
        assert_eq!(
            serde_json::to_value(Encoding::MessagePack).unwrap(),
            json!("msgpack")
        );
    }

    fn arb_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| json!(n)),
            any::<u64>().prop_map(|n| json!(n)),
            "[a-z0-9 ]{0,12}".prop_map(Value::String),
        ];
        leaf.prop_recursive(3, 32, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                prop::collection::btree_map("[a-z]{1,6}", inner, 0..6)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    fn arb_id() -> impl Strategy<Value = Id> {
        prop_oneof![
            any::<i64>().prop_map(Id::Number),
            "[a-z0-9-]{0,10}".prop_map(Id::String)
        ]
    }

    fn arb_message() -> impl Strategy<Value = Message> {
        let method = "[a-z/]{1,16}";
        prop_oneof![
            (arb_id(), method, prop::option::of(arb_value()))
                .prop_map(|(id, method, params)| { Request { id, method, params }.into() }),
            (method, prop::option::of(arb_value())).prop_map(|(method, params)| Notification {
                method,
                params
            }
            .into()),
            (arb_id(), arb_value()).prop_map(|(id, v)| Response::ok(id, v).into()),
            (
                prop::option::of(arb_id()),
                any::<i64>(),
                "[ -~]{0,20}",
                prop::option::of(arb_value())
            )
                .prop_map(|(id, code, message, data)| {
                    Response::err(
                        id,
                        ErrorObject {
                            code,
                            message,
                            data,
                        },
                    )
                    .into()
                }),
        ]
    }

    proptest! {
        #[test]
        fn prop_round_trip(m in arb_message()) {
            for enc in Encoding::ALL {
                let bytes = enc.encode(&m).unwrap();
                prop_assert_eq!(enc.decode(&bytes).unwrap(), m.clone());
            }
        }

        #[test]
        fn prop_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            for enc in Encoding::ALL {
                let _ = enc.decode(&bytes);
            }
        }
    }
}
