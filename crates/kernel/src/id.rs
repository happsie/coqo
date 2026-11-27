// SPDX-License-Identifier: Apache-2.0

//! Identifiers shared by the service and contribution registries.
//!
//! These are plain, serialisable value types with no kernel dependencies, so
//! they can move to `coqo-mod-api` (#31) and be re-exported from here without
//! breaking anyone: the kernel will `pub use coqo_mod_api::id::*`.
//!
//! All names use the same grammar: one or more dot-separated segments, each
//! starting with a lowercase ASCII letter or digit and continuing with
//! lowercase letters, digits, `-` or `_`. Examples: `coqo.git`, `commands`,
//! `views.sidebar`, `git.commit`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why a name or service reference failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdError {
    /// The name is empty.
    #[error("{kind} is empty")]
    Empty {
        /// Which kind of identifier was being parsed.
        kind: &'static str,
    },
    /// The name breaks the dotted-segment grammar.
    #[error("invalid {kind} `{value}`: use dot-separated segments of [a-z0-9][a-z0-9_-]*")]
    InvalidName {
        /// Which kind of identifier was being parsed.
        kind: &'static str,
        /// The rejected input.
        value: String,
    },
    /// A service reference lacks the `@<major>[.<minor>]` suffix.
    #[error("service reference `{0}` has no version; expected `<id>@<major>[.<minor>]`")]
    MissingVersion(String),
    /// The version part isn't `<major>` or `<major>.<minor>`.
    #[error("invalid service version `{0}`; expected `<major>` or `<major>.<minor>`")]
    InvalidVersion(String),
}

fn validate_name(kind: &'static str, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty { kind });
    }
    let segment_ok = |s: &str| {
        let mut chars = s.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    if value.split('.').all(segment_ok) {
        Ok(())
    } else {
        Err(IdError::InvalidName {
            kind,
            value: value.to_owned(),
        })
    }
}

macro_rules! name_type {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Parses and validates a name.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_name($kind, &value)?;
                Ok(Self(value))
            }

            /// The name as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

name_type!(
    /// Name of a service, without its version: `coqo.git`, `coqo.editor`.
    ServiceId,
    "service id"
);

name_type!(
    /// Who provides services or contributions: a mod id such as `coqo.git`.
    ///
    /// Built-in providers are ordinary ids too; whether a provider is built in
    /// is recorded separately (see [`crate::service::ProviderKind`]).
    ProviderId,
    "provider id"
);

name_type!(
    /// A contribution point: `commands`, `keybindings`, `views.sidebar`, `mcp.tools`.
    ContributionPointId,
    "contribution point id"
);

name_type!(
    /// Key of one contribution within a point, e.g. a command id `git.commit`.
    ContributionId,
    "contribution id"
);

/// A service interface version: `major.minor`.
///
/// The major version names an interface; a new major is a different,
/// incompatible interface that can be provided alongside the old one. Minor
/// versions only add to an interface, so a provider at `1.3` satisfies a
/// consumer that asks for `1.1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServiceVersion {
    /// Incompatible interface generation.
    pub major: u32,
    /// Backwards-compatible additions within the major version.
    pub minor: u32,
}

impl ServiceVersion {
    /// Builds a version from its parts.
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Whether a provider at `self` can serve a consumer that requires `required`.
    pub const fn satisfies(self, required: ServiceVersion) -> bool {
        self.major == required.major && self.minor >= required.minor
    }
}

impl fmt::Display for ServiceVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.minor == 0 {
            write!(f, "{}", self.major)
        } else {
            write!(f, "{}.{}", self.major, self.minor)
        }
    }
}

impl FromStr for ServiceVersion {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || IdError::InvalidVersion(s.to_owned());
        let number = |part: &str| {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            part.parse::<u32>().map_err(|_| invalid())
        };
        match s.split_once('.') {
            None => Ok(Self::new(number(s)?, 0)),
            Some((major, minor)) => Ok(Self::new(number(major)?, number(minor)?)),
        }
    }
}

impl TryFrom<String> for ServiceVersion {
    type Error = IdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ServiceVersion> for String {
    fn from(value: ServiceVersion) -> Self {
        value.to_string()
    }
}

/// A versioned service reference: `coqo.git@1` or `coqo.git@1.2`.
///
/// When providing, it states the exact interface version on offer. When
/// consuming, it states the minimum compatible version (same major, minor at
/// least this).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServiceRef {
    /// The service name.
    pub id: ServiceId,
    /// The interface version.
    pub version: ServiceVersion,
}

impl ServiceRef {
    /// Builds a reference from its parts.
    pub fn new(id: ServiceId, version: ServiceVersion) -> Self {
        Self { id, version }
    }
}

impl fmt::Display for ServiceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.id, self.version)
    }
}

impl FromStr for ServiceRef {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (id, version) = s
            .split_once('@')
            .ok_or_else(|| IdError::MissingVersion(s.to_owned()))?;
        Ok(Self::new(ServiceId::new(id)?, version.parse()?))
    }
}

impl TryFrom<String> for ServiceRef {
    type Error = IdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<&str> for ServiceRef {
    type Error = IdError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ServiceRef> for String {
    fn from(value: ServiceRef) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names() {
        for ok in ["coqo.git", "commands", "views.sidebar", "a-b_c.0x"] {
            assert_eq!(ServiceId::new(ok).unwrap().as_str(), ok);
        }
        for bad in ["", "Coqo", ".git", "git.", "a..b", "-a", "a b", "a@1"] {
            assert!(ServiceId::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn parses_service_refs() {
        let r: ServiceRef = "coqo.git@1".parse().unwrap();
        assert_eq!(r.id.as_str(), "coqo.git");
        assert_eq!(r.version, ServiceVersion::new(1, 0));
        assert_eq!(r.to_string(), "coqo.git@1");

        let r: ServiceRef = "coqo.git@2.3".parse().unwrap();
        assert_eq!(r.version, ServiceVersion::new(2, 3));
        assert_eq!(r.to_string(), "coqo.git@2.3");

        assert!(matches!(
            "coqo.git".parse::<ServiceRef>(),
            Err(IdError::MissingVersion(_))
        ));
        for bad in [
            "coqo.git@",
            "coqo.git@x",
            "coqo.git@1.",
            "coqo.git@1.2.3",
            "coqo.git@+1",
        ] {
            assert!(
                matches!(bad.parse::<ServiceRef>(), Err(IdError::InvalidVersion(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn version_compatibility() {
        let v = ServiceVersion::new;
        assert!(v(1, 3).satisfies(v(1, 0)));
        assert!(v(1, 3).satisfies(v(1, 3)));
        assert!(!v(1, 2).satisfies(v(1, 3)));
        assert!(!v(2, 0).satisfies(v(1, 0)));
        assert!(!v(1, 0).satisfies(v(2, 0)));
    }

    #[test]
    fn serde_round_trips_as_strings() {
        let r: ServiceRef = "coqo.git@1.2".parse().unwrap();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#""coqo.git@1.2""#);
        assert_eq!(serde_json::from_str::<ServiceRef>(&json).unwrap(), r);
        assert!(serde_json::from_str::<ServiceId>(r#""Not Valid""#).is_err());
    }
}
