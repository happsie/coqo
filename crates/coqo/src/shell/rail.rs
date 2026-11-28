// SPDX-License-Identifier: Apache-2.0

//! The workspace rail (design boards 1 and 6, PLAN §7.9).
//!
//! One 42 px button per **project** (ADR 0028): a repository with the
//! workspaces of all its worktrees, or a workspace in no repository
//! ([`RailProject`]), in the order they were added, with the project's
//! initials. The window's current project is lit in sage. A badge in the
//! top-right corner counts the project's agent sessions, across all its
//! worktrees, that wait for the user (`sand`), or shows `!` when one
//! failed (`rose`); a dot in the bottom-right corner shows an agent working
//! (`blue`, pulsing) or done (`sage`). Background workspaces keep running
//! in the daemon, so their badges stay live ([`AgentActivity`]). The
//! tooltip lists the busy sessions with the agent's status line. The
//! worktrees themselves are switched in the sidebar (`crate::worktrees`).
//!
//! - **Click** (or `Ctrl/Cmd+1..9`): switch the window to the project's
//!   most recently opened worktree, saving this one's layout and restoring
//!   that one's, or focus the window that already shows it.
//! - **Right-click:** open in a new window, rename (the main worktree's
//!   workspace), reveal the folder, or remove the project's workspaces from
//!   the list (`workspace/remove`, which never touches their folders).
//! - **+** opens a folder as a new workspace.
//! - The **bell** shows the unread alerts (a `sand` count when one needs
//!   you, `rose` when one failed) and opens the alert center
//!   ([`crate::alerts`]); opening an alert there is a [`RailEvent`] the
//!   window acts on.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use coqo_protocol::methods::{
    agents::{AgentState, SessionInfo},
    alerts::Alert,
    workspace::{WorkspaceId, WorkspaceInfo},
};
use coqo_ui::{
    ActiveTokens as _, Appearance, Rgb,
    gpui::{
        Animation, AnimationExt as _, AnyElement, App, AppContext as _, BoxShadow, Context, Entity,
        EventEmitter, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
        MouseDownEvent, ParentElement as _, Render, SharedString, Stateful,
        StatefulInteractiveElement as _, Styled as _, Subscription, Window, deferred, div, hsla,
        linear_color_stop, linear_gradient, point, prelude::FluentBuilder as _, pulsating_between,
        px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{Icon, TestSupportExt as _, icon, kbd, popover},
};

use crate::{
    activity::{AgentActivity, WorkspaceActivity},
    alerts::{
        AlertCenter,
        view::{self as alert_view, CenterActions},
    },
    chat::{agent_dot, status_line},
    shell::Shell,
    welcome::shortcut,
    windows,
    workspace::{self, KnownWorkspaces, RailProject, Workspaces, initials, prompt_open_folder_in},
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

/// What the rail asks its window to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailEvent {
    /// Open where an alert points (the alert center's row was clicked).
    OpenAlert(Box<Alert>),
    /// Show the agents overview (its button was clicked).
    ShowAgents,
}

/// A click on the bell this soon after the alert center closed by a click
/// outside it is that same click: it should not open the center again.
const REOPEN_GUARD: Duration = Duration::from_millis(250);

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
    /// Whether the alert center is open.
    alerts_open: bool,
    alerts_closed_at: Option<Instant>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RailEvent> for Rail {}

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
        if let Some(center) = AlertCenter::global(cx) {
            subscriptions.push(cx.observe(&center, |_, _, cx| cx.notify()));
        }
        Self {
            workspaces,
            menu: None,
            renaming: None,
            alerts_open: false,
            alerts_closed_at: None,
            _subscriptions: subscriptions,
        }
    }

    /// The projects on the rail, top to bottom: every known workspace's,
    /// and the window's current one's even before the list has it.
    pub fn list(&self, cx: &App) -> Vec<RailProject> {
        let mut list: Vec<WorkspaceInfo> = KnownWorkspaces::global(cx)
            .map(|known| known.read(cx).all().to_vec())
            .unwrap_or_default();
        if let Some(current) = self.workspaces.read(cx).current()
            && !list.iter().any(|w| w.id == current.id)
        {
            list.push(current.clone());
        }
        RailProject::group(&list)
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

    /// Switch the window to the `index`th project on the rail (from 0).
    pub fn switch_to_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(project) = self.list(cx).into_iter().nth(index) {
            self.switch_to_project(&project, cx);
        }
    }

    /// Switch to `project`'s most recently opened worktree, unless the
    /// window shows one of its worktrees already.
    fn switch_to_project(&mut self, project: &RailProject, cx: &mut Context<Self>) {
        let current = self.workspaces.read(cx).current().map(|w| w.id.clone());
        if current.as_ref().is_some_and(|id| project.contains(id)) {
            self.close_menu(cx);
            return;
        }
        self.switch_to(project.latest().id.clone(), cx);
    }

    fn switch_to(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.menu = None;
        self.workspaces.update(cx, |w, cx| w.switch_to(id, cx));
        cx.notify();
    }

    /// Whether the alert center is open.
    #[allow(dead_code)] // Tests use it.
    pub fn alerts_open(&self) -> bool {
        self.alerts_open
    }

    /// Open the alert center, or close it.
    pub fn toggle_alerts(&mut self, cx: &mut Context<Self>) {
        if !self.alerts_open
            && self
                .alerts_closed_at
                .is_some_and(|t| t.elapsed() < REOPEN_GUARD)
        {
            return;
        }
        self.menu = None;
        self.renaming = None;
        self.alerts_open = !self.alerts_open;
        cx.notify();
    }

    fn close_alerts(&mut self, cx: &mut Context<Self>) {
        if self.alerts_open {
            self.alerts_open = false;
            self.alerts_closed_at = Some(Instant::now());
            cx.notify();
        }
    }

    /// Open where `alert` points: the window does it.
    fn open_alert(&mut self, alert: Alert, cx: &mut Context<Self>) {
        self.alerts_open = false;
        cx.emit(RailEvent::OpenAlert(Box::new(alert)));
        cx.notify();
    }

    fn open_menu(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.alerts_open = false;
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

    fn remove(&mut self, ids: Vec<WorkspaceId>, cx: &mut Context<Self>) {
        self.close_menu(cx);
        for id in ids {
            workspace::remove(id, cx);
        }
    }
}

