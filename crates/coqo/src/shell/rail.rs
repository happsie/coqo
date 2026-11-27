// SPDX-License-Identifier: Apache-2.0

//! The workspace rail (design boards 1 and 6, PLAN §7.9).
//!
//! One 42 px button per known workspace, in the order they were added, with
//! the workspace's initials. The window's current workspace is lit in sage.
//! A badge in the top-right corner counts the workspace's agent sessions
//! that wait for the user (`sand`), or shows `!` when one failed (`rose`); a
//! dot in the bottom-right corner shows an agent working (`blue`, pulsing)
//! or done (`sage`). Background workspaces keep running in the daemon, so
//! their badges stay live ([`AgentActivity`]).
//!
//! - **Click** (or `Ctrl/Cmd+1..9`): switch the window to the workspace,
//!   saving this one's layout and restoring that one's, or focus the window
//!   that already shows it.
//! - **Right-click:** open in a new window, rename, reveal the folder, or
//!   remove it from the list (`workspace/remove`, which never touches its
//!   folders).
//! - **+** opens a folder as a new workspace.

use std::path::PathBuf;

use coqo_protocol::methods::workspace::{WorkspaceId, WorkspaceInfo};
use coqo_ui::{
    ActiveTokens as _, Appearance, Rgb,
    gpui::{
        Animation, AnimationExt as _, AnyElement, App, AppContext as _, BoxShadow, Context, Entity,
        FontWeight, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
        ParentElement as _, Render, SharedString, Stateful, StatefulInteractiveElement as _,
        Styled as _, Subscription, Window, deferred, div, hsla, linear_color_stop, linear_gradient,
        point, prelude::FluentBuilder as _, pulsating_between, px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{Icon, TestSupportExt as _, icon, kbd, popover},
};

use crate::{
    activity::{AgentActivity, WorkspaceActivity},
    welcome::shortcut,
    windows,
    workspace::{self, KnownWorkspaces, Workspaces, initials, prompt_open_folder_in},
};

/// What a rail button's top-right badge says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// This many sessions wait for the user (a question, a permission
    /// request, or blocked).
    Waiting(usize),
    /// A session failed, and none waits.
    Error,
}

/// What a rail button's bottom-right dot says, when there is no badge to
/// read first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// An agent is running a turn.
    Working,
    /// An agent finished its turn.
    Done,
}

impl Badge {
    pub fn of(activity: WorkspaceActivity) -> Option<Self> {
        if activity.waiting > 0 {
            Some(Self::Waiting(activity.waiting))
        } else if activity.errors > 0 {
            Some(Self::Error)
        } else {
            None
        }
    }
}

impl Dot {
    pub fn of(activity: WorkspaceActivity) -> Option<Self> {
        if activity.working > 0 {
            Some(Self::Working)
        } else if activity.done > 0 {
            Some(Self::Done)
        } else {
            None
        }
    }
}

/// A rename in progress.
struct Renaming {
    id: WorkspaceId,
    input: Entity<PromptInput>,
    _events: Subscription,
}

/// The rail of one window. See the module docs.
pub struct Rail {
    workspaces: Entity<Workspaces>,
    /// The workspace whose context menu is open.
    menu: Option<WorkspaceId>,
    renaming: Option<Renaming>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for Rail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rail")
            .field("menu", &self.menu)
            .field("renaming", &self.renaming.as_ref().map(|r| &r.id))
            .finish_non_exhaustive()
    }
}

impl Rail {
    /// The rail of the window showing `workspaces`.
    pub fn new(workspaces: Entity<Workspaces>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = vec![cx.observe(&workspaces, |_, _, cx| cx.notify())];
        if let Some(known) = KnownWorkspaces::global(cx) {
            subscriptions.push(cx.observe(&known, |this, known, cx| {
                // A removed workspace takes its menu with it.
                let known = known.read(cx);
                if this.menu.as_ref().is_some_and(|id| known.get(id).is_none()) {
                    this.menu = None;
                }
                if this
                    .renaming
                    .as_ref()
                    .is_some_and(|r| known.get(&r.id).is_none())
                {
                    this.renaming = None;
                }
                cx.notify();
            }));
        }
        if let Some(activity) = AgentActivity::global(cx) {
            subscriptions.push(cx.observe(&activity, |_, _, cx| cx.notify()));
        }
        Self {
            workspaces,
            menu: None,
            renaming: None,
            _subscriptions: subscriptions,
        }
    }

