// SPDX-License-Identifier: Apache-2.0

//! Typed method definitions.
//!
//! A protocol method is a zero-sized type implementing [`Method`] (request →
//! response), [`NotificationMethod`] (fire and forget), or [`Topic`] (a
//! subscription stream). The [`Client`](crate::Client) and
//! [`Router`](crate::Router) use these to (de)serialize params and results,
//! so a method's name and types are declared exactly once:
//!
//! ```
//! use coqo_protocol::Method;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug)]
//! pub enum Echo {}
//!
//! #[derive(Debug, Serialize, Deserialize)]
//! pub struct EchoParams { pub text: String }
//!
//! impl Method for Echo {
//!     const NAME: &'static str = "test/echo";
//!     type Params = EchoParams;
//!     type Result = String;
//! }
//! ```
//!
//! Names starting with `coqo/` are reserved for the protocol itself
//! ([`builtin`]); `rpc.` is reserved by JSON-RPC 2.0.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Prefix of the protocol's own method names.
pub const RESERVED_PREFIX: &str = "coqo/";

/// A request/response method.
pub trait Method: 'static {
    /// The wire name, e.g. `workspace/open`.
    const NAME: &'static str;
    /// Request params. Use `()` for none.
    type Params: Serialize + DeserializeOwned + Send + 'static;
    /// Response result. Use `()` for none.
    type Result: Serialize + DeserializeOwned + Send + 'static;
}

/// A notification (no response).
pub trait NotificationMethod: 'static {
    /// The wire name.
    const NAME: &'static str;
    /// Notification params.
    type Params: Serialize + DeserializeOwned + Send + 'static;
}

/// A subscription topic: a stream of sequenced events.
///
/// Clients subscribe with [`builtin::Subscribe`], naming the topic and
/// optionally the last sequence number they saw.
pub trait Topic: 'static {
    /// The topic name, e.g. `workspace/events`.
    const NAME: &'static str;
    /// Params that select or filter the stream. Use `()` for none.
    type Params: Serialize + DeserializeOwned + Send + 'static;
    /// One event.
    type Event: Serialize + DeserializeOwned + Send + 'static;
}

/// Whether `name` is reserved for the protocol itself.
pub fn is_reserved(name: &str) -> bool {
    name.starts_with(RESERVED_PREFIX) || name.starts_with("rpc.")
}

/// The protocol's own methods.
pub mod builtin {
    use super::{Method, NotificationMethod};
    use crate::handshake::{Hello as HelloParams, Welcome};
    use crate::subscription::{
        EventParams, SubscribeParams, SubscribeResult, SubscriptionEndedParams, UnsubscribeParams,
    };

    /// `coqo/hello`: the handshake. Always the first request on a connection.
    #[derive(Debug)]
    pub enum Hello {}
    impl Method for Hello {
        const NAME: &'static str = "coqo/hello";
        type Params = HelloParams;
        type Result = Welcome;
    }

    /// `coqo/subscribe`: start a subscription to a topic.
    #[derive(Debug)]
    pub enum Subscribe {}
    impl Method for Subscribe {
        const NAME: &'static str = "coqo/subscribe";
        type Params = SubscribeParams;
        type Result = SubscribeResult;
    }

    /// `coqo/unsubscribe`: stop a subscription. Result is `true` if it existed.
    #[derive(Debug)]
    pub enum Unsubscribe {}
    impl Method for Unsubscribe {
        const NAME: &'static str = "coqo/unsubscribe";
        type Params = UnsubscribeParams;
        type Result = bool;
    }

    /// `coqo/event` (server → client): one event of a subscription.
    #[derive(Debug)]
    pub enum Event {}
    impl NotificationMethod for Event {
        const NAME: &'static str = "coqo/event";
        type Params = EventParams;
    }

    /// `coqo/subscriptionEnded` (server → client): the server ended a
    /// subscription, with an error if it could not continue (for example
    /// [`RESUME_GAP`](crate::codes::RESUME_GAP) when the client fell too far
    /// behind).
    #[derive(Debug)]
    pub enum SubscriptionEnded {}
    impl NotificationMethod for SubscriptionEnded {
        const NAME: &'static str = "coqo/subscriptionEnded";
        type Params = SubscriptionEndedParams;
    }
}
