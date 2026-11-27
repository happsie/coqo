// SPDX-License-Identifier: Apache-2.0

//! Contribution points: typed handles for where contributions go.
//!
//! Each point is a marker type implementing [`ContributionPoint`], which
//! names the point's id and the Rust type its contributions carry. Use them
//! with [`ModContext`](crate::ModContext):
//!
//! ```
//! # use coqo_mod_api::{ModContext, ModError, points, contrib::StatusItem};
//! # fn demo(cx: &ModContext) -> Result<(), ModError> {
//! cx.contribute::<points::StatusItems>(StatusItem::new("hello.status".parse()?, "Hello"))?;
//! # Ok(()) }
//! ```
//!
//! A point id is the same as its key path under `[contributes]` in
//! `coqo.mod.toml`: `[[contributes.commands]]` goes to `commands`,
//! `[[contributes.views.sidebar]]` to `views.sidebar`.
//!
//! The set is open: a crate that owns a new point (the editor's decorations,
//! say) defines its own marker type the same way. A point's value type can be
//! plain data, as here, or a runtime handle such as `Arc<dyn Trait>` for
//! in-process mods (see [`CommandHandlers`]).

use std::sync::Arc;

use crate::contrib::{
    AgentProviderInfo, AlertSource, Command, ContextProvider, DebugAdapter, Keybinding, Language,
    LspServer, McpTool, MenuItem, Setting, SlashCommand, StatusItem, Theme, View, ViewLocation,
};
use crate::id::ContributionPointId;
use crate::{CommandHandler, McpToolHandler};

/// A typed contribution point.
pub trait ContributionPoint: 'static {
    /// The point's id, such as `commands` or `views.sidebar`. Must be a valid
    /// [`ContributionPointId`].
    const ID: &'static str;

    /// What each contribution to the point carries.
    type Value: Send + Sync + 'static;

    /// [`ID`](Self::ID) as a [`ContributionPointId`].
    ///
    /// # Panics
    ///
    /// If `ID` is not a valid contribution point id, which is a bug in the
    /// point's definition.
    fn id() -> ContributionPointId {
        ContributionPointId::new(Self::ID).unwrap_or_else(|e| {
            panic!(
                "ContributionPoint::ID of {}: {e}",
                std::any::type_name::<Self>()
            )
        })
    }
}

macro_rules! points {
    ($($(#[$meta:meta])* $name:ident = $id:literal => $value:ty;)*) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, Default)]
            pub struct $name;

            impl ContributionPoint for $name {
                const ID: &'static str = $id;
                type Value = $value;
            }
        )*

        /// Ids of every v1 contribution point, in declaration order. Hosts
        /// declare these at startup.
        pub const ALL: &[&str] = &[$($id),*];
    };
}

points! {
    /// Commands: `commands` takes [`Command`].
    Commands = "commands" => Command;
    /// Command handlers of in-process mods, keyed by command id:
    /// `command_handlers` takes `Arc<dyn CommandHandler>`. WASM and process
    /// hosts register a handler that forwards to the mod.
    CommandHandlers = "command_handlers" => Arc<dyn CommandHandler>;
    /// Keybindings: `keybindings` takes [`Keybinding`].
    Keybindings = "keybindings" => Keybinding;
    /// Menu items: `menus` takes [`MenuItem`].
    Menus = "menus" => MenuItem;
    /// Settings schema: `settings` takes [`Setting`].
    Settings = "settings" => Setting;
    /// Sidebar views: `views.sidebar` takes [`View`].
    SidebarViews = "views.sidebar" => View;
    /// Bottom panel views: `views.panel` takes [`View`].
    PanelViews = "views.panel" => View;
    /// Main-area tab views: `views.tab` takes [`View`].
    TabViews = "views.tab" => View;
    /// Workspace rail views: `views.rail` takes [`View`].
    RailViews = "views.rail" => View;
    /// Status bar items: `views.status` takes [`StatusItem`].
    StatusItems = "views.status" => StatusItem;
    /// Languages: `languages` takes [`Language`].
    Languages = "languages" => Language;
    /// Language servers: `lsp_servers` takes [`LspServer`].
    LspServers = "lsp_servers" => LspServer;
    /// Debug adapters: `debug_adapters` takes [`DebugAdapter`].
    DebugAdapters = "debug_adapters" => DebugAdapter;
    /// MCP tools for agents: `mcp_tools` takes [`McpTool`].
    McpTools = "mcp_tools" => McpTool;
    /// MCP tool handlers of in-process mods, keyed by tool name:
    /// `mcp_tool_handlers` takes `Arc<dyn McpToolHandler>`. WASM and process
    /// hosts register a handler that forwards to the mod.
    McpToolHandlers = "mcp_tool_handlers" => Arc<dyn McpToolHandler>;
    /// Agent providers: `agent_providers` takes [`AgentProviderInfo`].
    AgentProviders = "agent_providers" => AgentProviderInfo;
    /// Chat slash commands: `slash_commands` takes [`SlashCommand`].
    SlashCommands = "slash_commands" => SlashCommand;
    /// Chat `@`-mention providers: `context_providers` takes [`ContextProvider`].
    ContextProviders = "context_providers" => ContextProvider;
    /// Alert sources: `alert_sources` takes [`AlertSource`].
    AlertSources = "alert_sources" => AlertSource;
    /// Colour themes: `themes` takes [`Theme`].
    Themes = "themes" => Theme;
}

/// The point that holds views of `location`. For
/// [`ViewLocation::Status`] that is [`StatusItems`], whose value type is
/// [`StatusItem`] rather than [`View`].
pub fn view_point(location: ViewLocation) -> ContributionPointId {
    match location {
        ViewLocation::Sidebar => SidebarViews::id(),
        ViewLocation::Panel => PanelViews::id(),
        ViewLocation::Tab => TabViews::id(),
        ViewLocation::Status => StatusItems::id(),
        ViewLocation::Rail => RailViews::id(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_point_id_is_valid_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for id in ALL {
            ContributionPointId::new(*id).unwrap();
            assert!(seen.insert(*id), "{id} listed twice");
        }
        assert_eq!(Commands::id().as_str(), "commands");
        assert_eq!(view_point(ViewLocation::Status), StatusItems::id());
    }
}
