// SPDX-License-Identifier: Apache-2.0

//! The Coqo Protocol: the versioned contract between the UI, the CLI and the
//! daemon (`docs/PLAN.md` §4.4, ADR 0003, and the spec in `protocol/`).
//!
//! - [`frame`]: u32 big-endian length-prefixed frames.
//! - [`Message`]: JSON-RPC 2.0 requests, responses and notifications,
//!   encoded as JSON or (negotiated) MessagePack ([`Encoding`]).
//! - [`handshake`]: `coqo/hello` with protocol version + build id; an
//!   incompatible version is a clear error, never a hang.
//! - [`auth`]: session tokens in the handshake and the [`Scope`] they
//!   grant each connection, and the per-route [`Access`] that decides what
//!   a session scope may call (default: nothing).
//! - [`Method`], [`NotificationMethod`], [`Topic`]: typed method
//!   definitions, used by the [`Client`] and the server [`Router`].
//! - [`methods`]: the application methods and topics (terminals, …).
//! - [`subscription`]: sequenced event streams with resume-from-seq, and
//!   [`SeqLog`], an in-memory source.
//! - [`daemon`]: `daemon/status` and `daemon/shutdown`, the daemon's own
//!   lifecycle methods.
//! - [`methods`]: the daemon's application methods and topics
//!   (`workspace/*`, `fs/*`, `agents/*`, `sessions/*`, `buffer/*`), shared by
//!   clients and `coqod`.
//!
//! The client and server work over any `AsyncRead + AsyncWrite` byte stream;
//! [`unix`] has Unix socket helpers.
//!
//! ```
//! # use coqo_protocol::*;
//! # use serde::{Deserialize, Serialize};
//! #[derive(Debug)]
//! enum Add {}
//! impl Method for Add {
//!     const NAME: &'static str = "math/add";
//!     type Params = (i64, i64);
//!     type Result = i64;
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), ProtocolError> {
//! let router = Router::new().method::<Add, _, _>(|_ctx, (a, b)| async move { Ok(a + b) });
//! let server = Server::new(router, ServerConfig::new("coqod", "dev"));
//!
//! let (client_io, server_io) = tokio::io::duplex(64 * 1024);
//! tokio::spawn(async move { server.serve(server_io).await });
//!
//! let (client, _notifications) =
//!     Client::connect(client_io, ClientConfig::new("coqo-cli", "dev")).await?;
//! assert_eq!(client.request::<Add>((2, 3)).await?, 5);
//! # Ok(())
//! # }
//! ```

pub mod auth;
pub mod client;
pub mod daemon;
pub mod encoding;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod message;
pub mod method;
pub mod methods;
pub mod server;
pub mod subscription;
mod transport;
#[cfg(unix)]
pub mod unix;

#[cfg(test)]
mod schema;
#[cfg(test)]
mod tests;

pub use auth::{Access, AccessKind, Authenticator, Scope, Unauthorized};
pub use client::{Client, ClientConfig, Notifications, Subscription};
pub use encoding::{DecodeError, DecodeErrorKind, EncodeError, Encoding};
pub use error::ProtocolError;
pub use frame::{FrameCodec, FrameError};
pub use handshake::{Hello, PROTOCOL_VERSION, ProtocolVersion, SessionToken, Welcome};
pub use message::{ErrorObject, Id, Message, Notification, Request, Response, codes};
pub use method::{Method, NotificationMethod, Topic, builtin};
pub use server::{Peer, RequestContext, RouteInfo, RouteKind, Router, Server, ServerConfig};
pub use subscription::{EventFeed, EventStream, SeqLog, Sequenced, SubscriptionId};
