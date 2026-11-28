// SPDX-License-Identifier: Apache-2.0

//! The workbench's main tab area (PLAN §7.1, design board 1): tab groups in
//! resizable splits, drawn as floating rounded panels on the ground.
//!
//! - [`tree`]: the layout as plain, serializable data ([`PaneTree`]).
//! - [`TabItem`] / [`AnyTab`]: what a tab is. Any kind of tab (file, chat, diff,
//!   PR, trace) can sit in any group, beside any other.
//! - [`register_tab_kind`]: how a saved tab is rebuilt after a restart.
//! - [`Dock`]: the view, with the open / move / split / close API.
//! - [`store`]: saving and loading the whole workbench layout.
//!
//! Keyboard: split right `Ctrl \`, split down `Ctrl Alt \`, close tab
//! `Ctrl W`, close group `Ctrl Alt W` (`Cmd` for `Ctrl` on macOS). Shift-clicking the split
//! button splits down.

pub mod store;
pub mod tree;

mod tab;
mod view;

pub use tab::{AnyTab, MissingTab, TabItem, TabRegistry, register_tab_kind};
pub use tree::{Edge, NodeId, PaneNode, PaneTree, SplitAxis, TabGroup, TabId, TabState, TabTarget};
pub use view::{Dock, DockEvent, DraggedTab, DropZone};

use gpui_kit::App;

use crate::keys;

#[allow(unused_imports)]
use crate::gpui; // The action derive spells its paths `gpui::…`.

gpui_kit::actions!(
    dock,
    [
        /// Split the active group to the right, duplicating its tab.
        SplitRight,
        /// Split the active group downwards, duplicating its tab.
        SplitDown,
        /// Close the active tab.
        CloseTab,
        /// Close the active group and its tabs.
        CloseGroup,
    ]
);

#[cfg(target_os = "macos")]
const KEYS: [&str; 4] = ["cmd-\\", "cmd-alt-\\", "cmd-w", "cmd-alt-w"];
#[cfg(not(target_os = "macos"))]
const KEYS: [&str; 4] = ["ctrl-\\", "ctrl-alt-\\", "ctrl-w", "ctrl-alt-w"];

pub(crate) fn init(cx: &mut App) {
    cx.default_global::<TabRegistry>();
    let context = Some("Dock");
    cx.bind_keys([
        keys::binding(KEYS[0], SplitRight, context),
        keys::binding(KEYS[1], SplitDown, context),
        keys::binding(KEYS[2], CloseTab, context),
        keys::binding(KEYS[3], CloseGroup, context),
    ]);
}
