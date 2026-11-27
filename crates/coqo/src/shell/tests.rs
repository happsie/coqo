// SPDX-License-Identifier: Apache-2.0

use std::{rc::Rc, time::Duration};

use coqo_ui::{
    Appearance, Palette, Tokens,
    dock::{
        AnyTab, Dock, Edge, MissingTab, NodeId, PaneNode, SplitAxis, TabId, TabItem, TabTarget,
        register_tab_kind,
        store::{LAYOUT_VERSION, LayoutStore, MemoryLayoutStore, SidebarLayout, WorkbenchLayout},
    },
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Context, ElementId, Entity,
        InteractiveElement as _, IntoElement, Keystroke, ParentElement as _, Pixels, Point, Render,
        SharedString, StatefulInteractiveElement as _, Styled as _, Window, WindowBounds,
        WindowOptions, div, point, px, size,
    },
    serde_json::{Value, json},
    slots::{SidebarPane, SidebarPanes, Slot, Slots},
    test::{TestAppContext, TestWindowExt as _},
    widgets::TestSupportExt as _,
};

use super::{DEFAULT_WORKSPACE, Shell};
use crate::welcome;

/// A second tab kind, so tests mix kinds. Keyed by name.
struct NoteTab {
    name: String,
}

impl TabItem for NoteTab {
    fn kind(&self) -> SharedString {
        "note".into()
    }
    fn title(&self, _: &App) -> SharedString {
        self.name.clone().into()
    }
    fn key(&self, _: &App) -> Option<SharedString> {
        Some(self.name.clone().into())
    }
    fn save(&self, _: &App) -> Value {
        json!({ "name": self.name })
    }
}

impl Render for NoteTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.name.clone())
    }
}

fn note(name: &str, cx: &mut App) -> AnyTab {
    let name = name.to_owned();
    AnyTab::new(cx.new(|_| NoteTab { name }))
}

struct Harness {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
}

fn open_with(store: MemoryLayoutStore, appearance: Appearance) -> Harness {
    let cx = TestAppContext::single();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(appearance, cx);
        super::init(cx);
        register_tab_kind(cx, "note", |state, _, cx| {
            let name = state.get("name")?.as_str()?.to_owned();
            Some(AnyTab::new(cx.new(|_| NoteTab { name })))
        });
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1440.), px(900.)),
            })),
            ..Default::default()
        };
        coqo_ui::open_window(options, cx, |window, cx| {
            cx.new(|cx| Shell::new(Rc::new(store), DEFAULT_WORKSPACE, window, cx))
        })
        .expect("open shell window")
    });
    let shell = shell;
    Harness { cx, window, shell }
}

fn open(appearance: Appearance) -> Harness {
    open_with(MemoryLayoutStore::default(), appearance)
}

impl Harness {
    /// Render a frame, then run `f` against the window.
    fn frame<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx
            .update_window(self.window, |_, window, cx| {
                window.render_frame(cx);
                f(window, cx)
            })
            .unwrap()
    }

    fn dock<R>(&mut self, f: impl FnOnce(&mut Dock, &mut Window, &mut Context<Dock>) -> R) -> R {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx
            .update_window(self.window, |_, window, cx| {
                dock.update(cx, |dock, cx| f(dock, window, cx))
            })
            .unwrap()
    }

    fn groups(&mut self) -> Vec<NodeId> {
        self.dock(|d, _, _| d.tree().groups().iter().map(|g| g.id).collect())
    }

    fn bounds(&mut self, id: impl Into<ElementId>) -> Bounds<Pixels> {
        let id = id.into();
        self.frame(|window, _| window.find(id).bounds())
    }

    fn group_bounds(&mut self, group: NodeId) -> Bounds<Pixels> {
        self.bounds(("group", group.0))
    }

    fn tab_kinds(&mut self) -> Vec<Vec<String>> {
        self.dock(|d, _, _| {
            d.tree()
                .groups()
                .iter()
                .map(|g| g.tabs.iter().map(|t| t.kind.clone()).collect())
                .collect()
        })
    }

    fn drag(&mut self, from: Point<Pixels>, to: Point<Pixels>) {
        self.frame(|window, cx| window.drag(from, to, cx));
    }

    fn press(&mut self, keys: &'static str) {
        self.frame(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
        });
    }

    fn layout(&mut self) -> WorkbenchLayout {
        self.cx.read(|cx| self.shell.read(cx).layout(cx))
    }

    fn save_now(&mut self) {
        self.cx
            .update(|cx| self.shell.update(cx, |s, cx| s.save_now(cx)));
    }
}