    /// The workspaces on the rail, top to bottom: every known one, and the
    /// window's current one even before the list has it.
    pub fn list(&self, cx: &App) -> Vec<WorkspaceInfo> {
        let mut list = KnownWorkspaces::global(cx)
            .map(|known| known.read(cx).in_rail_order())
            .unwrap_or_default();
        if let Some(current) = self.workspaces.read(cx).current()
            && !list.iter().any(|w| w.id == current.id)
        {
            list.push(current.clone());
        }
        list
    }

    /// The workspace whose context menu is open.
    #[allow(dead_code)] // Tests use it.
    pub fn menu(&self) -> Option<&WorkspaceId> {
        self.menu.as_ref()
    }

    /// The name field of a rename in progress.
    #[allow(dead_code)] // Tests use it.
    pub fn rename_input(&self) -> Option<&Entity<PromptInput>> {
        self.renaming.as_ref().map(|r| &r.input)
    }

    /// Switch the window to the `index`th workspace on the rail (from 0).
    pub fn switch_to_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(info) = self.list(cx).into_iter().nth(index) {
            self.switch_to(info.id, cx);
        }
    }

    fn switch_to(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.menu = None;
        self.workspaces.update(cx, |w, cx| w.switch_to(id, cx));
        cx.notify();
    }

    fn open_menu(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.renaming = None;
        self.menu = Some(id);
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// Show `id` in a new window (unless a window shows it already: then
    /// that one is focused, as only one window shows a workspace).
    fn open_in_new_window(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.close_menu(cx);
        if let Some(window) = Workspaces::window_showing(&id, cx) {
            window
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            return;
        }
        match windows::open(cx) {
            Ok((_, shell)) => {
                let workspaces = shell.read(cx).workspaces().clone();
                workspaces.update(cx, |w, cx| w.switch_to(id, cx));
            }
            Err(err) => eprintln!("coqo: failed to open a window: {err:#}"),
        }
    }

    fn start_rename(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let name = KnownWorkspaces::global(cx)
            .and_then(|known| known.read(cx).get(&id).map(|w| w.name.clone()))
            .unwrap_or_default();
        let input = cx.new(|cx| {
            let mut input = PromptInput::new("Workspace name", 1, 1, window, cx);
            input.set_text(name, window, cx);
            input
        });
        let events = cx.subscribe(&input, |this, _, event, cx| match event {
            PromptInputEvent::Submit => this.finish_rename(true, cx),
            PromptInputEvent::Cancel => this.finish_rename(false, cx),
            _ => {}
        });
        let focus = coqo_ui::gpui::Focusable::focus_handle(input.read(cx), cx);
        window.focus(&focus, cx);
        self.renaming = Some(Renaming {
            id,
            input,
            _events: events,
        });
        cx.notify();
    }

    /// End the rename, sending the new name if `accept`.
    fn finish_rename(&mut self, accept: bool, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        if accept {
            let name = renaming.input.read(cx).text(cx);
            workspace::rename(renaming.id, &name, cx);
        }
        cx.notify();
    }

    /// Show the workspace's first folder in the platform's file manager.
    fn reveal(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.close_menu(cx);
        cx.reveal_path(&path);
    }

    fn remove(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.close_menu(cx);
        workspace::remove(id, cx);
    }
}

impl Render for Rail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list = self.list(cx);
        let current = self.workspaces.read(cx).current().map(|w| w.id.clone());
        let activity = AgentActivity::global(cx);
        let buttons: Vec<AnyElement> = list
            .iter()
            .enumerate()
            .map(|(ix, info)| {
                let activity = activity
                    .as_ref()
                    .map(|a| a.read(cx).of(&info.id.0))
                    .unwrap_or_default();
                self.render_workspace(ix, info, current.as_ref() == Some(&info.id), activity, cx)
            })
            .collect();
        let open_folder = self.workspaces.downgrade();
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let muted = p.text_muted.hsla();
        div()
            .id("workspace-rail")
            .test_support()
            .w(m.rail_width)
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.))
            .py(px(6.))
            .children(buttons)
            .child(
                rail_button("add-workspace", cx)
                    .test_support()
                    .border_1()
                    .border_dashed()
                    .border_color(p.border_strong.hsla())
                    .cursor_pointer()
                    .hover(|s| s.bg(p.surface.hsla()))
                    .child(icon(Icon::Plus, px(18.), muted))
                    .on_click(move |_, _, cx| prompt_open_folder_in(open_folder.clone(), cx)),
            )
            .child(div().flex_1())
            .child(rail_button("alerts", cx).child(icon(Icon::Bell, px(20.), muted)))
            .child(rail_button("settings", cx).child(icon(Icon::Settings, px(20.), muted)))
    }
}

