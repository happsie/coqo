// SPDX-License-Identifier: Apache-2.0

//! The `coqo.mod.toml` manifest (PLAN §5).
//!
//! Every mod, whatever its tier, ships one. It says what the mod is, which
//! Coqo versions it supports, how to run it, when to activate it, which
//! services it provides and consumes, what it may touch, and what it
//! contributes up front.
//!
//! ```toml
//! id = "coqo.hello"
//! name = "Hello"
//! version = "0.1.0"
//! engine = "^0.0"
//! tier = "core"
//! activation = ["startup"]
//! provides = ["coqo.greeter@1"]
//! consumes = ["coqo.git@1", { service = "coqo.github@1", optional = true }]
//!
//! [[contributes.commands]]
//! id = "hello.greet"
//! title = "Say hello"
//! ```
//!
//! Parse it with [`Manifest::from_toml_str`], which also validates it and
//! reports every problem with its line, column and field path. The full
//! format, with every contribution point, is documented in
//! `docs/mods/manifest.md`; `crates/mod-api/examples/coqo.mod.toml` is a
//! complete example.

mod activation;
mod error;
mod permissions;
mod validate;

use std::fmt;
use std::str::FromStr;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use activation::{ActivationEvent, ActivationEventError};
pub use error::{ManifestError, ManifestProblem, Position};
pub use permissions::{FsPermissions, Permissions};

use crate::contrib::{
    AgentProviderInfo, AlertSource, Command, ContextProvider, DebugAdapter, Keybinding, Language,
    LspServer, McpTool, MenuItem, Setting, SlashCommand, Theme, Views,
};
use crate::error::ModError;
use crate::id::{ContributionId, ProviderId, ServiceRef};
use crate::module::ModContext;
use crate::points::{self, ContributionPoint};

/// The manifest's file name.
pub const FILE_NAME: &str = "coqo.mod.toml";

/// How a mod runs (PLAN §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A Rust crate linked into Coqo, implementing [`Mod`](crate::Mod).
    /// Takes no `entry`.
    Core,
    /// A WASM component run sandboxed by the daemon. `entry` is the
    /// component's path, relative to the mod's folder, ending in `.wasm`.
    Wasm,
    /// An executable speaking the Coqo Mod Protocol over stdio, supervised by
    /// the daemon. `entry` is the program (relative to the mod's folder, or a
    /// name on `PATH`) and `args` its arguments.
    Process,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Core => "core",
            Self::Wasm => "wasm",
            Self::Process => "process",
        })
    }
}

/// A service a mod consumes: `"coqo.git@1"`, or
/// `{ service = "coqo.git@1", optional = true }` if the mod works without it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceDependency {
    /// The service and minimum version.
    pub service: ServiceRef,
    /// Whether the mod still activates when no provider is available.
    pub optional: bool,
}

impl ServiceDependency {
    /// A required dependency on `service`.
    pub fn required(service: ServiceRef) -> Self {
        Self {
            service,
            optional: false,
        }
    }
}

impl Serialize for ServiceDependency {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Table<'a> {
            service: &'a ServiceRef,
            optional: bool,
        }
        if self.optional {
            Table {
                service: &self.service,
                optional: true,
            }
            .serialize(serializer)
        } else {
            self.service.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for ServiceDependency {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DependencyVisitor;

        impl<'de> Visitor<'de> for DependencyVisitor {
            type Value = ServiceDependency;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a service reference such as \"coqo.git@1\", or a table { service = \"coqo.git@1\", optional = true }",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse()
                    .map(ServiceDependency::required)
                    .map_err(E::custom)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut service = None;
                let mut optional = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "service" if service.is_some() => {
                            return Err(de::Error::duplicate_field("service"));
                        }
                        "service" => service = Some(map.next_value::<ServiceRef>()?),
                        "optional" if optional.is_some() => {
                            return Err(de::Error::duplicate_field("optional"));
                        }
                        "optional" => optional = Some(map.next_value::<bool>()?),
                        other => {
                            return Err(de::Error::unknown_field(other, &["service", "optional"]));
                        }
                    }
                }
                Ok(ServiceDependency {
                    service: service.ok_or_else(|| de::Error::missing_field("service"))?,
                    optional: optional.unwrap_or(false),
                })
            }
        }

        deserializer.deserialize_any(DependencyVisitor)
    }
}

/// A parsed `coqo.mod.toml`.
///
/// Build it with [`Manifest::from_toml_str`] (or `str::parse`), which
/// validates it. Deserializing it any other way skips validation; call
/// [`validate`](Self::validate) then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Mod id, such as `coqo.git`. Also the provider id of everything the mod
    /// registers.
    pub id: ProviderId,
    /// Display name; defaults to the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The mod's own version (semver).
    pub version: semver::Version,
    /// One-line description for the mod manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Authors, as `Name <email>` or just a name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    /// SPDX licence expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Source repository URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Range of mod API versions ([`API_VERSION`](crate::API_VERSION)) the
    /// mod works with, as a semver requirement such as `^0.3`.
    pub engine: semver::VersionReq,
    /// How the mod runs.
    pub tier: Tier,
    /// What to run: see [`Tier`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// Arguments for a process mod's `entry`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// When to activate the mod. Empty means only when something it
    /// provides is needed, or never for a purely declarative mod (a theme,
    /// say).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activation: Vec<ActivationEvent>,
    /// Services the mod provides, at their exact versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<ServiceRef>,
    /// Services the mod consumes, at their minimum versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumes: Vec<ServiceDependency>,
    /// What the mod may touch.
    #[serde(default, skip_serializing_if = "Permissions::is_empty")]
    pub permissions: Permissions,
    /// Static contributions, registered before the mod activates.
    #[serde(default, skip_serializing_if = "Contributes::is_empty")]
    pub contributes: Contributes,
}