#[cfg(target_os = "macos")]
const SPLIT_RIGHT: &str = "cmd-\\";
#[cfg(not(target_os = "macos"))]
const SPLIT_RIGHT: &str = "ctrl-\\";
#[cfg(target_os = "macos")]
const SPLIT_DOWN: &str = "cmd-alt-\\";
#[cfg(not(target_os = "macos"))]
const SPLIT_DOWN: &str = "ctrl-alt-\\";
#[cfg(target_os = "macos")]
const CLOSE_TAB: &str = "cmd-w";
#[cfg(not(target_os = "macos"))]
const CLOSE_TAB: &str = "ctrl-w";

#[test]
fn renders_board_one_chrome() {
    let mut h = open(Appearance::Dark);
    let group = h.groups()[0];
    h.frame(|window, _| {
        for id in [
            "top-bar",
            "command-search",
            "bolt",
            "daemon-status",
            "workspace-rail",
            "sidebar",
            "sidebar-files",
            "sidebar-files-pane",
            "main",
            "empty-state",
            "status-bar",
        ] {
            let el = window.find(id);
            assert!(el.visible(), "{id} is not visible");
            assert!(el.bounds().size.width > px(0.), "{id} has no width");
        }
        assert!(window.find(("tab-strip", group.0)).visible());
        assert!(window.find(("split", group.0)).visible());

        let top = window.find("top-bar").bounds();
        let rail = window.find("workspace-rail").bounds();
        let sidebar = window.find("sidebar").bounds();
        let main = window.find("main").bounds();
        let panel = window.find(("group", group.0)).bounds();
        let status = window.find("status-bar").bounds();

        // Rail, sidebar and main sit left to right, 10 px apart.
        assert_eq!(sidebar.left() - rail.right(), px(10.));
        assert_eq!(main.left() - sidebar.right(), px(10.));
        // The top bar is above the panels and the status bar below them.
        assert_eq!(rail.top() - top.bottom(), px(10.));
        assert_eq!(status.top() - main.bottom(), px(10.));
        // 10 px window padding, fixed rail and sidebar widths, main takes the rest.
        assert_eq!(rail.left(), px(10.));
        assert_eq!(rail.size.width, px(52.));
        assert_eq!(sidebar.size.width, px(250.));
        assert_eq!(main.right(), px(1430.));
        assert!(main.size.width > px(1000.));
        // One group fills the tab area as one floating panel.
        assert_eq!(panel, main);
    });
}

#[test]
fn explicit_appearance_selects_its_palette() {
    let h = open(Appearance::Light);
    h.cx.update(|cx| {
        let tokens = cx.global::<Tokens>();
        assert_eq!(tokens.choice, Appearance::Light);
        assert_eq!(tokens.palette, Palette::LIGHT);
    });
    let h = open(Appearance::Dark);
    h.cx.update(|cx| assert_eq!(cx.global::<Tokens>().palette, Palette::DARK));
}

#[test]
fn splits_right_and_down_into_panels_ten_px_apart() {
    let mut h = open(Appearance::Dark);
    h.dock(|d, window, cx| d.open_tab(welcome::new(cx), window, cx));
    let main = h.bounds("main");

    h.press(SPLIT_RIGHT);
    let groups = h.groups();
    assert_eq!(groups.len(), 2);
    let (left, right) = (h.group_bounds(groups[0]), h.group_bounds(groups[1]));
    assert_eq!(right.left() - left.right(), px(10.));
    assert_eq!(left.left(), main.left());
    assert_eq!(right.right(), main.right());
    assert!((left.size.width - right.size.width).abs() <= px(1.));
    // The split duplicates the tab, and the new group is active.
    assert_eq!(h.tab_kinds(), [["welcome"], ["welcome"]]);
    assert_eq!(h.dock(|d, _, _| d.active_group()), groups[1]);

    h.press(SPLIT_DOWN);
    let groups = h.groups();
    assert_eq!(groups.len(), 3);
    let (top, bottom) = (h.group_bounds(groups[1]), h.group_bounds(groups[2]));
    assert_eq!(bottom.top() - top.bottom(), px(10.));
    assert_eq!(top.left(), bottom.left());
    let shape = h.dock(|d, _, _| match d.tree().root() {
        PaneNode::Split { axis, children, .. } => (
            *axis,
            matches!(
                children[1],
                PaneNode::Split {
                    axis: SplitAxis::Column,
                    ..
                }
            ),
        ),
        PaneNode::Group(_) => panic!("expected a split"),
    });
    assert_eq!(shape, (SplitAxis::Row, true));

    // Closing the bottom tab collapses the column back into the row.
    h.press(CLOSE_TAB);
    assert_eq!(h.groups().len(), 2);
}

