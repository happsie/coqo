// SPDX-License-Identifier: Apache-2.0

//! The dock's layout as plain data: a tree of splits whose leaves are tab groups.
//!
//! Nothing here touches GPUI, so every edit (open, move, split, close, resize) is
//! tested as an ordinary value. [`Dock`](super::Dock) owns one [`PaneTree`] and
//! projects it into elements; persistence serializes its [`PaneNode`] root.
//!
//! Every edit leaves the tree normalized: no empty groups unless the tree has only
//! one group, no splits with fewer than two children, no split directly inside a
//! split of the same axis, and split sizes that are positive and sum to 1.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifies a group or a split. Stable across edits and restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u64);

/// Identifies an open tab. Stable across moves and restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TabId(pub u64);

/// The direction a split lays out its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitAxis {
    /// Children side by side, left to right ("split right").
    Row,
    /// Children stacked, top to bottom ("split down").
    Column,
}

/// A side of a group, where a new group can be split off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn axis(self) -> SplitAxis {
        match self {
            Edge::Left | Edge::Right => SplitAxis::Row,
            Edge::Top | Edge::Bottom => SplitAxis::Column,
        }
    }

    /// Whether the new group goes after the existing one.
    fn after(self) -> bool {
        matches!(self, Edge::Right | Edge::Bottom)
    }
}

/// A tab as the layout records it: its id, its kind (which tab builder restores
/// it) and the tab's own saved state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabState {
    pub id: TabId,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub state: Value,
}

/// A tab group: a tab strip and the content of its active tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabGroup {
    pub id: NodeId,
    #[serde(default)]
    pub tabs: Vec<TabState>,
    /// Index of the displayed tab.
    #[serde(default)]
    pub active: usize,
}

impl TabGroup {
    pub fn new(id: NodeId) -> Self {
        Self {
            id,
            tabs: Vec::new(),
            active: 0,
        }
    }

    /// The displayed tab, if the group has any.
    pub fn active_tab(&self) -> Option<TabId> {
        self.tabs.get(self.active).map(|t| t.id)
    }

    pub fn position(&self, tab: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == tab)
    }
}

/// A node of the layout tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneNode {
    Split {
        id: NodeId,
        axis: SplitAxis,
        children: Vec<PaneNode>,
        /// Each child's share of the split, summing to 1.
        sizes: Vec<f32>,
    },
    Group(TabGroup),
}

impl PaneNode {
    pub fn id(&self) -> NodeId {
        match self {
            PaneNode::Split { id, .. } => *id,
            PaneNode::Group(g) => g.id,
        }
    }

    fn for_each_group<'a>(&'a self, f: &mut impl FnMut(&'a TabGroup)) {
        match self {
            PaneNode::Split { children, .. } => children.iter().for_each(|c| c.for_each_group(f)),
            PaneNode::Group(g) => f(g),
        }
    }

    fn for_each_group_mut(&mut self, f: &mut impl FnMut(&mut TabGroup)) {
        match self {
            PaneNode::Split { children, .. } => {
                children.iter_mut().for_each(|c| c.for_each_group_mut(f))
            }
            PaneNode::Group(g) => f(g),
        }
    }

    fn find_mut(&mut self, id: NodeId) -> Option<&mut PaneNode> {
        if self.id() == id {
            return Some(self);
        }
        match self {
            PaneNode::Split { children, .. } => children.iter_mut().find_map(|c| c.find_mut(id)),
            PaneNode::Group(_) => None,
        }
    }

    fn max_id(&self) -> u64 {
        match self {
            PaneNode::Split { id, children, .. } => {
                children.iter().map(PaneNode::max_id).fold(id.0, u64::max)
            }
            PaneNode::Group(g) => g.tabs.iter().map(|t| t.id.0).fold(g.id.0, u64::max),
        }
    }
}

/// Where a tab goes when it is opened or moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabTarget {
    /// Into a group: at `index`, or after its last tab when `None`.
    Group { group: NodeId, index: Option<usize> },
    /// Into the group holding `tab`, just before it.
    Before(TabId),
    /// Into a new group split off `group` on `edge`.
    Split { group: NodeId, edge: Edge },
}

/// The smallest share a split child can be resized to.
pub const MIN_SHARE: f32 = 0.05;