impl Manifest {
    /// Parses and validates a manifest.
    ///
    /// On failure the error lists every problem found, each with its line,
    /// column and field path. A TOML syntax or type error stops parsing, so
    /// it is reported alone.
    pub fn from_toml_str(source: &str) -> Result<Self, ManifestError> {
        let manifest: Self =
            toml::from_str(source).map_err(|e| ManifestError::from_toml(source, &e))?;
        let problems = validate::validate(&manifest);
        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(ManifestError::from_problems(Some(source), problems))
        }
    }

    /// Validates a manifest built in code or deserialized from elsewhere.
    /// Problems carry field paths but no positions.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let problems = validate::validate(self);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(ManifestError::from_problems(None, problems))
        }
    }

    /// Writes the manifest back as TOML. Parsing the result gives an equal
    /// manifest.
    pub fn to_toml_string(&self) -> Result<String, ManifestError> {
        toml::to_string(self).map_err(|e| {
            ManifestError::new(vec![ManifestProblem {
                path: None,
                message: format!("can't write manifest as TOML: {e}"),
                span: None,
                position: None,
            }])
        })
    }

    /// The display name, falling back to the id.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(self.id.as_str())
    }

    /// Whether the mod works with mod API version `api` (normally
    /// [`api_version`](crate::api_version)).
    pub fn supports_api(&self, api: &semver::Version) -> bool {
        self.engine.matches(api)
    }

    /// Whether `event` is one of the mod's activation events.
    pub fn activates_on(&self, event: &ActivationEvent) -> bool {
        self.activation.contains(event)
    }
}

impl FromStr for Manifest {
    type Err = ManifestError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_toml_str(s)
    }
}

/// The `[contributes]` table: contributions known before the mod runs.
///
/// Each key is a contribution point id (see [`points`]), and
/// each entry has that point's value type from [`contrib`](crate::contrib).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contributes {
    /// `[[contributes.commands]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,
    /// `[[contributes.keybindings]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keybindings: Vec<Keybinding>,
    /// `[[contributes.menus]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub menus: Vec<MenuItem>,
    /// `[[contributes.settings]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<Setting>,
    /// `[[contributes.views.<location>]]`.
    #[serde(default, skip_serializing_if = "Views::is_empty")]
    pub views: Views,
    /// `[[contributes.languages]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<Language>,
    /// `[[contributes.lsp_servers]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lsp_servers: Vec<LspServer>,
    /// `[[contributes.debug_adapters]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub debug_adapters: Vec<DebugAdapter>,
    /// `[[contributes.mcp_tools]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_tools: Vec<McpTool>,
    /// `[[contributes.agent_providers]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_providers: Vec<AgentProviderInfo>,
    /// `[[contributes.slash_commands]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slash_commands: Vec<SlashCommand>,
    /// `[[contributes.context_providers]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_providers: Vec<ContextProvider>,
    /// `[[contributes.alert_sources]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_sources: Vec<AlertSource>,
    /// `[[contributes.themes]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<Theme>,
}

impl Contributes {
    /// Whether nothing is contributed.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Registers every contribution through `cx`. Hosts call this when they
    /// load a mod, before activating it.
    ///
    /// Keybindings and menu items have no id of their own, so they are
    /// registered as `<mod id>.keybinding-<n>` and `<mod id>.menu-<n>`.
    pub fn register(&self, cx: &ModContext) -> Result<(), ModError> {
        fn all<P>(cx: &ModContext, values: &[P::Value]) -> Result<(), ModError>
        where
            P: ContributionPoint,
            P::Value: crate::contrib::Keyed + Clone,
        {
            values
                .iter()
                .try_for_each(|v| cx.contribute::<P>(v.clone()))
        }

        all::<points::Commands>(cx, &self.commands)?;
        for (n, binding) in self.keybindings.iter().enumerate() {
            let id = generated_id(cx.mod_id(), "keybinding", n)?;
            cx.contribute_as::<points::Keybindings>(id, binding.clone())?;
        }
        for (n, item) in self.menus.iter().enumerate() {
            let id = generated_id(cx.mod_id(), "menu", n)?;
            cx.contribute_as::<points::Menus>(id, item.clone())?;
        }
        all::<points::Settings>(cx, &self.settings)?;
        all::<points::SidebarViews>(cx, &self.views.sidebar)?;
        all::<points::PanelViews>(cx, &self.views.panel)?;
        all::<points::TabViews>(cx, &self.views.tab)?;
        all::<points::RailViews>(cx, &self.views.rail)?;
        all::<points::StatusItems>(cx, &self.views.status)?;
        all::<points::Languages>(cx, &self.languages)?;
        all::<points::LspServers>(cx, &self.lsp_servers)?;
        all::<points::DebugAdapters>(cx, &self.debug_adapters)?;
        all::<points::McpTools>(cx, &self.mcp_tools)?;
        all::<points::AgentProviders>(cx, &self.agent_providers)?;
        all::<points::SlashCommands>(cx, &self.slash_commands)?;
        all::<points::ContextProviders>(cx, &self.context_providers)?;
        all::<points::AlertSources>(cx, &self.alert_sources)?;
        all::<points::Themes>(cx, &self.themes)?;
        Ok(())
    }
}

fn generated_id(mod_id: &ProviderId, kind: &str, n: usize) -> Result<ContributionId, ModError> {
    Ok(ContributionId::new(format!("{mod_id}.{kind}-{n}"))?)
}

#[cfg(test)]
mod tests;
