// SPDX-License-Identifier: Apache-2.0

//! The version handshake.
//!
//! The first frame on every connection is a `coqo/hello` **request** from
//! the client, encoded as JSON. The server answers with a [`Welcome`] or an
//! error, also as JSON, and only then do both sides switch to the negotiated
//! [`Encoding`]. This exchange (u32 big-endian length prefix + JSON
//! `coqo/hello`) is frozen across all protocol versions so that any two
//! versions can always tell each other apart.
//!
//! Compatibility: two versions are compatible when their `major` numbers
//! match. `minor` bumps only add things; the effective minor version of a
//! connection is the smaller of the two.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::encoding::Encoding;
use crate::message::{ErrorObject, codes};

/// The Coqo Protocol version implemented by this crate.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 0 };

/// Default time either side waits for the other's half of the handshake.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A protocol version: `major.minor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ProtocolVersion {
    /// Incremented for breaking changes.
    pub major: u32,
    /// Incremented for additive changes.
    pub minor: u32,
}

impl ProtocolVersion {
    /// Whether a peer speaking `other` can talk to us.
    pub fn is_compatible_with(self, other: ProtocolVersion) -> bool {
        self.major == other.major
    }

    /// The version both sides use on a connection: same major, lower minor.
    /// `None` if incompatible.
    pub fn negotiate(self, other: ProtocolVersion) -> Option<ProtocolVersion> {
        self.is_compatible_with(other).then(|| ProtocolVersion {
            major: self.major,
            minor: self.minor.min(other.minor),
        })
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Params of `coqo/hello`, sent by the client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Hello {
    /// The client's protocol version.
    pub protocol: ProtocolVersion,
    /// The client's build id (version + commit), shown in errors and logs.
    pub build: String,
    /// What the client is, e.g. `coqo-ui`, `coqo-cli`, `coqod-mcp`.
    pub client: String,
    /// Encodings the client accepts after the handshake, in preference
    /// order. JSON is always accepted, even if not listed.
    #[serde(default)]
    pub encodings: Vec<Encoding>,
}

/// Result of `coqo/hello`, sent by the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Welcome {
    /// The server's protocol version.
    pub protocol: ProtocolVersion,
    /// The negotiated version for this connection.
    pub negotiated: ProtocolVersion,
    /// The server's build id. Equal builds are compatible by definition;
    /// different builds are compatible when the protocol versions are.
    pub build: String,
    /// What the server is, e.g. `coqod`.
    pub server: String,
    /// The encoding both sides use from the next frame on.
    pub encoding: Encoding,
    /// A server-assigned id for this connection, unique per server process.
    pub connection: u64,
}

/// `data` of the [`codes::INCOMPATIBLE_PROTOCOL`] error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct IncompatibleProtocol {
    /// The server's protocol version.
    pub protocol: ProtocolVersion,
    /// The server's build id.
    pub build: String,
}

impl IncompatibleProtocol {
    /// The error object the server sends.
    pub fn to_error(&self, client: ProtocolVersion) -> ErrorObject {
        ErrorObject::new(
            codes::INCOMPATIBLE_PROTOCOL,
            format!(
                "incompatible Coqo Protocol: client speaks {client}, server (build {}) speaks {}",
                self.build, self.protocol
            ),
        )
        .with_data(self)
    }
}

/// Pick the encoding for a connection: the client's most preferred encoding
/// that the server also supports, else JSON.
pub fn negotiate_encoding(client: &[Encoding], server: &[Encoding]) -> Encoding {
    client
        .iter()
        .copied()
        .find(|e| server.contains(e))
        .unwrap_or(Encoding::Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_is_by_major() {
        let v = |major, minor| ProtocolVersion { major, minor };
        assert!(v(0, 0).is_compatible_with(v(0, 7)));
        assert!(!v(0, 0).is_compatible_with(v(1, 0)));
        assert_eq!(v(2, 3).negotiate(v(2, 1)), Some(v(2, 1)));
        assert_eq!(v(2, 3).negotiate(v(3, 3)), None);
    }

    #[test]
    fn encoding_negotiation() {
        use Encoding::*;
        assert_eq!(
            negotiate_encoding(&[MessagePack, Json], &[Json, MessagePack]),
            MessagePack
        );
        assert_eq!(negotiate_encoding(&[MessagePack], &[Json]), Json);
        assert_eq!(negotiate_encoding(&[], &[MessagePack]), Json);
        assert_eq!(
            negotiate_encoding(&[Json, MessagePack], &Encoding::ALL),
            Json
        );
    }

    #[test]
    fn hello_tolerates_unknown_and_missing_optional_fields() {
        let h: Hello = serde_json::from_value(serde_json::json!({
            "protocol": {"major": 9, "minor": 1},
            "build": "b",
            "client": "c",
            "future": true
        }))
        .unwrap();
        assert!(h.encodings.is_empty());
    }
}