#[test]
fn shift_clicking_the_split_button_splits_down() {
    let mut h = open(Appearance::Dark);
    let group = h.groups()[0];
    // An empty group splits off a Welcome tab.
    h.frame(|window, cx| {
        window.click_with_modifiers(("split", group.0), coqo_ui::gpui::Modifiers::shift(), cx)
    });
    let groups = h.groups();
    assert_eq!(groups.len(), 1, "the empty group gives way to the new one");
    assert_eq!(h.tab_kinds(), [["welcome"]]);
    h.frame(|window, cx| window.click(("split", groups[0].0), cx));
    let groups = h.groups();
    assert_eq!(groups.len(), 2);
    let (a, b) = (h.group_bounds(groups[0]), h.group_bounds(groups[1]));
    assert_eq!(b.left() - a.right(), px(10.));
}

#[test]
fn any_tab_kind_drags_onto_the_edge_of_any_group() {
    let mut h = open(Appearance::Dark);
    let note_tab = h.dock(|d, window, cx| {
        d.open_tab(welcome::new(cx), window, cx);
        d.open_tab(note("notes.md", cx), window, cx)
    });
    let group = h.groups()[0];

    // Drag the note tab to the right edge of its own group: a new group.
    let from = h.bounds(("tab", note_tab.0)).center();
    let content = h.bounds(("group-content", group.0));
    let to = point(content.right() - px(20.), content.center().y);
    h.drag(from, to);
    assert_eq!(h.tab_kinds(), [["welcome"], ["note"]]);
    let groups = h.groups();
    assert_eq!(
        h.group_bounds(groups[1]).left() - h.group_bounds(groups[0]).right(),
        px(10.)
    );

    // Drag the welcome tab onto the bottom edge of the note group.
    let welcome_tab = h.dock(|d, _, _| d.tree().groups()[0].tabs[0].id);
    let from = h.bounds(("tab", welcome_tab.0)).center();
    let content = h.bounds(("group-content", groups[1].0));
    let to = point(content.center().x, content.bottom() - px(20.));
    h.drag(from, to);
    // Its old group closed; the note group split into a column.
    assert_eq!(h.tab_kinds(), [["note"], ["welcome"]]);
    let groups = h.groups();
    let (top, bottom) = (h.group_bounds(groups[0]), h.group_bounds(groups[1]));
    assert_eq!(bottom.top() - top.bottom(), px(10.));

    // Drag the welcome tab back onto the note tab: one group again.
    let from = h.bounds(("tab", welcome_tab.0)).center();
    let to = h.bounds(("tab", note_tab.0)).center();
    h.drag(from, to);
    assert_eq!(h.tab_kinds(), [["welcome", "note"]]);
    // No drop indicator is left behind.
    assert!(h.frame(|window, _| window.try_find("drop-indicator").is_none()));
}

#[test]
fn dragging_into_the_middle_of_a_group_merges_into_it() {
    let mut h = open(Appearance::Dark);
    let a = h.dock(|d, window, cx| d.open_tab(note("a", cx), window, cx));
    h.dock(|d, window, cx| {
        let group = d.active_group();
        d.open_tab_at(
            note("b", cx),
            TabTarget::Split {
                group,
                edge: Edge::Right,
            },
            window,
            cx,
        );
    });
    let right = h.groups()[1];
    let from = h.bounds(("tab", a.0)).center();
    let to = h.bounds(("group-content", right.0)).center();
    h.drag(from, to);
    assert_eq!(h.tab_kinds(), [["note", "note"]]);
    let names = h.dock(|d, _, cx| {
        d.tree().groups()[0]
            .tabs
            .iter()
            .map(|t| d.tab(t.id).unwrap().title(cx).to_string())
            .collect::<Vec<_>>()
    });
    assert_eq!(names, ["b", "a"]);
}

