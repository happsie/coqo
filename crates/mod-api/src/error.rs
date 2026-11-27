// SPDX-License-Identifier: Apache-2.0

//! The error type mods and hosts exchange.

use std::fmt;

use crate::id::{
    ContributionId, ContributionPointId, IdError, ProviderId, ServiceRef, ServiceVersion,
};

/// Why a mod call failed: activating, providing or consuming a service,
/// contributing, or running a command.
///
/// Hosts map their own errors onto these variants so a mod sees the same
/// errors whichever host runs it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ModError {
    /// An id or service reference didn't parse.
    #[error(transparent)]
    Id(#[from] IdError),
    /// Nobody provides the requested service.
    #[error("no provider for service `{0}`")]
    ServiceNotFound(ServiceRef),
    /// The service exists, but no active provider is compatible with the request.
    #[error("service `{requested}` not available; active versions: {}", fmt_versions(.available))]
    ServiceVersionMismatch {
        /// What was asked for.
        requested: ServiceRef,
        /// Versions of the active providers, one per major version.
        available: Vec<ServiceVersion>,
    },
    /// The provider registered a different Rust type than the consumer asked for.
    #[error("service `{service}` from `{provider}` is a `{actual}`, not a `{expected}`")]
    ServiceTypeMismatch {
        /// The resolved service.
        service: ServiceRef,
        /// Who provides it.
        provider: ProviderId,
        /// Type the consumer asked for.
        expected: &'static str,
        /// Type the provider registered.
        actual: &'static str,
    },
    /// The contribution point doesn't exist in this host.
    #[error("unknown contribution point `{0}`")]
    UnknownPoint(ContributionPointId),
    /// The point takes a different contribution type.
    #[error("contribution point `{point}` takes `{declared}`, not `{requested}`")]
    ContributionTypeMismatch {
        /// The point.
        point: ContributionPointId,
        /// Type the point takes.
        declared: &'static str,
        /// Type used in this call.
        requested: &'static str,
    },
    /// Another mod already contributed this id to the point.
    #[error("`{id}` is already contributed to `{point}` by `{existing}`")]
    DuplicateContribution {
        /// The point.
        point: ContributionPointId,
        /// The contested id.
        id: ContributionId,
        /// Who contributed it first.
        existing: ProviderId,
    },
    /// No handler is registered for the command.
    #[error("no handler for command `{0}`")]
    UnknownCommand(ContributionId),
    /// The host refused the call because the mod lacks a permission or didn't
    /// declare the service in its manifest.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// Anything else, with a message for the user and the logs.
    #[error("{0}")]
    Failed(String),
    /// An error from the mod's own code or libraries.
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl ModError {
    /// A [`ModError::Failed`] with `message`.
    pub fn failed(message: impl fmt::Display) -> Self {
        Self::Failed(message.to_string())
    }

    /// Wraps any error as [`ModError::Other`].
    pub fn other(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Other(Box::new(error))
    }
}

fn fmt_versions(versions: &[ServiceVersion]) -> String {
    if versions.is_empty() {
        return "none".to_owned();
    }
    versions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
