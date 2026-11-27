// SPDX-License-Identifier: Apache-2.0

//! The error type of the client and server helpers.

use std::time::Duration;

use crate::encoding::{DecodeError, EncodeError};
use crate::frame::FrameError;
use crate::handshake::ProtocolVersion;
use crate::message::ErrorObject;

/// Everything that can go wrong on a Coqo Protocol connection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The transport failed.
    #[error("transport error: {0}")]
    Io(#[from] std::io::Error),
    /// A frame exceeded the size limit; the stream cannot be resynchronised.
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    FrameTooLarge {
        /// Announced payload length.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// A message could not be encoded.
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// A message could not be decoded.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// Typed params, results or events did not match their Rust types.
    #[error("cannot convert {what}: {message}")]
    Conversion {
        /// What was being converted, e.g. `result of workspace/open`.
        what: String,
        /// The serde error.
        message: String,
    },
    /// The peer did not complete the handshake in time.
    #[error(
        "Coqo Protocol handshake timed out after {0:?}: the peer did not complete `coqo/hello` \
         (is it a Coqo Protocol endpoint?)"
    )]
    HandshakeTimeout(Duration),
    /// The peers speak incompatible protocol versions.
    #[error(
        "incompatible Coqo Protocol version: this side speaks {local}, the peer (build \
         {peer_build}) speaks {peer}; restart or update the daemon"
    )]
    IncompatibleVersion {
        /// Our version.
        local: ProtocolVersion,
        /// The peer's version.
        peer: ProtocolVersion,
        /// The peer's build id.
        peer_build: String,
    },
    /// The handshake failed for another reason.
    #[error("Coqo Protocol handshake failed: {0}")]
    Handshake(String),
    /// The peer answered a request with an error.
    #[error("{0}")]
    Rpc(ErrorObject),
    /// A request got no response in time.
    #[error("request `{method}` timed out after {timeout:?}")]
    RequestTimeout {
        /// The method.
        method: String,
        /// The configured timeout.
        timeout: Duration,
    },
    /// The connection is closed.
    #[error("connection closed")]
    Closed,
    /// A subscription's local buffer overflowed; resubscribe with
    /// `since: Some(resume_from)`.
    #[error("subscription fell behind; resubscribe from seq {resume_from}")]
    Lagged {
        /// The last sequence number delivered (or the starting point).
        resume_from: u64,
    },
    /// The peer broke the protocol (for example, a non-increasing `seq`).
    #[error("protocol violation: {0}")]
    Violation(String),
}

impl From<FrameError> for ProtocolError {
    fn from(e: FrameError) -> Self {
        match e {
            FrameError::Io(e) => ProtocolError::Io(e),
            FrameError::TooLarge { len, max } => ProtocolError::FrameTooLarge { len, max },
        }
    }
}

impl ProtocolError {
    /// The JSON-RPC error object, if the peer sent one.
    pub fn rpc(&self) -> Option<&ErrorObject> {
        match self {
            ProtocolError::Rpc(e) => Some(e),
            _ => None,
        }
    }

    pub(crate) fn conversion(what: impl Into<String>, e: impl std::fmt::Display) -> Self {
        ProtocolError::Conversion {
            what: what.into(),
            message: e.to_string(),
        }
    }
}