impl Render for Rail {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list = self.list(cx);
        let current = self.workspaces.read(cx).current().map(|w| w.id.clone());
        let activity = AgentActivity::global(cx);
        let buttons: Vec<AnyElement> = list
            .iter()
            .enumerate()
            .map(|(ix, project)| {
                // Summed over every worktree of the project.
                let activity = activity
                    .as_ref()
                    .map(|a| {
                        let a = a.read(cx);
                        project
                            .workspaces
                            .iter()
                            .map(|w| a.of(&w.id.0))
                            .fold(WorkspaceActivity::default(), WorkspaceActivity::sum)
                    })
                    .unwrap_or_default();
                let selected = current.as_ref().is_some_and(|id| project.contains(id));
                self.render_workspace(ix, project, selected, activity, cx)
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
            .child(self.render_agents(window, cx))
            .child(self.render_bell(cx))
            .child(rail_button("settings", cx).child(icon(Icon::Settings, px(20.), muted)))
    }
}

impl Rail {
    /// The agents overview button (board 6): lit in sage while the overview
    /// is on screen.
    fn render_agents(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let lit = Shell::of_window(window.window_handle(), cx)
            .is_some_and(|shell| shell.read(cx).agents_shown(cx));
        let t = cx.tokens();
        let p = t.palette.clone();
        let light = t.appearance == Appearance::Light;
        let key: SharedString = shortcut(crate::agents::SHOW_AGENTS_LABEL);
        let button = rail_button("rail-agents", cx)
            .cursor_pointer()
            .hover(|s| s.bg(p.surface.hsla()))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(RailEvent::ShowAgents)))
            .tooltip(move |_, cx| {
                let key = key.clone();
                cx.new(|_| RailTooltip {
                    name: "Agents overview".into(),
                    key: Some(key),
                    sessions: Vec::new(),
                })
                .into()
            });
        let button = if lit {
            workspace_surface(button, true, &p, light).child(icon(
                Icon::LayoutGrid,
                px(20.),
                p.text.hsla(),
            ))
        } else {
            button.child(icon(Icon::LayoutGrid, px(20.), p.text_muted.hsla()))
        };
        button.test_support().into_any_element()
    }

    /// The bell, its unread badge, and the alert center when open.
    fn render_bell(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let center = AlertCenter::global(cx);
        let badge = center.as_ref().and_then(|c| c.read(cx).badge());
        let label: SharedString = match badge {
            Some(b) => format!("Alerts · {} unread", b.unread).into(),
            None => "Alerts".into(),
        };
        let bell = rail_button("alerts", cx)
            .test_support()
            .cursor_pointer()
            .when(self.alerts_open, |b| b.bg(p.surface.hsla()))
            .hover(|s| s.bg(p.surface.hsla()))
            .child(icon(
                Icon::Bell,
                px(20.),
                if self.alerts_open {
                    p.text
                } else {
                    p.text_muted
                }
                .hsla(),
            ))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_alerts(cx)))
            .tooltip(move |_, cx| {
                let label = label.clone();
                cx.new(|_| RailTooltip {
                    name: label,
                    key: None,
                    sessions: Vec::new(),
                })
                .into()
            });
        let mut slot = div()
            .relative()
            .size(m.rail_button)
            .flex_none()
            .child(bell)
            .children(badge.map(|b| alert_view::bell_badge(b, cx)));
        if self.alerts_open
            && let Some(center) = center
        {
            let rail = cx.entity().downgrade();
            let (dismiss, dnd, all) = (center.downgrade(), center.downgrade(), center.downgrade());
            let actions = CenterActions {
                open: Rc::new(move |alert, _, cx| {
                    let alert = alert.clone();
                    rail.update(cx, |rail, cx| rail.open_alert(alert, cx)).ok();
                }),
                dismiss: Rc::new(move |id, _, cx| {
                    dismiss.update(cx, |c, cx| c.dismiss(id, cx)).ok();
                }),
                toggle_dnd: Rc::new(move |_, cx| {
                    dnd.update(cx, |c, cx| {
                        let on = !c.do_not_disturb();
                        c.set_do_not_disturb(on, cx);
                    })
                    .ok();
                }),
                dismiss_all: Rc::new(move |_, cx| {
                    all.update(cx, |c, cx| c.dismiss_all(cx)).ok();
                }),
            };
            let card = div()
                .id("alerts-popup")
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_alerts(cx)))
                .child(alert_view::center(center.read(cx), &actions, cx));
            slot = slot.child(
                deferred(
                    div()
                        .absolute()
                        .left(m.rail_button + px(10.))
                        .bottom(px(-6.))
                        .child(card),
                )
                .with_priority(3),
            );
        }
        slot.into_any_element()
    }

    fn render_workspace(
        &self,
        ix: usize,
        project: &RailProject,
        selected: bool,
        activity: WorkspaceActivity,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let light = t.appearance == Appearance::Light;
        let target = project.clone();
        let menu_id = project.latest().id.clone();
        let tooltip_ids: Vec<WorkspaceId> =
            project.workspaces.iter().map(|w| w.id.clone()).collect();
        let name: SharedString = project.name.clone().into();
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
            .child(initials(&project.name))
            .on_click(cx.listener(move |this, _, _, cx| this.switch_to_project(&target, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.open_menu(menu_id.clone(), cx);
                    cx.stop_propagation();
                }),
            )
            .tooltip(move |_, cx| {
                let (name, key) = (name.clone(), key.clone());
                let sessions = AgentActivity::global(cx)
                    .map(|a| {
                        let a = a.read(cx);
                        TooltipSession::list(crate::activity::busy_in_any(
                            a.sessions(),
                            &tooltip_ids,
                        ))
                    })
                    .unwrap_or_default();
                cx.new(|_| RailTooltip {
                    name,
                    key,
                    sessions,
                })
                .into()
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
        if self.menu.as_ref().is_some_and(|id| project.contains(id)) {
            slot = slot.child(self.render_menu(project, cx));
        }
        if let Some(renaming) = self.renaming.as_ref().filter(|r| project.contains(&r.id)) {
            slot = slot.child(render_rename(renaming, cx));
        }
        slot.into_any_element()
    }

    fn render_menu(&self, project: &RailProject, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let info = project.latest();
        let shown = Workspaces::window_showing(&info.id, cx).is_some();
        let path = project.folder();
        let several = project.workspaces.len() > 1;
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
            project.main().map(|w| w.id.clone()),
            path.clone(),
            project
                .workspaces
                .iter()
                .map(|w| w.id.clone())
                .collect::<Vec<_>>(),
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
                            .child(project.name.clone()),
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
            .children(rename.map(|rename| {
                item("workspace-menu-rename", Icon::FilePen, "Rename…", p.text).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.start_rename(rename.clone(), window, cx)
                    }),
                )
            }))
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
                    if several {
                        "Remove its worktrees from list"
                    } else {
                        "Remove from list"
                    },
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
                    .child(if several {
                        "Removing never touches the folders."
                    } else {
                        "Removing never touches the folder."
                    }),
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