impl Rail {
    fn render_workspace(
        &self,
        ix: usize,
        info: &WorkspaceInfo,
        selected: bool,
        activity: WorkspaceActivity,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let light = t.appearance == Appearance::Light;
        let id = info.id.clone();
        let menu_id = info.id.clone();
        let name: SharedString = info.name.clone().into();
        let key = (ix < 9).then(|| shortcut(&format!("Ctrl {}", ix + 1)));
        let button = div()
            .id(("workspace", ix))
            .relative()
            .size(m.rail_button)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(m.radius_rail)
            .text_size(px(13.))
            .font_weight(FontWeight::BOLD)
            .cursor_pointer()
            .child(initials(&info.name))
            .on_click(cx.listener(move |this, _, _, cx| this.switch_to(id.clone(), cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.open_menu(menu_id.clone(), cx);
                    cx.stop_propagation();
                }),
            )
            .tooltip(move |_, cx| {
                let (name, key) = (name.clone(), key.clone());
                cx.new(|_| RailTooltip { name, key }).into()
            });
        let button = workspace_surface(button, selected, &p, light).test_support();
        // Badges sit beside the button, not in it: GPUI draws a border over
        // its element's children.
        let mut slot = div()
            .relative()
            .size(m.rail_button)
            .flex_none()
            .child(button)
            .children(Badge::of(activity).map(|badge| render_badge(ix, badge, &p, light)))
            .children(Dot::of(activity).map(|dot| render_dot(ix, dot, &p)));
        if self.menu.as_ref() == Some(&info.id) {
            slot = slot.child(self.render_menu(info, cx));
        }
        if let Some(renaming) = self.renaming.as_ref().filter(|r| r.id == info.id) {
            slot = slot.child(render_rename(renaming, cx));
        }
        slot.into_any_element()
    }

    fn render_menu(&self, info: &WorkspaceInfo, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let shown = Workspaces::window_showing(&info.id, cx).is_some();
        let path = info.roots.first().map(|r| PathBuf::from(&r.path));
        let item = |id: &'static str, glyph: Icon, label: &'static str, color: Rgb| {
            div()
                .id(id)
                .test_support()
                .flex()
                .items_center()
                .gap(px(10.))
                .mx(px(6.))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(8.))
                .cursor_pointer()
                .text_color(color.hsla())
                .hover(|s| s.bg(p.surface_selected.hsla()))
                .child(icon(glyph, px(14.), color.hsla()))
                .child(label)
        };
        let (new_window, rename, reveal, remove) = (
            info.id.clone(),
            info.id.clone(),
            path.clone(),
            info.id.clone(),
        );
        let mut card = popover(cx)
            .id("workspace-menu")
            .occlude()
            .test_support()
            .w(px(230.))
            .py(px(6.))
            .text_size(m.text_sm)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(px(16.))
                    .pt(px(6.))
                    .pb(px(8.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .truncate()
                            .child(info.name.clone()),
                    )
                    .children(path.as_ref().map(|path| {
                        div()
                            .text_size(m.text_xs)
                            .text_color(p.text_subtle.hsla())
                            .truncate()
                            .child(path.display().to_string())
                    })),
            );
        card = if shown {
            // Only one window shows a workspace.
            card.child(
                item(
                    "workspace-menu-new-window",
                    Icon::Columns,
                    "Open in new window",
                    p.text_subtle,
                )
                .opacity(0.6)
                .cursor_default(),
            )
        } else {
            card.child(
                item(
                    "workspace-menu-new-window",
                    Icon::Columns,
                    "Open in new window",
                    p.text,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_in_new_window(new_window.clone(), cx)
                })),
            )
        };
        card = card
            .child(
                item("workspace-menu-rename", Icon::FilePen, "Rename…", p.text).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.start_rename(rename.clone(), window, cx)
                    }),
                ),
            )
            .children(reveal.map(|path| {
                item(
                    "workspace-menu-reveal",
                    Icon::FolderOpen,
                    "Reveal folder",
                    p.text,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.reveal(path.clone(), cx)))
            }))
            .child(
                div()
                    .h(px(1.))
                    .mx(px(12.))
                    .my(px(5.))
                    .bg(p.border_strong.hsla()),
            )
            .child(
                item(
                    "workspace-menu-remove",
                    Icon::Trash,
                    "Remove from list",
                    p.rose,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.remove(remove.clone(), cx))),
            )
            .child(
                div()
                    .px(px(16.))
                    .pt(px(2.))
                    .pb(px(4.))
                    .text_size(m.text_xs)
                    .text_color(p.text_subtle.hsla())
                    .child("Removing never touches the folder."),
            );
        popup(card.into_any_element(), cx)
    }
}