/// The whole dock layout plus the id counter for new nodes and tabs.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneTree {
    root: PaneNode,
    next_id: u64,
}

impl Default for PaneTree {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneTree {
    /// One empty group.
    pub fn new() -> Self {
        Self {
            root: PaneNode::Group(TabGroup::new(NodeId(1))),
            next_id: 2,
        }
    }

    /// Adopt a restored root, repairing anything a hand-edited or older file got
    /// wrong: duplicate ids are renumbered and the tree is normalized.
    pub fn from_root(mut root: PaneNode) -> Self {
        let mut next_id = root.max_id() + 1;
        let mut seen = std::collections::HashSet::new();
        fn dedupe(node: &mut PaneNode, seen: &mut std::collections::HashSet<u64>, next: &mut u64) {
            let mut fresh = |id: &mut u64| {
                if !seen.insert(*id) {
                    *id = *next;
                    *next += 1;
                    seen.insert(*id);
                }
            };
            match node {
                PaneNode::Split { id, children, .. } => {
                    fresh(&mut id.0);
                    children.iter_mut().for_each(|c| dedupe(c, seen, next));
                }
                PaneNode::Group(g) => {
                    fresh(&mut g.id.0);
                    g.tabs.iter_mut().for_each(|t| fresh(&mut t.id.0));
                }
            }
        }
        dedupe(&mut root, &mut seen, &mut next_id);
        let mut tree = Self { root, next_id };
        tree.normalize();
        tree
    }

    pub fn root(&self) -> &PaneNode {
        &self.root
    }

    /// A copy of the root with every tab passed through `update`, for saving
    /// with fresh tab state.
    pub fn snapshot_with(&self, mut update: impl FnMut(&mut TabState)) -> PaneNode {
        let mut root = self.root.clone();
        root.for_each_group_mut(&mut |g| g.tabs.iter_mut().for_each(&mut update));
        root
    }

    /// Drop every tab `keep` rejects, normalizing afterwards.
    pub fn retain_tabs(&mut self, mut keep: impl FnMut(&TabState) -> bool) {
        self.root
            .for_each_group_mut(&mut |g| g.tabs.retain(|t| keep(t)));
        self.normalize();
    }

    /// A fresh id, unique among every node and tab this tree has held.
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Every group, in reading order (depth first, left to right / top to bottom).
    pub fn groups(&self) -> Vec<&TabGroup> {
        let mut out = Vec::new();
        self.root.for_each_group(&mut |g| out.push(g));
        out
    }

    pub fn group(&self, id: NodeId) -> Option<&TabGroup> {
        self.groups().into_iter().find(|g| g.id == id)
    }

    fn group_mut(&mut self, id: NodeId) -> Option<&mut TabGroup> {
        match self.root.find_mut(id)? {
            PaneNode::Group(g) => Some(g),
            PaneNode::Split { .. } => None,
        }
    }

    /// The group holding `tab`.
    pub fn group_of(&self, tab: TabId) -> Option<NodeId> {
        self.groups()
            .into_iter()
            .find(|g| g.position(tab).is_some())
            .map(|g| g.id)
    }

    pub fn tabs(&self) -> Vec<&TabState> {
        self.groups().into_iter().flat_map(|g| &g.tabs).collect()
    }

    /// Display the tab in its group. Returns false if there is no such tab.
    pub fn activate(&mut self, tab: TabId) -> bool {
        let Some(group) = self.group_of(tab) else {
            return false;
        };
        let g = self.group_mut(group).expect("group_of returned it");
        g.active = g.position(tab).expect("group_of found it");
        true
    }

    /// Put `tab` at `target` and display it. Returns the group it landed in, or
    /// `None` (leaving the tree untouched) if the target doesn't exist.
    pub fn insert(&mut self, tab: TabState, target: TabTarget) -> Option<NodeId> {
        let group = match target {
            TabTarget::Group { group, index } => {
                let g = self.group_mut(group)?;
                let ix = index.unwrap_or(g.tabs.len()).min(g.tabs.len());
                g.tabs.insert(ix, tab);
                g.active = ix;
                group
            }
            TabTarget::Before(before) => {
                let group = self.group_of(before)?;
                let g = self.group_mut(group).expect("group_of returned it");
                let ix = g.position(before).expect("group_of found it");
                g.tabs.insert(ix, tab);
                g.active = ix;
                group
            }
            TabTarget::Split { group, edge } => {
                self.group(group)?;
                let new_id = NodeId(self.alloc_id());
                let split_id = NodeId(self.alloc_id());
                let mut new_group = TabGroup::new(new_id);
                new_group.tabs.push(tab);
                self.split(group, edge, new_group, split_id);
                new_id
            }
        };
        self.normalize();
        Some(group)
    }