#[test]
fn the_close_button_closes_the_tab_and_collapses_the_split() {
    let mut h = open(Appearance::Dark);
    h.dock(|d, window, cx| d.open_tab(welcome::new(cx), window, cx));
    h.press(SPLIT_RIGHT);
    let tab = h.dock(|d, _, _| d.active_tab().unwrap());
    h.frame(|window, cx| {
        window.hover(("tab", tab.0), cx);
        window.click(("tab-close", tab.0), cx);
    });
    assert_eq!(h.groups().len(), 1);
    let (group, main) = (h.groups()[0], h.bounds("main"));
    assert_eq!(h.group_bounds(group), main);
}

#[test]
fn dragging_the_gap_between_groups_resizes_the_split() {
    let mut h = open(Appearance::Dark);
    h.dock(|d, window, cx| d.open_tab(welcome::new(cx), window, cx));
    h.press(SPLIT_RIGHT);
    let split = h.dock(|d, _, _| d.tree().root().id());
    let groups = h.groups();
    let before = h.group_bounds(groups[0]).size.width;
    let handle = h.bounds(ElementId::Name(
        format!("split-handle-{}-0", split.0).into(),
    ));
    assert_eq!(handle.size.width, px(10.));
    let from = handle.center();
    h.drag(from, point(from.x + px(200.), from.y));
    let after = h.group_bounds(groups[0]).size.width;
    assert!(
        (after - before - px(200.)).abs() <= px(1.),
        "{before:?} -> {after:?}"
    );
    // The gap stays 10 px.
    assert_eq!(
        h.group_bounds(groups[1]).left() - h.group_bounds(groups[0]).right(),
        px(10.)
    );
}

#[test]
fn dragging_the_sidebar_edge_resizes_it_within_limits() {
    let mut h = open(Appearance::Dark);
    let from = h.bounds("sidebar-handle").center();
    h.drag(from, point(from.x + px(100.), from.y));
    assert_eq!(h.bounds("sidebar").size.width, px(350.));
    let from = h.bounds("sidebar-handle").center();
    h.drag(from, point(px(20.), from.y));
    assert_eq!(h.bounds("sidebar").size.width, px(180.));
}

#[test]
fn layout_restores_after_restart() {
    let store = MemoryLayoutStore::default();
    let mut first = open_with(store.clone(), Appearance::Dark);
    first.dock(|d, window, cx| {
        d.open_tab(welcome::new(cx), window, cx);
        let group = d.active_group();
        d.open_tab_at(
            note("plan.md", cx),
            TabTarget::Split {
                group,
                edge: Edge::Right,
            },
            window,
            cx,
        );
        d.open_tab(note("todo.md", cx), window, cx);
    });
    let from = first.bounds("sidebar-handle").center();
    first.drag(from, point(from.x + px(40.), from.y));
    first.frame(|window, cx| window.click("sidebar-git", cx));
    first.save_now();
    let saved = first.layout();
    assert_eq!(
        store.load(DEFAULT_WORKSPACE).unwrap().as_ref(),
        Some(&saved)
    );
    assert_eq!(saved.sidebar.width, 290.);
    assert_eq!(saved.sidebar.pane.as_deref(), Some("git"));

    // A fresh UI against the same store comes back the same.
    let mut second = open_with(store, Appearance::Dark);
    assert_eq!(second.layout(), saved);
    assert_eq!(second.tab_kinds(), [vec!["welcome"], vec!["note", "note"]]);
    assert_eq!(second.bounds("sidebar").size.width, px(290.));
    assert!(second.frame(|window, _| window.find("sidebar-git-pane").visible()));
    let groups = second.groups();
    assert_eq!(
        second.group_bounds(groups[1]).left() - second.group_bounds(groups[0]).right(),
        px(10.)
    );
}

#[test]
fn changes_are_saved_without_an_explicit_save() {
    let store = MemoryLayoutStore::default();
    let mut h = open_with(store.clone(), Appearance::Dark);
    h.dock(|d, window, cx| d.open_tab(welcome::new(cx), window, cx));
    assert_eq!(store.load(DEFAULT_WORKSPACE).unwrap(), None);
    h.cx.executor().advance_clock(Duration::from_secs(1));
    h.cx.run_until_parked();
    let saved = store.load(DEFAULT_WORKSPACE).unwrap().expect("saved");
    assert_eq!(saved, h.layout());
}

