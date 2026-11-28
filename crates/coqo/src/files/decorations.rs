// SPDX-License-Identifier: Apache-2.0

//! Row decorations (issue #163): what other features say about the tree's
//! rows, without the tree knowing who they are.
//!
//! A provider ([`register_provider`]) is attached to each window's
//! [`RowDecorations`] when that window's tree first needs them, and fills
//! it per root: a right-aligned letter on a path's row ([`Badge`]) and a
//! tint on the name of a folder with something decorated inside. The git
//! status letters (`M`, `A`, `D`, `?`, …) are one such provider
//! (`git::decorations`); the tree only draws what it is given.
//!
//! Colours are palette roles ([`Tone`]), so decorations follow the
//! appearance.

use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    rc::Rc,
};

use coqo_protocol::methods::workspace::RootId;
use coqo_ui::{
    Palette, Rgb,
    gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, Global, WeakEntity, Window},
};

/// A palette role a decoration is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    Sand,
    Green,
    Rose,
    Lavender,
    Blue,
    Subtle,
}

impl Tone {
    pub fn rgb(self, p: &Palette) -> Rgb {
        match self {
            Tone::Sand => p.sand,
            Tone::Green => p.green,
            Tone::Rose => p.rose,
            Tone::Lavender => p.lavender,
            Tone::Blue => p.blue,
            Tone::Subtle => p.text_subtle,
        }
    }
}

/// A letter at the right end of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Badge {
    pub letter: char,
    pub tone: Tone,
}

/// What a provider says about one root's paths (root-relative,
/// `/`-separated; `""` is the root itself).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootDecorations {
    /// Letters, by path.
    pub badges: HashMap<String, Badge>,
    /// Tints of folder names, by folder.
    pub tints: HashMap<String, Tone>,
}

/// What one row shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowDecoration {
    pub badge: Option<Badge>,
    pub tint: Option<Tone>,
}

/// Attaches a provider to a window's decorations; what it returns is kept
/// alive with them (its models and subscriptions).
pub type Attach = Rc<dyn Fn(&Entity<RowDecorations>, &mut Window, &mut App) -> Box<dyn Any>>;

/// The registered providers, by id.
#[derive(Default)]
struct Providers(BTreeMap<&'static str, Attach>);

impl Global for Providers {}

/// Register a decoration provider. `attach` runs once per window, the
/// first time a tree there asks for its decorations, and fills them with
/// [`RowDecorations::set`].
pub fn register_provider(
    cx: &mut App,
    id: &'static str,
    attach: impl Fn(&Entity<RowDecorations>, &mut Window, &mut App) -> Box<dyn Any> + 'static,
) {
    cx.default_global::<Providers>()
        .0
        .insert(id, Rc::new(attach));
}

/// The decorations of one window's tree rows, from every provider.
#[derive(Default)]
pub struct RowDecorations {
    /// By provider id (the first id wins a row), then root.
    by_provider: BTreeMap<&'static str, HashMap<RootId, RootDecorations>>,
    _attached: Vec<Box<dyn Any>>,
}

impl std::fmt::Debug for RowDecorations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowDecorations")
            .field("providers", &self.by_provider.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The decorations, by window.
#[derive(Default)]
struct Windows(Vec<(AnyWindowHandle, WeakEntity<RowDecorations>)>);

impl Global for Windows {}

impl RowDecorations {
    /// `window`'s decorations, with every provider attached on first use.
    /// The trees that hold them keep them alive.
    pub fn for_window(window: &mut Window, cx: &mut App) -> Entity<Self> {
        let handle = window.window_handle();
        let windows = cx.default_global::<Windows>();
        windows.0.retain(|(_, d)| d.upgrade().is_some());
        if let Some(existing) = windows
            .0
            .iter()
            .find(|(h, _)| *h == handle)
            .and_then(|(_, d)| d.upgrade())
        {
            return existing;
        }
        let decorations = cx.new(|_| Self::default());
        cx.default_global::<Windows>()
            .0
            .push((handle, decorations.downgrade()));
        let providers: Vec<Attach> = cx
            .try_global::<Providers>()
            .map(|p| p.0.values().cloned().collect())
            .unwrap_or_default();
        let attached: Vec<Box<dyn Any>> = providers
            .iter()
            .map(|attach| attach(&decorations, window, cx))
            .collect();
        decorations.update(cx, |d, _| d._attached = attached);
        decorations
    }

    /// Replace everything provider `id` says, for every root.
    pub fn set(
        &mut self,
        id: &'static str,
        roots: HashMap<RootId, RootDecorations>,
        cx: &mut Context<Self>,
    ) {
        if self.by_provider.get(id) != Some(&roots) {
            self.by_provider.insert(id, roots);
            cx.notify();
        }
    }

    /// What the row of `path` under `root` shows.
    pub fn row(&self, root: RootId, path: &str) -> RowDecoration {
        let mut row = RowDecoration::default();
        for roots in self.by_provider.values() {
            let Some(d) = roots.get(&root) else {
                continue;
            };
            row.badge = row.badge.or_else(|| d.badges.get(path).copied());
            row.tint = row.tint.or_else(|| d.tints.get(path).copied());
        }
        row
    }
}