/// A popup to the right of its rail button.
fn popup(card: AnyElement, cx: &App) -> AnyElement {
    let m = &cx.tokens().metrics;
    deferred(
        div()
            .absolute()
            .left(m.rail_button + px(10.))
            .top(px(-6.))
            .child(card),
    )
    .with_priority(3)
    .into_any_element()
}

fn render_rename(renaming: &Renaming, cx: &mut Context<Rail>) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let card = popover(cx)
        .id("workspace-rename")
        .occlude()
        .test_support()
        .w(px(260.))
        .p(px(12.))
        .gap(px(8.))
        .text_size(m.text_sm)
        .on_mouse_down_out(cx.listener(|this, _, _, cx| this.finish_rename(false, cx)))
        .child(
            div()
                .text_size(m.text_label)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text_subtle.hsla())
                .child("RENAME WORKSPACE"),
        )
        .child(
            div()
                .px(px(10.))
                .py(px(6.))
                .rounded(m.radius_control)
                .bg(p.panel_raised.hsla())
                .border_1()
                .border_color(p.blue.alpha(0.6))
                .child(renaming.input.clone()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(m.text_xs)
                .text_color(p.text_subtle.hsla())
                .child(kbd("Enter", cx))
                .child("to rename")
                .child(kbd("Esc", cx))
                .child("to cancel"),
        );
    popup(card.into_any_element(), cx)
}

/// The fill, hairline and shadow of a workspace button (boards 1 and 6):
/// sage-tinted when it is the window's workspace, raised grey otherwise.
fn workspace_surface(
    button: Stateful<coqo_ui::gpui::Div>,
    selected: bool,
    p: &coqo_ui::Palette,
    light: bool,
) -> Stateful<coqo_ui::gpui::Div> {
    let (highlight, drop) = match (light, selected) {
        (true, _) => (0.9, 0.12),
        (false, true) => (0.08, 0.4),
        (false, false) => (0.05, 0.35),
    };
    let shadows = vec![
        BoxShadow {
            color: hsla(0., 0., 1., highlight),
            offset: point(px(0.), px(1.)),
            blur_radius: px(0.),
            spread_radius: px(0.),
            inset: true,
        },
        BoxShadow {
            color: hsla(0., 0., 0., drop),
            offset: point(px(0.), px(if selected { 3. } else { 2. })),
            blur_radius: px(if selected { 8. } else { 6. }),
            spread_radius: px(0.),
            inset: false,
        },
    ];
    let (top, bottom, border, text) = if selected {
        (
            p.ground.mix(p.sage, 0.28),
            p.ground.mix(p.sage, 0.2),
            p.ground.mix(p.sage, 0.42),
            p.text,
        )
    } else {
        (
            p.surface_selected,
            p.surface,
            p.border.mix(p.border_strong, 0.5),
            p.text_muted,
        )
    };
    let hover = p.surface_selected.mix(p.border_strong, 0.4);
    button
        .bg(linear_gradient(
            180.,
            linear_color_stop(top.hsla(), 0.),
            linear_color_stop(bottom.hsla(), 1.),
        ))
        .border_1()
        .border_color(border.hsla())
        .text_color(text.hsla())
        .shadow(shadows)
        .when(!selected, |b| {
            b.hover(|s| s.bg(hover.hsla()).text_color(p.text.hsla()))
        })
}

fn render_badge(ix: usize, badge: Badge, p: &coqo_ui::Palette, light: bool) -> AnyElement {
    let (id, label, color) = match badge {
        Badge::Waiting(n) => (
            "workspace-badge-waiting",
            if n > 9 {
                "9+".to_owned()
            } else {
                n.to_string()
            },
            p.sand,
        ),
        Badge::Error => ("workspace-badge-error", "!".to_owned(), p.rose),
    };
    // Dark text on the light accent in the dark appearance, and the other
    // way round.
    let text = if light { p.panel_raised } else { p.ground };
    div()
        .id((id, ix))
        .test_support()
        .absolute()
        .top(px(-5.))
        .right(px(-5.))
        .min_w(px(18.))
        .h(px(18.))
        .px(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(9.))
        .bg(color.hsla())
        .text_color(text.hsla())
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .child(label)
        .into_any_element()
}

