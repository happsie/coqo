// SPDX-License-Identifier: Apache-2.0

//! Session manager errors and their protocol error codes.

use coqo_protocol::ErrorObject;
use coqo_protocol::methods::agents::{PermissionRequestId, SessionId};
use coqo_protocol::methods::codes;

use crate::provider::ProviderError;
use crate::store::StoreError;

/// Why a session operation failed.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// No such session.
    #[error("unknown agent session {0}")]
    UnknownSession(SessionId),
    /// No such provider, or it is not available.
    #[error("unknown or unavailable agent provider `{0}`")]
    UnknownProvider(String),
    /// A turn is already running.
    #[error("agent session {0} is already running a turn")]
    Busy(SessionId),
    /// The session was closed.
    #[error("agent session {0} is closed")]
    Closed(SessionId),
    /// The agent's session can't be resumed, so the session takes no more
    /// prompts.
    #[error("the agent can't resume this chat ({reason}); start a new chat with its context")]
    Ended {
        /// The session.
        session: SessionId,
        /// Why, from the agent.
        reason: String,
    },
    /// No such pending permission request, or not one of its options.
    #[error("agent session {session} has no pending permission request {request} with that option")]
    UnknownPermission {
        /// The session.
        session: SessionId,
        /// The request.
        request: u64,
    },
    /// Invalid arguments.
    #[error("{0}")]
    InvalidParams(String),
    /// The provider failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The store failed.
    #[error("agent session store: {0}")]
    Store(#[from] StoreError),
}

impl AgentError {
    pub(crate) fn unknown_permission(session: &SessionId, request: PermissionRequestId) -> Self {
        Self::UnknownPermission {
            session: session.clone(),
            request: request.0,
        }
    }

    /// The protocol error for this failure.
    pub fn to_error(&self) -> ErrorObject {
        let code = match self {
            Self::UnknownSession(_) => codes::UNKNOWN_SESSION,
            Self::UnknownProvider(_) => codes::UNKNOWN_PROVIDER,
            Self::Busy(_) => codes::SESSION_BUSY,
            Self::Closed(_) => codes::SESSION_CLOSED,
            Self::Ended { .. } | Self::Provider(ProviderError::SessionLost(_)) => {
                codes::SESSION_ENDED
            }
            Self::UnknownPermission { .. } => codes::UNKNOWN_PERMISSION_REQUEST,
            Self::InvalidParams(m) => return ErrorObject::invalid_params(m.clone()),
            Self::Provider(ProviderError::AuthRequired(_)) => codes::AUTH_REQUIRED,
            Self::Provider(ProviderError::Unsupported(_)) => codes::UNSUPPORTED,
            Self::Provider(ProviderError::Failed(_)) => codes::PROVIDER_ERROR,
            Self::Store(_) => codes::STORAGE_ERROR,
        };
        ErrorObject::new(code, self.to_string())
    }
}

impl From<AgentError> for ErrorObject {
    fn from(e: AgentError) -> Self {
        e.to_error()
    }
}