    /// Take a tab out of the layout. An emptied group closes, collapsing its split.
    pub fn remove(&mut self, tab: TabId) -> Option<TabState> {
        let removed = self.take(tab)?;
        self.normalize();
        Some(removed)
    }

    fn take(&mut self, tab: TabId) -> Option<TabState> {
        let group = self.group_of(tab)?;
        let g = self.group_mut(group).expect("group_of returned it");
        let ix = g.position(tab).expect("group_of found it");
        let removed = g.tabs.remove(ix);
        if g.active > ix || g.active >= g.tabs.len() {
            g.active = g.active.saturating_sub(1);
        }
        Some(removed)
    }

    /// Move an open tab to `target`. Returns the group it landed in. Dropping the
    /// only tab of a group onto that group's own edge changes nothing.
    pub fn move_tab(&mut self, tab: TabId, target: TabTarget) -> Option<NodeId> {
        let from = self.group_of(tab)?;
        let valid = match target {
            TabTarget::Group { group, .. } | TabTarget::Split { group, .. } => {
                self.group(group).is_some()
            }
            TabTarget::Before(other) => other != tab && self.group_of(other).is_some(),
        };
        if !valid {
            return None;
        }
        if let TabTarget::Split { group, .. } = target
            && group == from
            && self.group(from).is_some_and(|g| g.tabs.len() == 1)
        {
            return Some(from);
        }
        let state = self.take(tab).expect("group_of found it");
        // Empty groups stay until `insert` normalizes, so a split target that was
        // just emptied still exists to split against.
        self.insert(state, target)
    }

    /// Close a group and return its tabs.
    pub fn close_group(&mut self, group: NodeId) -> Vec<TabState> {
        let Some(g) = self.group_mut(group) else {
            return Vec::new();
        };
        let tabs = std::mem::take(&mut g.tabs);
        g.active = 0;
        self.normalize();
        tabs
    }

    /// Set the boundary after child `handle` of `split` so that child takes
    /// `share` of the pair it forms with the next child. Returns false if there is
    /// no such split or handle.
    pub fn resize(&mut self, split: NodeId, handle: usize, share: f32) -> bool {
        let Some(PaneNode::Split { sizes, .. }) = self.root.find_mut(split) else {
            return false;
        };
        if handle + 1 >= sizes.len() {
            return false;
        }
        let pair = sizes[handle] + sizes[handle + 1];
        let first = share.clamp(MIN_SHARE.min(pair / 2.), (pair - MIN_SHARE).max(pair / 2.));
        sizes[handle] = first;
        sizes[handle + 1] = pair - first;
        true
    }

    /// The sizes of a split, for the view's resize math.
    pub fn split_sizes(&self, split: NodeId) -> Option<&[f32]> {
        fn find(node: &PaneNode, id: NodeId) -> Option<&[f32]> {
            match node {
                PaneNode::Split {
                    id: sid,
                    sizes,
                    children,
                    ..
                } => {
                    if *sid == id {
                        Some(sizes)
                    } else {
                        children.iter().find_map(|c| find(c, id))
                    }
                }
                PaneNode::Group(_) => None,
            }
        }
        find(&self.root, split)
    }

    /// Place `new_group` beside `target`. Joins the parent split when its axis
    /// matches, so repeated "split right" gives one row rather than nesting.
    fn split(&mut self, target: NodeId, edge: Edge, new_group: TabGroup, split_id: NodeId) {
        let axis = edge.axis();
        let new_node = PaneNode::Group(new_group);
        if let Some(PaneNode::Split {
            children, sizes, ..
        }) = self
            .parent_of(target)
            .and_then(|p| self.root.find_mut(p))
            .filter(|p| matches!(p, PaneNode::Split { axis: a, .. } if *a == axis))
        {
            let ix = children
                .iter()
                .position(|c| c.id() == target)
                .expect("parent holds target");
            let half = sizes[ix] / 2.;
            sizes[ix] = half;
            let at = if edge.after() { ix + 1 } else { ix };
            children.insert(at, new_node);
            sizes.insert(at, half);
            return;
        }
        let node = self.root.find_mut(target).expect("target exists");
        let old = std::mem::replace(node, PaneNode::Group(TabGroup::new(NodeId(0))));
        let children = if edge.after() {
            vec![old, new_node]
        } else {
            vec![new_node, old]
        };
        *node = PaneNode::Split {
            id: split_id,
            axis,
            children,
            sizes: vec![0.5, 0.5],
        };
    }

    fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        fn find(node: &PaneNode, id: NodeId) -> Option<NodeId> {
            match node {
                PaneNode::Split {
                    id: sid, children, ..
                } => {
                    if children.iter().any(|c| c.id() == id) {
                        Some(*sid)
                    } else {
                        children.iter().find_map(|c| find(c, id))
                    }
                }
                PaneNode::Group(_) => None,
            }
        }
        find(&self.root, id)
    }

    /// Restore the invariants listed in the module docs.
    pub fn normalize(&mut self) {
        let first_group = self.groups().first().map(|g| g.id).unwrap_or(NodeId(1));
        let root = std::mem::replace(&mut self.root, PaneNode::Group(TabGroup::new(first_group)));
        self.root = prune(root).unwrap_or_else(|| PaneNode::Group(TabGroup::new(first_group)));
        self.root.for_each_group_mut(&mut |g| {
            g.active = g.active.min(g.tabs.len().saturating_sub(1));
        });
    }
}

/// Drop empty groups and degenerate splits, flatten same-axis nesting, fix sizes.
fn prune(node: PaneNode) -> Option<PaneNode> {
    match node {
        PaneNode::Group(g) => (!g.tabs.is_empty()).then_some(PaneNode::Group(g)),
        PaneNode::Split {
            id,
            axis,
            children,
            sizes,
        } => {
            let sizes = repair_sizes(sizes, children.len());
            let mut kept: Vec<(PaneNode, f32)> = Vec::new();
            for (child, size) in children.into_iter().zip(sizes) {
                match prune(child) {
                    Some(PaneNode::Split {
                        axis: inner,
                        children: grand,
                        sizes: grand_sizes,
                        ..
                    }) if inner == axis => {
                        kept.extend(
                            grand
                                .into_iter()
                                .zip(grand_sizes.into_iter().map(|s| s * size)),
                        );
                    }
                    Some(child) => kept.push((child, size)),
                    None => {}
                }
            }
            match kept.len() {
                0 => None,
                1 => kept.pop().map(|(c, _)| c),
                _ => {
                    let (children, sizes): (Vec<_>, Vec<_>) = kept.into_iter().unzip();
                    let total: f32 = sizes.iter().sum();
                    Some(PaneNode::Split {
                        id,
                        axis,
                        children,
                        sizes: sizes.into_iter().map(|s| s / total).collect(),
                    })
                }
            }
        }
    }
}

