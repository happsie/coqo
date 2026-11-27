// SPDX-License-Identifier: Apache-2.0

//! Activation events: when the host activates a mod.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::id::{ContributionId, IdError, ServiceId};

/// When to activate a mod, as written in the manifest's `activation` list.
///
/// | Event | Fires when |
/// |---|---|
/// | `startup` | Coqo starts (use sparingly) |
/// | `command:<id>` | the command runs |
/// | `language:<id>` | a file of that language opens |
/// | `view:<id>` | the view is shown |
/// | `workspace_contains:<glob>` | the workspace has a matching file, e.g. `**/pom.xml` |
/// | `service:<id>` | another mod consumes the service, e.g. `coqo.git` |
/// | `mcp_tool:<name>` | an agent calls the MCP tool |
/// | `agent_provider:<id>` | a chat session starts with the agent provider |
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ActivationEvent {
    /// `startup`.
    Startup,
    /// `command:<id>`.
    Command(ContributionId),
    /// `language:<id>`.
    Language(ContributionId),
    /// `view:<id>`.
    View(ContributionId),
    /// `workspace_contains:<glob>`, with the glob relative to the workspace root.
    WorkspaceContains(String),
    /// `service:<id>`.
    Service(ServiceId),
    /// `mcp_tool:<name>`.
    McpTool(ContributionId),
    /// `agent_provider:<id>`.
    AgentProvider(ContributionId),
}

/// The event names [`ActivationEvent`] accepts, for error messages.
const EXPECTED: &str = "startup, command:<id>, language:<id>, view:<id>, \
     workspace_contains:<glob>, service:<id>, mcp_tool:<name>, agent_provider:<id>";

/// Why an activation event failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ActivationEventError {
    /// Not one of the known event kinds.
    #[error("unknown activation event `{0}`; expected one of {EXPECTED}")]
    Unknown(String),
    /// The event needs an argument after the colon.
    #[error("activation event `{0}` needs an argument after `:`")]
    MissingArgument(String),
    /// The argument isn't a valid id.
    #[error("in activation event `{event}`: {source}")]
    InvalidArgument {
        /// The whole event.
        event: String,
        /// Why the id was rejected.
        source: IdError,
    },
    /// The `workspace_contains` glob is absolute or leaves the workspace.
    #[error(
        "in activation event `{0}`: the glob must be relative to the workspace and must not use `..`"
    )]
    InvalidGlob(String),
}

impl FromStr for ActivationEvent {
    type Err = ActivationEventError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "startup" {
            return Ok(Self::Startup);
        }
        let Some((kind, arg)) = s.split_once(':') else {
            return Err(ActivationEventError::Unknown(s.to_owned()));
        };
        let known = [
            "command",
            "language",
            "view",
            "workspace_contains",
            "service",
            "mcp_tool",
            "agent_provider",
        ];
        if !known.contains(&kind) {
            return Err(ActivationEventError::Unknown(s.to_owned()));
        }
        if arg.is_empty() {
            return Err(ActivationEventError::MissingArgument(s.to_owned()));
        }
        let invalid = |source| ActivationEventError::InvalidArgument {
            event: s.to_owned(),
            source,
        };
        let cid = |arg: &str| ContributionId::new(arg).map_err(invalid);
        Ok(match kind {
            "command" => Self::Command(cid(arg)?),
            "language" => Self::Language(cid(arg)?),
            "view" => Self::View(cid(arg)?),
            "mcp_tool" => Self::McpTool(cid(arg)?),
            "agent_provider" => Self::AgentProvider(cid(arg)?),
            "service" => Self::Service(ServiceId::new(arg).map_err(invalid)?),
            "workspace_contains" => {
                if arg.starts_with('/') || arg.split('/').any(|seg| seg == "..") {
                    return Err(ActivationEventError::InvalidGlob(s.to_owned()));
                }
                Self::WorkspaceContains(arg.to_owned())
            }
            _ => unreachable!("kind checked above"),
        })
    }
}

impl fmt::Display for ActivationEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Startup => f.write_str("startup"),
            Self::Command(id) => write!(f, "command:{id}"),
            Self::Language(id) => write!(f, "language:{id}"),
            Self::View(id) => write!(f, "view:{id}"),
            Self::WorkspaceContains(glob) => write!(f, "workspace_contains:{glob}"),
            Self::Service(id) => write!(f, "service:{id}"),
            Self::McpTool(id) => write!(f, "mcp_tool:{id}"),
            Self::AgentProvider(id) => write!(f, "agent_provider:{id}"),
        }
    }
}

impl TryFrom<String> for ActivationEvent {
    type Error = ActivationEventError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ActivationEvent> for String {
    fn from(value: ActivationEvent) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays_every_kind() {
        for s in [
            "startup",
            "command:hello.greet",
            "language:java",
            "view:files.tree",
            "workspace_contains:**/pom.xml",
            "service:coqo.git",
            "mcp_tool:maven_test",
            "agent_provider:claude-acp",
        ] {
            let event: ActivationEvent = s.parse().unwrap();
            assert_eq!(event.to_string(), s);
        }
    }

    #[test]
    fn rejects_bad_events_precisely() {
        let err = |s: &str| s.parse::<ActivationEvent>().unwrap_err();
        assert!(matches!(err("onStartup"), ActivationEventError::Unknown(_)));
        assert!(matches!(err("file:x"), ActivationEventError::Unknown(_)));
        assert!(matches!(
            err("command:"),
            ActivationEventError::MissingArgument(_)
        ));
        assert!(matches!(
            err("command:Hello"),
            ActivationEventError::InvalidArgument { .. }
        ));
        assert!(matches!(
            err("workspace_contains:../x"),
            ActivationEventError::InvalidGlob(_)
        ));
        assert!(
            err("onStartup")
                .to_string()
                .contains("expected one of startup")
        );
    }
}