/// Sessions a rail tooltip lists at most.
const TOOLTIP_SESSIONS: usize = 5;

/// A busy session in a rail tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooltipSession {
    pub state: AgentState,
    pub title: SharedString,
    /// The agent's status line, or why it waits.
    pub line: Option<SharedString>,
}

impl TooltipSession {
    /// The first [`TOOLTIP_SESSIONS`] of `sessions`, as
    /// [`crate::activity::busy_in_any`] orders them.
    pub fn list<'a>(sessions: impl IntoIterator<Item = &'a SessionInfo>) -> Vec<Self> {
        sessions
            .into_iter()
            .take(TOOLTIP_SESSIONS)
            .map(|s| Self {
                state: s.state,
                title: s
                    .title
                    .clone()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "New chat".into())
                    .into(),
                line: status_line(s).map(Into::into),
            })
            .collect()
    }
}

/// A rail button's tooltip: the workspace's name and its key, and what its
/// busy agents are doing.
struct RailTooltip {
    name: SharedString,
    key: Option<SharedString>,
    sessions: Vec<TooltipSession>,
}

impl Render for RailTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .child(self.name.clone())
            .children(self.key.clone().map(|key| kbd(key, cx)));
        let rows = self.sessions.iter().enumerate().map(|(ix, s)| {
            div()
                .flex()
                .items_start()
                .gap(px(8.))
                .child(
                    div()
                        .pt(px(5.))
                        .child(agent_dot(s.state, ("rail-tooltip-dot", ix), cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().child(s.title.clone()))
                        .children(s.line.clone().map(|line| {
                            div()
                                .truncate()
                                .text_size(m.text_label)
                                .text_color(p.text_muted.hsla())
                                .child(line)
                        })),
                )
        });
        popover(cx)
            .flex_col()
            .gap(px(6.))
            .max_w(px(300.))
            .px(px(10.))
            .py(px(6.))
            .text_size(m.text_sm)
            .text_color(p.text.hsla())
            .font_family(m.font_ui.clone())
            .child(header)
            .children(rows)
    }
}