#[test]
fn unknown_tab_kinds_survive_a_round_trip() {
    let store = MemoryLayoutStore::default();
    let dock: PaneNode = coqo_ui::serde_json::from_value(json!({
        "type": "group", "id": 1,
        "tabs": [
            { "id": 2, "kind": "from-a-removed-mod", "state": { "x": 1 } },
            { "id": 3, "kind": "note", "state": { "bad": true } },
            { "id": 4, "kind": "note", "state": { "name": "kept" } }
        ]
    }))
    .unwrap();
    let layout = WorkbenchLayout {
        version: LAYOUT_VERSION,
        sidebar: SidebarLayout {
            width: 250.,
            pane: None,
        },
        dock,
    };
    store.save(DEFAULT_WORKSPACE, &layout).unwrap();
    let mut h = open_with(store, Appearance::Dark);
    // The unknown kind is a placeholder; the note its builder declined is gone.
    assert_eq!(h.tab_kinds(), [["from-a-removed-mod", "note"]]);
    let missing = h.dock(|d, _, _| {
        let id = d.tree().groups()[0].tabs[0].id;
        d.tab(id).unwrap().downcast::<MissingTab>().is_some()
    });
    assert!(missing);
    let PaneNode::Group(group) = h.layout().dock else {
        panic!("expected a group")
    };
    assert_eq!(group.tabs[0].state, json!({ "x": 1 }));
}

#[test]
fn opening_a_tab_with_the_same_key_shows_the_open_one() {
    let mut h = open(Appearance::Dark);
    let a = h.dock(|d, window, cx| d.open_tab(note("a", cx), window, cx));
    h.dock(|d, window, cx| d.open_tab(note("b", cx), window, cx));
    let again = h.dock(|d, window, cx| d.open_tab(note("a", cx), window, cx));
    assert_eq!(again, a);
    assert_eq!(h.dock(|d, _, _| d.active_tab()), Some(a));
    assert_eq!(h.tab_kinds(), [["note", "note"]]);
    assert!(h.dock(|d, _, cx| d.find_tab("note", "b", cx).is_some()));
}

#[test]
fn mods_fill_the_status_bar_and_top_bar_slots() {
    let mut h = open(Appearance::Dark);
    h.cx.update(|cx| {
        Slots::register(cx, Slot::StatusBarEnd, "test.memory", 50, |_, _| {
            div()
                .id("test-memory")
                .test_support()
                .child("1.4 GB")
                .into_any_element()
        });
        Slots::register(cx, Slot::TopBarStart, "test.branch", 0, |_, _| {
            div()
                .id("test-branch")
                .test_support()
                .child("main")
                .into_any_element()
        });
    });
    h.frame(|window, _| {
        let memory = window.find("test-memory").bounds();
        let status = window.find("status-bar").bounds();
        assert!(status.contains(&memory.center()));
        let branch = window.find("test-branch").bounds();
        let top = window.find("top-bar").bounds();
        assert!(top.contains(&branch.center()));
        assert!(branch.right() < window.find("command-search").bounds().left());
    });
}

/// A sidebar pane that opens a note tab, as the file tree will open files.
struct OpenerPane {
    dock: coqo_ui::gpui::WeakEntity<Dock>,
}

impl Render for OpenerPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dock = self.dock.clone();
        div()
            .id("opener")
            .test_support()
            .size(px(100.))
            .on_click(cx.listener(move |_, _, window, cx| {
                let tab = note("opened.md", cx);
                dock.update(cx, |dock, cx| dock.open_tab(tab, window, cx))
                    .ok();
            }))
    }
}

#[test]
fn sidebar_panes_are_registered_and_open_tabs() {
    let mut h = open(Appearance::Dark);
    h.cx.update(|cx| {
        SidebarPanes::register(
            cx,
            SidebarPane::new("files", "Files", 0, |dock, _, cx| {
                cx.new(|_| OpenerPane { dock }).into()
            }),
        );
    });
    h.frame(|window, cx| window.click("opener", cx));
    assert_eq!(h.tab_kinds(), [["note"]]);
    let tab = h.dock(|d, _, _| d.active_tab()).unwrap_or(TabId(0));
    h.frame(|window, _| assert!(window.find(("tab", tab.0)).visible()));

    // Switching panes swaps the content.
    h.frame(|window, cx| window.click("sidebar-git", cx));
    h.frame(|window, _| {
        assert!(window.find("sidebar-git-pane").visible());
        assert!(window.try_find("opener").is_none());
    });
}
