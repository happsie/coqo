// SPDX-License-Identifier: Apache-2.0

//! Connection authentication: who may attach, and what a connection may do.
//!
//! A [`Server`](crate::Server) with an [`Authenticator`] checks the
//! [`SessionToken`] in each client's `coqo/hello`. A missing or wrong token
//! is answered with [`codes::UNAUTHORIZED`] and the connection is closed;
//! otherwise the authenticator's [`Scope`] is attached to every request on
//! that connection ([`RequestContext::scope`](crate::RequestContext)).
//! Handlers check it with [`Scope::require_daemon`] and
//! [`Scope::require_workspace`].
//!
//! A server without an authenticator gives every connection
//! [`Scope::Daemon`]: that is for in-process and test servers, never for a
//! socket other users or processes can reach.

use std::fmt;

use crate::handshake::SessionToken;
use crate::message::{ErrorObject, codes};

/// What an authenticated connection may do.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Scope {
    /// Everything: the daemon's own token (the UI, the CLI), or a server
    /// without authentication.
    Daemon,
    /// One agent session in one workspace: the token `coqod mcp` gets for
    /// the agent that launched it (PLAN §7.4). Daemon-wide methods and other
    /// workspaces are [`codes::FORBIDDEN`].
    Session {
        /// The workspace id.
        workspace: String,
        /// The agent session id.
        session: String,
    },
}

impl Scope {
    /// Whether this scope may act on `workspace`.
    pub fn allows_workspace(&self, workspace: &str) -> bool {
        match self {
            Scope::Daemon => true,
            Scope::Session { workspace: w, .. } => w == workspace,
        }
    }

    /// `Ok` for [`Scope::Daemon`], else a [`codes::FORBIDDEN`] error for
    /// `what` (the method name, say).
    pub fn require_daemon(&self, what: &str) -> Result<(), ErrorObject> {
        match self {
            Scope::Daemon => Ok(()),
            _ => Err(forbidden(format!(
                "{what} needs the daemon token; this connection's token is scoped to {self}"
            ))),
        }
    }

    /// `Ok` if this scope may act on `workspace`, else a
    /// [`codes::FORBIDDEN`] error.
    pub fn require_workspace(&self, workspace: &str) -> Result<(), ErrorObject> {
        if self.allows_workspace(workspace) {
            Ok(())
        } else {
            Err(forbidden(format!(
                "workspace {workspace} is outside this connection's scope ({self})"
            )))
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Daemon => f.write_str("the daemon"),
            Scope::Session { workspace, session } => {
                write!(f, "session {session} in workspace {workspace}")
            }
        }
    }
}

fn forbidden(message: String) -> ErrorObject {
    ErrorObject::new(codes::FORBIDDEN, message)
}

/// Why a client was not let in. The message is sent to the client, so it
/// must not contain the expected token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unauthorized(pub String);

/// Decides which clients may attach, and with what [`Scope`].
pub trait Authenticator: Send + Sync + fmt::Debug + 'static {
    /// Check the token from a client's `coqo/hello` (`None` if it sent
    /// none). Compare tokens in constant time ([`SessionToken`]'s `==`
    /// does).
    fn authenticate(&self, token: Option<&SessionToken>) -> Result<Scope, Unauthorized>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_scope_is_limited_to_its_workspace() {
        let s = Scope::Session {
            workspace: "ws-a".into(),
            session: "s-1".into(),
        };
        assert!(s.allows_workspace("ws-a"));
        assert!(!s.allows_workspace("ws-b"));
        assert!(s.require_workspace("ws-a").is_ok());
        let e = s.require_workspace("ws-b").unwrap_err();
        assert_eq!(e.code, codes::FORBIDDEN);
        assert!(e.message.contains("ws-b"), "{}", e.message);
        assert_eq!(
            s.require_daemon("daemon/shutdown").unwrap_err().code,
            codes::FORBIDDEN
        );

        assert!(Scope::Daemon.allows_workspace("anything"));
        assert!(Scope::Daemon.require_daemon("daemon/shutdown").is_ok());
    }
}