#[cfg(test)]
mod tests;

/// Renders the shell with four projects (one with four worktrees, one a
/// subagent's that the main worktree's chat works in)
/// offscreen to PNGs, for visual review against boards 1 and 6: the rail,
/// its menu, and the sidebar's worktree switcher open. Needs a GPU:
/// `cargo test -p coqo -- --ignored rail_screenshots`. Writes to
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[cfg(test)]
mod screenshots {
    use std::rc::Rc;

    use coqo_protocol::methods::{
        agents::{AgentState, SessionId, SessionInfo, WorktreeActivity},
        git::{Head, RepoInfo, StatusResult, Worktree, WorktreesResult},
        workspace::{ProjectInfo, RootId, WorkspaceId, WorkspaceInfo, WorkspaceRoot},
    };
    use coqo_ui::{
        Appearance,
        dock::store::MemoryLayoutStore,
        gpui::{AppContext as _, size},
        test::render_offscreen,
    };

    use crate::{
        activity::AgentActivity,
        git::model::{GitModel, RootGit},
        shell::{DEFAULT_WORKSPACE, Shell},
        workspace::{self, KnownWorkspaces},
    };

    const MAIN: &str = "/home/you/code/checkout-service";

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
            project: Some(ProjectInfo::from_common_dir(&format!(
                "/home/you/code/{name}/.git"
            ))),
        }
    }

    /// A linked worktree of checkout-service.
    fn worktree(ix: i64, branch: &str) -> WorkspaceInfo {
        let folder = branch.replace('/', "-");
        let mut info = workspace(ix, &folder);
        info.roots[0].path = format!("{MAIN}-worktrees/{folder}");
        info.project = Some(ProjectInfo::from_common_dir(&format!("{MAIN}/.git")));
        info.last_opened_at = 100 + ix;
        info
    }

    fn row(path: &str, branch: &str, main: bool, changes: u32) -> Worktree {
        Worktree {
            path: path.into(),
            branch: Some(branch.into()),
            head: Some("4f1c2ab".into()),
            main,
            locked: false,
            prunable: false,
            changes: Some(changes),
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
            state_since: None,
            status: None,
            criteria: Vec::new(),
            tool_use: Vec::new(),
            worktrees: Vec::new(),
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
        for (suffix, menu, switcher) in [
            ("", false, false),
            ("-menu", true, false),
            ("-worktrees", false, true),
        ] {
            for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
                let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
                    workspace::init(None, cx);
                    super::super::init(cx);
                    let list = vec![
                        workspace(1, "checkout-service"),
                        workspace(2, "web-storefront"),
                        workspace(3, "payments-gateway"),
                        workspace(4, "inventory-api"),
                        worktree(5, "feature/volume-discount"),
                        worktree(6, "fix-flaky-it"),
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
                                session(5, AgentState::Working),
                                session(6, AgentState::NeedsInput),
                                // The main worktree's chat, its subagent
                                // working in `.claude/worktrees` (#359).
                                SessionInfo {
                                    id: SessionId("s-7".into()),
                                    title: Some("Split pricing".into()),
                                    provider: coqo_protocol::methods::agents::CLAUDE_PROVIDER_ID
                                        .into(),
                                    worktrees: vec![WorktreeActivity {
                                        path: format!("{MAIN}/.claude/worktrees/agent-a1b2"),
                                        since: 1,
                                    }],
                                    ..session(1, AgentState::Working)
                                },
                            ],
                            cx,
                        )
                    });
                    let store = Rc::new(MemoryLayoutStore::default());
                    cx.new(|cx| {
                        let shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                        let current = list[4].clone();
                        shell
                            .workspaces()
                            .update(cx, |w, cx| w.set_current_for_tests(current.clone(), cx));
                        let status = StatusResult {
                            repo: RepoInfo {
                                workdir: current.roots[0].path.clone(),
                                prefix: String::new(),
                            },
                            head: Head {
                                branch: Some("feature/volume-discount".into()),
                                ..Head::default()
                            },
                            entries: Vec::new(),
                        };
                        let mut root = RootGit::new(RootId(0));
                        root.status = Some(status);
                        GitModel::for_window(window, cx)
                            .update(cx, |m, cx| m.set_for_tests(current.clone(), vec![root], cx));
                        let rows = WorktreesResult {
                            worktrees: vec![
                                row(MAIN, "main", true, 0),
                                row(&current.roots[0].path, "feature/volume-discount", false, 3),
                                row(&list[5].roots[0].path, "fix-flaky-it", false, 1),
                                // Made by Claude Code for a subagent: no
                                // workspace, its work the main chat's.
                                Worktree {
                                    branch: None,
                                    ..row(
                                        &format!("{MAIN}/.claude/worktrees/agent-a1b2"),
                                        "",
                                        false,
                                        2,
                                    )
                                },
                            ],
                            worktrees_dir: format!("{MAIN}-worktrees"),
                        };
                        shell
                            .switcher()
                            .update(cx, |s, cx| s.set_for_tests(rows, switcher, cx));
                        if menu {
                            shell
                                .rail()
                                .update(cx, |rail, cx| rail.open_menu(list[1].id.clone(), cx));
                        }
                        shell
                    })
                })
                .expect("offscreen render");
                let path = dir.join(format!("coqo-rail{suffix}-{name}.png"));
                image.save(&path).expect("save png");
                println!("wrote {}", path.display());
            }
        }
    }
}