fn render_dot(ix: usize, dot: Dot, p: &coqo_ui::Palette) -> AnyElement {
    let (id, color) = match dot {
        Dot::Working => ("workspace-dot-working", p.blue),
        Dot::Done => ("workspace-dot-done", p.sage),
    };
    let el = div()
        .id((id, ix))
        .test_support()
        .absolute()
        .bottom(px(-3.))
        .right(px(-3.))
        .size(px(12.))
        .rounded_full()
        .bg(color.hsla())
        .border_2()
        .border_color(p.ground.hsla());
    match dot {
        Dot::Working => el
            .with_animation(
                ("workspace-dot-pulse", ix),
                Animation::new(std::time::Duration::from_millis(1600))
                    .repeat()
                    .with_easing(pulsating_between(0.45, 1.)),
                |dot, delta| dot.opacity(delta),
            )
            .into_any_element(),
        Dot::Done => el.into_any_element(),
    }
}

fn rail_button(id: &'static str, cx: &App) -> Stateful<coqo_ui::gpui::Div> {
    let m = &cx.tokens().metrics;
    div()
        .id(id)
        .size(m.rail_button)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(m.radius_rail)
}

/// A rail button's tooltip: the workspace's name and its key.
struct RailTooltip {
    name: SharedString,
    key: Option<SharedString>,
}

impl Render for RailTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        popover(cx)
            .flex_row()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(6.))
            .text_size(t.metrics.text_sm)
            .text_color(t.palette.text.hsla())
            .font_family(t.metrics.font_ui.clone())
            .child(self.name.clone())
            .children(self.key.clone().map(|key| kbd(key, cx)))
    }
}

#[cfg(test)]
mod tests;

/// Renders the shell with four workspaces offscreen to PNGs, for visual
/// review against boards 1 and 6. Needs a GPU:
/// `cargo test -p coqo -- --ignored rail_screenshots`. Writes to
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[cfg(test)]
mod screenshots {
    use std::rc::Rc;

    use coqo_protocol::methods::{
        agents::{AgentState, SessionId, SessionInfo},
        workspace::{RootId, WorkspaceId, WorkspaceInfo, WorkspaceRoot},
    };
    use coqo_ui::{
        Appearance,
        dock::store::MemoryLayoutStore,
        gpui::{AppContext as _, size},
        test::render_offscreen,
    };

    use crate::{
        activity::AgentActivity,
        shell::{DEFAULT_WORKSPACE, Shell},
        workspace::{self, KnownWorkspaces},
    };

    fn workspace(ix: i64, name: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            id: WorkspaceId(format!("ws-{ix}")),
            name: name.into(),
            roots: vec![WorkspaceRoot {
                id: RootId(0),
                path: format!("/home/you/code/{name}"),
                name: name.into(),
            }],
            created_at: ix,
            last_opened_at: ix,
            open: true,
        }
    }

    fn session(ix: i64, state: AgentState) -> SessionInfo {
        SessionInfo {
            id: SessionId(format!("s-{ix}")),
            provider: "claude-code".into(),
            workspace: Some(format!("ws-{ix}")),
            cwd: "/".into(),
            title: None,
            state,
            state_reason: None,
            mode: None,
            closed: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    #[ignore = "needs a GPU; run manually"]
    fn rail_screenshots() {
        let dir = std::env::var_os("COQO_SHOT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        for menu in [false, true] {
            for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
                let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
                    workspace::init(None, cx);
                    super::super::init(cx);
                    let list = vec![
                        workspace(1, "checkout-service"),
                        workspace(2, "web-storefront"),
                        workspace(3, "payments-gateway"),
                        workspace(4, "inventory-api"),
                    ];
                    let known = KnownWorkspaces::global(cx).unwrap();
                    known.update(cx, |k, cx| k.set_for_tests(list.clone(), cx));
                    let activity = AgentActivity::global(cx).unwrap();
                    activity.update(cx, |a, cx| {
                        a.set_for_tests(
                            vec![
                                session(1, AgentState::NeedsInput),
                                session(2, AgentState::Working),
                                session(3, AgentState::Done),
                                session(4, AgentState::Error),
                            ],
                            cx,
                        )
                    });
                    let store = Rc::new(MemoryLayoutStore::default());
                    cx.new(|cx| {
                        let shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                        shell
                            .workspaces()
                            .update(cx, |w, cx| w.set_current_for_tests(list[0].clone(), cx));
                        if menu {
                            shell
                                .rail()
                                .update(cx, |rail, cx| rail.open_menu(list[1].id.clone(), cx));
                        }
                        shell
                    })
                })
                .expect("offscreen render");
                let suffix = if menu { "-menu" } else { "" };
                let path = dir.join(format!("coqo-rail{suffix}-{name}.png"));
                image.save(&path).expect("save png");
                println!("wrote {}", path.display());
            }
        }
    }
}