fn repair_sizes(sizes: Vec<f32>, len: usize) -> Vec<f32> {
    let ok = sizes.len() == len && sizes.iter().all(|s| s.is_finite() && *s > 0.);
    if ok {
        sizes
    } else {
        vec![1. / len.max(1) as f32; len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(tree: &mut PaneTree, kind: &str) -> TabState {
        TabState {
            id: TabId(tree.alloc_id()),
            kind: kind.into(),
            state: Value::Null,
        }
    }

    fn open(tree: &mut PaneTree, kind: &str, target: TabTarget) -> TabId {
        let t = tab(tree, kind);
        let id = t.id;
        tree.insert(t, target).expect("target exists");
        id
    }

    /// Open a tab in a new group split off `group`; returns the new group.
    fn split(tree: &mut PaneTree, kind: &str, group: NodeId, edge: Edge) -> NodeId {
        let t = tab(tree, kind);
        tree.insert(t, TabTarget::Split { group, edge })
            .expect("target exists")
    }

    fn root_group(tree: &PaneTree) -> NodeId {
        tree.groups()[0].id
    }

    fn shape(node: &PaneNode) -> String {
        match node {
            PaneNode::Group(g) => {
                let kinds: Vec<_> = g.tabs.iter().map(|t| t.kind.as_str()).collect();
                format!("[{}]", kinds.join(","))
            }
            PaneNode::Split { axis, children, .. } => {
                let inner: Vec<_> = children.iter().map(shape).collect();
                let tag = match axis {
                    SplitAxis::Row => "row",
                    SplitAxis::Column => "col",
                };
                format!("{tag}({})", inner.join(" "))
            }
        }
    }

    fn append(group: NodeId) -> TabTarget {
        TabTarget::Group { group, index: None }
    }

    #[test]
    fn opening_appends_and_activates() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        let a = open(&mut tree, "a", append(g));
        let b = open(&mut tree, "b", append(g));
        assert_eq!(shape(tree.root()), "[a,b]");
        assert_eq!(tree.group(g).unwrap().active_tab(), Some(b));
        assert!(tree.activate(a));
        assert_eq!(tree.group(g).unwrap().active_tab(), Some(a));
    }

    #[test]
    fn any_kind_splits_next_to_any_other() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        open(&mut tree, "file", append(g));
        let right = split(&mut tree, "chat", g, Edge::Right);
        split(&mut tree, "diff", right, Edge::Bottom);
        split(&mut tree, "trace", g, Edge::Left);
        assert_eq!(shape(tree.root()), "row([trace] [file] col([chat] [diff]))");
    }

    #[test]
    fn same_axis_splits_join_one_row_and_share_space() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        open(&mut tree, "a", append(g));
        let b = split(&mut tree, "b", g, Edge::Right);
        split(&mut tree, "c", b, Edge::Right);
        assert_eq!(shape(tree.root()), "row([a] [b] [c])");
        let PaneNode::Split { sizes, .. } = tree.root() else {
            panic!("expected a split")
        };
        assert_eq!(sizes, &[0.5, 0.25, 0.25]);
    }

    #[test]
    fn closing_the_last_tab_collapses_the_split() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        open(&mut tree, "a", append(g));
        let right = split(&mut tree, "b", g, Edge::Right);
        let lower = split(&mut tree, "c", right, Edge::Bottom);
        let c = tree.group(lower).unwrap().tabs[0].id;
        let b = tree.group(right).unwrap().tabs[0].id;
        tree.remove(c);
        assert_eq!(shape(tree.root()), "row([a] [b])");
        tree.remove(b);
        assert_eq!(shape(tree.root()), "[a]");
        assert_eq!(tree.root().id(), g);
    }

    #[test]
    fn the_last_group_stays_when_emptied() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        let a = open(&mut tree, "a", append(g));
        tree.remove(a);
        assert_eq!(shape(tree.root()), "[]");
        assert_eq!(tree.root().id(), g);
    }

    #[test]
    fn close_group_returns_its_tabs_and_collapses() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        open(&mut tree, "a", append(g));
        let right = split(&mut tree, "b", g, Edge::Right);
        open(&mut tree, "c", append(right));
        let closed = tree.close_group(right);
        assert_eq!(
            closed.iter().map(|t| t.kind.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
        assert_eq!(shape(tree.root()), "[a]");
    }

    #[test]
    fn moving_tabs_between_groups_and_onto_edges() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        let a = open(&mut tree, "a", append(g));
        let b = open(&mut tree, "b", append(g));
        let c = open(&mut tree, "c", append(g));

        // Onto the right edge of its own group: a new group to the right.
        let right = tree
            .move_tab(
                c,
                TabTarget::Split {
                    group: g,
                    edge: Edge::Right,
                },
            )
            .unwrap();
        assert_eq!(shape(tree.root()), "row([a,b] [c])");

        // Before a tab in another group.
        tree.move_tab(a, TabTarget::Before(c)).unwrap();
        assert_eq!(shape(tree.root()), "row([b] [a,c])");
        assert_eq!(tree.group(right).unwrap().active_tab(), Some(a));

        // The last tab of a group onto another group: the source group closes.
        tree.move_tab(b, append(right)).unwrap();
        assert_eq!(shape(tree.root()), "[a,c,b]");
    }

    #[test]
    fn moving_the_only_tab_onto_its_own_edge_is_a_no_op() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        let a = open(&mut tree, "a", append(g));
        let before = tree.clone();
        assert_eq!(
            tree.move_tab(
                a,
                TabTarget::Split {
                    group: g,
                    edge: Edge::Bottom
                }
            ),
            Some(g)
        );
        assert_eq!(tree, before);
    }

    #[test]
    fn moving_the_only_tab_of_a_group_beside_another_group() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        open(&mut tree, "a", append(g));
        let right = split(&mut tree, "b", g, Edge::Right);
        let b = tree.group(right).unwrap().tabs[0].id;
        tree.move_tab(
            b,
            TabTarget::Split {
                group: g,
                edge: Edge::Bottom,
            },
        )
        .unwrap();
        assert_eq!(shape(tree.root()), "col([a] [b])");
    }

    #[test]
    fn reordering_within_a_group() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        let a = open(&mut tree, "a", append(g));
        open(&mut tree, "b", append(g));
        let c = open(&mut tree, "c", append(g));
        tree.move_tab(c, TabTarget::Before(a)).unwrap();
        assert_eq!(shape(tree.root()), "[c,a,b]");
        assert_eq!(tree.move_tab(c, TabTarget::Before(c)), None);
    }

    #[test]
    fn invalid_targets_leave_the_tree_alone() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        let a = open(&mut tree, "a", append(g));
        let before = tree.clone();
        assert_eq!(tree.move_tab(a, append(NodeId(999))), None);
        assert_eq!(tree.move_tab(TabId(999), append(g)), None);
        assert_eq!(tree, before);
    }

    #[test]
    fn resizing_keeps_the_pair_total_and_a_minimum() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        open(&mut tree, "a", append(g));
        split(&mut tree, "b", g, Edge::Right);
        let split = tree.root().id();
        assert!(tree.resize(split, 0, 0.7));
        assert_eq!(tree.split_sizes(split).unwrap(), &[0.7, 0.3]);
        assert!(tree.resize(split, 0, 2.0));
        let sizes = tree.split_sizes(split).unwrap();
        assert!((sizes[1] - MIN_SHARE).abs() < 1e-6, "{sizes:?}");
        assert!(!tree.resize(split, 1, 0.5));
        assert!(!tree.resize(g, 0, 0.5));
    }

    #[test]
    fn active_index_follows_removal() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        let a = open(&mut tree, "a", append(g));
        let b = open(&mut tree, "b", append(g));
        let c = open(&mut tree, "c", append(g));
        tree.activate(c);
        tree.remove(a);
        assert_eq!(tree.group(g).unwrap().active_tab(), Some(c));
        tree.remove(c);
        assert_eq!(tree.group(g).unwrap().active_tab(), Some(b));
    }

    #[test]
    fn round_trips_through_json() {
        let mut tree = PaneTree::new();
        let g = root_group(&tree);
        let mut t = tab(&mut tree, "file");
        t.state = serde_json::json!({ "path": "src/main.rs" });
        tree.insert(t, append(g));
        split(&mut tree, "chat", g, Edge::Bottom);
        let json = serde_json::to_string(tree.root()).unwrap();
        let back = PaneTree::from_root(serde_json::from_str(&json).unwrap());
        assert_eq!(back.root(), tree.root());
        // New ids keep clear of every restored one.
        let mut back = back;
        assert!(back.alloc_id() > tree.root().max_id());
    }

    #[test]
    fn restoring_repairs_a_damaged_tree() {
        let json = r#"{
            "type": "split", "id": 5, "axis": "row", "sizes": [0.2],
            "children": [
                { "type": "group", "id": 1, "tabs": [{ "id": 1, "kind": "a" }], "active": 9 },
                { "type": "split", "id": 6, "axis": "row", "sizes": [1, 1], "children": [
                    { "type": "group", "id": 2, "tabs": [{ "id": 3, "kind": "b" }] },
                    { "type": "group", "id": 4, "tabs": [] }
                ]}
            ]
        }"#;
        let tree = PaneTree::from_root(serde_json::from_str(json).unwrap());
        assert_eq!(shape(tree.root()), "row([a] [b])");
        let PaneNode::Split { sizes, .. } = tree.root() else {
            panic!("expected a split")
        };
        assert_eq!(sizes, &[0.5, 0.5]);
        let ids: Vec<u64> = tree.tabs().iter().map(|t| t.id.0).collect();
        assert_ne!(
            ids[0], 1,
            "tab id 1 collides with group 1 and is renumbered"
        );
        assert_eq!(tree.groups()[0].active, 0);
    }
}
