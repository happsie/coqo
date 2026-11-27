// SPDX-License-Identifier: Apache-2.0

//! Connection authentication: who may attach, and what a connection may do.
//!
//! A [`Server`](crate::Server) with an [`Authenticator`] checks the
//! [`SessionToken`] in each client's `coqo/hello`. A missing or wrong token
//! is answered with [`codes::UNAUTHORIZED`] and the connection is closed;
//! otherwise the authenticator's [`Scope`] is attached to every request on
//! that connection ([`RequestContext::scope`](crate::RequestContext)).
//!
//! **Session scopes are denied by default.** Each route declares who may
//! call it when it is registered, with an [`Access`]
//! ([`Router::method_with`](crate::Router::method_with) and friends), and
//! the router checks it before the handler runs. A route registered without
//! one ([`Router::method`](crate::Router::method)) answers every
//! non-daemon scope with [`codes::FORBIDDEN`], so a handler that forgets to
//! check cannot let a session token through. [`Scope::Daemon`] connections
//! are never checked.
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

    /// Whether this scope may act on agent session `session`.
    pub fn allows_session(&self, session: &str) -> bool {
        match self {
            Scope::Daemon => true,
            Scope::Session { session: s, .. } => s == session,
        }
    }

    /// `Ok` if this scope may act on agent session `session`, else a
    /// [`codes::FORBIDDEN`] error.
    pub fn require_session(&self, session: &str) -> Result<(), ErrorObject> {
        if self.allows_session(session) {
            Ok(())
        } else {
            Err(forbidden(format!(
                "agent session {session} is outside this connection's scope ({self})"
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

type CheckFn<P> = Box<dyn Fn(&Scope, &P) -> Result<(), ErrorObject> + Send + Sync>;

/// Who may call a route: declared when the route is registered
/// ([`Router::method_with`](crate::Router::method_with),
/// [`notification_with`](crate::Router::notification_with),
/// [`topic_with`](crate::Router::topic_with)) and checked by the router
/// before the handler runs. `P` is the route's params type.
///
/// [`Scope::Daemon`] connections may call every route; an `Access` only
/// limits the other scopes (session tokens):
///
/// - [`Access::daemon`]: no session token. They get [`codes::FORBIDDEN`]
///   before the params are even parsed.
/// - [`Access::workspace`]: session tokens for their own workspace, read
///   from the params.
/// - [`Access::session`]: session tokens for their own agent session, read
///   from the params.
/// - [`Access::check`]: session tokens, if a custom check passes (for
///   example one that looks up which workspace a buffer id belongs to).
///
/// ```
/// # use coqo_protocol::{Access, Method, Router};
/// # use serde::{Deserialize, Serialize};
/// # #[derive(Debug)] enum ReadFile {}
/// # #[derive(Debug, Serialize, Deserialize)]
/// # struct ReadParams { workspace: String, path: String }
/// # impl Method for ReadFile {
/// #     const NAME: &'static str = "fs/read";
/// #     type Params = ReadParams;
/// #     type Result = String;
/// # }
/// let router = Router::new().method_with::<ReadFile, _, _>(
///     Access::workspace(|p| &p.workspace),
///     |_ctx, p| async move { Ok(format!("{} in {}", p.path, p.workspace)) },
/// );
/// ```
pub struct Access<P> {
    rule: Rule<P>,
    declared: bool,
}

enum Rule<P> {
    Daemon,
    Workspace(fn(&P) -> &str),
    Session(fn(&P) -> &str),
    Check(CheckFn<P>),
}

/// What an [`Access`] lets session tokens do, for listing routes
/// ([`Router::routes`](crate::Router::routes)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AccessKind {
    /// Daemon token only ([`Access::daemon`]).
    Daemon,
    /// Session tokens, for their own workspace ([`Access::workspace`]).
    Workspace,
    /// Session tokens, for their own agent session ([`Access::session`]).
    Session,
    /// Session tokens, if a custom check passes ([`Access::check`]).
    Check,
}

impl AccessKind {
    /// Whether some session token may call the route.
    pub fn opens_to_sessions(self) -> bool {
        self != AccessKind::Daemon
    }
}

impl<P> Access<P> {
    /// Only the daemon token. Session tokens get [`codes::FORBIDDEN`].
    pub fn daemon() -> Self {
        Self {
            rule: Rule::Daemon,
            declared: true,
        }
    }

    /// What [`Router::method`](crate::Router::method) and friends use:
    /// daemon only, but reported as not declared.
    pub(crate) fn undeclared() -> Self {
        Self {
            rule: Rule::Daemon,
            declared: false,
        }
    }

    /// Session tokens too, for the workspace `workspace` reads from the
    /// params ([`Scope::require_workspace`]).
    pub fn workspace(workspace: fn(&P) -> &str) -> Self {
        Self {
            rule: Rule::Workspace(workspace),
            declared: true,
        }
    }

    /// Session tokens too, for the agent session `session` reads from the
    /// params ([`Scope::require_session`]).
    pub fn session(session: fn(&P) -> &str) -> Self {
        Self {
            rule: Rule::Session(session),
            declared: true,
        }
    }

    /// Session tokens too, when `check` returns `Ok`. It sees only
    /// non-daemon scopes, after the params are parsed and before the
    /// handler runs. Return a [`codes::FORBIDDEN`] error (from
    /// [`Scope::require_workspace`], say) to refuse.
    pub fn check(
        check: impl Fn(&Scope, &P) -> Result<(), ErrorObject> + Send + Sync + 'static,
    ) -> Self {
        Self {
            rule: Rule::Check(Box::new(check)),
            declared: true,
        }
    }

    /// What this lets session tokens do; `None` if it was not declared.
    pub(crate) fn kind(&self) -> Option<AccessKind> {
        self.declared.then_some(match self.rule {
            Rule::Daemon => AccessKind::Daemon,
            Rule::Workspace(_) => AccessKind::Workspace,
            Rule::Session(_) => AccessKind::Session,
            Rule::Check(_) => AccessKind::Check,
        })
    }

    /// The check before the params are parsed: daemon-only routes refuse
    /// other scopes here.
    pub(crate) fn admit(&self, scope: &Scope, route: &str) -> Result<(), ErrorObject> {
        match self.rule {
            Rule::Daemon => scope.require_daemon(route),
            _ => Ok(()),
        }
    }

    /// The check with the parsed params, before the handler runs.
    pub(crate) fn allow(&self, scope: &Scope, params: &P, route: &str) -> Result<(), ErrorObject> {
        if *scope == Scope::Daemon {
            return Ok(());
        }
        match &self.rule {
            Rule::Daemon => scope.require_daemon(route),
            Rule::Workspace(workspace) => scope.require_workspace(workspace(params)),
            Rule::Session(session) => scope.require_session(session(params)),
            Rule::Check(check) => check(scope, params),
        }
    }
}

impl<P> fmt::Debug for Access<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            Some(kind) => write!(f, "Access::{kind:?}"),
            None => f.write_str("Access::Undeclared"),
        }
    }
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
