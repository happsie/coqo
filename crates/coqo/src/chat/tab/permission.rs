// SPDX-License-Identifier: Apache-2.0

//! Permission requests in the chat (boards 1 and 6) and the workspace's
//! permission rules (#65).
//!
//! - **The card** says what the agent wants ("Wants to run `cargo test`",
//!   "Wants to edit src/cart.rs"), previews the command or the diff, and has
//!   one button per option the provider offers, ordered allow → reject and
//!   once → always. The click answers with the option's id, so the provider's
//!   own "always" choice goes to the agent unchanged.
//! - **Keys:** focus jumps to a new pending request (unless you are typing a
//!   message); **Enter** allows once and **Esc** denies once, and focus goes
//!   back to the composer.
//! - **Remember for this workspace:** when the daemon offers a rule for the
//!   request (`suggestedRule`: a simple command, or files inside the
//!   workspace), a tick box under the buttons stores it with the answer
//!   (`remember: true`). The daemon then answers matching requests itself:
//!   they show as one line, "Allowed by a workspace rule", and never wait.
//! - **Rules panel:** the session panel lists the workspace's rules
//!   (`agents/permissionRules`) with a delete button each
//!   (`agents/deletePermissionRule`).

use std::collections::HashSet;

use coqo_protocol::methods::agents::{
    ContentBlock, DeletePermissionRule, PermissionOptionKind, PermissionOutcome, PermissionRequest,
    PermissionRequestId, PermissionRule, PermissionRuleId, PermissionRuleSpec, PermissionRules,
    RuleDecision, RuleParams, SessionUpdate, ToolCallContent, ToolKind, WorkspaceRulesParams,
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, Context, ElementId, FocusHandle, Focusable as _, FontWeight,
        InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
        StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
    },
    serde_json::Value,
    text::markdown,
    widgets::{Icon, TestSupportExt as _, icon, kbd, primary, raised, section_label},
};

use super::{ChatTab, kind_icon, option_rank};
use crate::chat::{AllowPermission, RejectPermission, transcript::diffs_of};

/// The tallest a plan gets in its approval card before it scrolls.
const PLAN_MAX_HEIGHT: f32 = 360.;

/// Where focus goes on the next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FocusTarget {
    /// The oldest pending permission request.
    Permission,
    /// Back to the composer, if a card had it.
    Composer,
}

/// The chat tab's permission state.
pub(super) struct Permissions {
    /// The focus of the oldest pending request's card.
    pub focus: FocusHandle,
    /// The newest request focus jumped to.
    jumped_to: Option<PermissionRequestId>,
    pub focus_next: Option<FocusTarget>,
    /// Requests whose "remember for this workspace" is ticked.
    pub remember: HashSet<PermissionRequestId>,
    /// The workspace's rules, and which workspace they are of.
    pub rules: Vec<PermissionRule>,
    rules_of: Option<String>,
}

impl Permissions {
    pub fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            jumped_to: None,
            focus_next: None,
            remember: HashSet::new(),
            rules: Vec::new(),
            rules_of: None,
        }
    }
}

/// What a request asks, for the card's title.
struct Ask {
    /// "Wants to run", or the request's own title.
    verb: String,
    /// The command or path, in mono.
    subject: Option<String>,
    command: Option<String>,
}

fn command_of(raw: &Value) -> Option<String> {
    match raw.get("command").or_else(|| raw.get("cmd"))? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => Some(
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// How a rule reads: "`cargo test …`", "files in `src/**`".
pub(super) fn rule_text(spec: &PermissionRuleSpec) -> String {
    match spec.tool {
        ToolKind::Execute => format!("{} …", spec.pattern),
        _ => spec.pattern.clone(),
    }
}

fn rule_scope(spec: &PermissionRuleSpec) -> &'static str {
    match spec.tool {
        ToolKind::Execute => "commands starting with",
        ToolKind::Edit => "edits of",
        ToolKind::Read => "reads of",
        _ => "",
    }
}

impl ChatTab {
    /// Whether the oldest pending request's card has the focus.
    #[cfg(test)]
    pub fn permission_focused(&self, window: &Window) -> bool {
        self.permissions.focus.is_focused(window)
    }

    /// The workspace's rules, as last fetched.
    #[cfg(test)]
    pub fn rules(&self) -> &[PermissionRule] {
        &self.permissions.rules
    }

    /// Show `rules` without a daemon (screenshots).
    #[cfg(test)]
    pub fn set_preview_rules(&mut self, rules: Vec<PermissionRule>, cx: &mut Context<Self>) {
        self.permissions.rules_of = self.workspace_id(cx);
        self.permissions.rules = rules;
        cx.notify();
    }

    /// Permission requests (not the agent's questions) still waiting,
    /// oldest first.
    fn pending_asks(&self) -> impl Iterator<Item = &PermissionRequest> {
        self.transcript
            .pending_permissions()
            .filter(|r| !r.is_question())
    }

    /// Call with each batch before it is applied: answers carrying a rule
    /// mean the workspace's rules changed or were used.
    pub(super) fn note_permission_updates(
        &mut self,
        updates: &[(u64, SessionUpdate)],
        cx: &mut Context<Self>,
    ) {
        let rules_touched = updates.iter().any(|(seq, u)| {
            *seq > self.transcript.head
                && matches!(
                    u,
                    SessionUpdate::PermissionResolved { by_rule, remembered, .. }
                        if by_rule.is_some() || remembered.is_some()
                )
        });
        if rules_touched {
            self.refresh_rules(cx);
        }
    }

    /// After a batch: jump to a new pending request.
    pub(super) fn jump_to_pending(&mut self) {
        let Some(request) = self.pending_asks().next() else {
            return;
        };
        let id = request.id;
        if self.permissions.jumped_to.is_some_and(|j| j >= id) {
            return;
        }
        self.permissions.jumped_to = Some(id);
        self.permissions.focus_next = Some(FocusTarget::Permission);
        if let Some(ix) = self.transcript.permission_index(id)
            && !self.list.is_following_tail()
        {
            self.list.scroll_to_reveal_item(ix);
        }
    }

    /// Moves focus as the last updates asked. Call while rendering.
    pub(super) fn apply_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.permissions.focus_next.take() else {
            return;
        };
        let composer = self.composer.read(cx).focus_handle(cx);
        match target {
            FocusTarget::Permission => {
                // Don't take a message being typed: Enter would answer.
                let typing =
                    composer.is_focused(window) && !self.composer.read(cx).text(cx).is_empty();
                if !typing && self.pending_asks().next().is_some() {
                    window.focus(&self.permissions.focus, cx);
                }
            }
            FocusTarget::Composer => {
                if self.permissions.focus.is_focused(window) {
                    window.focus(&composer, cx);
                }
            }
        }
    }

    /// Where focus goes once a request is answered.
    pub(super) fn after_answer(&mut self, answered: PermissionRequestId) {
        let more = self.pending_asks().any(|r| r.id != answered);
        self.permissions.focus_next = Some(if more {
            FocusTarget::Permission
        } else {
            FocusTarget::Composer
        });
        self.permissions.remember.remove(&answered);
    }

    /// Answer the oldest pending request with its option of the first of
    /// `kinds` it has. `false` if it has none.
    fn answer_by_kind(&mut self, kinds: &[PermissionOptionKind], cx: &mut Context<Self>) -> bool {
        let Some(request) = self.pending_asks().next() else {
            return false;
        };
        let Some(option) = kinds
            .iter()
            .find_map(|k| request.options.iter().find(|o| o.kind == *k))
        else {
            return false;
        };
        let (id, option) = (request.id, option.id.clone());
        self.respond(id, option, cx);
        true
    }

    fn toggle_remember(&mut self, id: PermissionRequestId, cx: &mut Context<Self>) {
        if !self.permissions.remember.remove(&id) {
            self.permissions.remember.insert(id);
        }
        if let Some(ix) = self.transcript.permission_index(id) {
            self.list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    fn workspace_id(&self, cx: &App) -> Option<String> {
        self.latest_info(cx).and_then(|i| i.workspace.clone())
    }

    /// Fetch the workspace's rules.
    pub(super) fn refresh_rules(&mut self, cx: &mut Context<Self>) {
        let (Some(workspace), Some(daemon)) = (self.workspace_id(cx), Self::daemon(cx)) else {
            return;
        };
        let request = daemon
            .read(cx)
            .request::<PermissionRules>(WorkspaceRulesParams {
                workspace: workspace.clone(),
            });
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(rules) => {
                        this.permissions.rules = rules;
                        this.permissions.rules_of = Some(workspace);
                    }
                    Err(e) => {
                        this.error = Some(format!("Couldn't load the workspace's rules: {e}"))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Fetch the rules if they are not the current workspace's yet.
    pub(super) fn ensure_rules(&mut self, cx: &mut Context<Self>) {
        if self.permissions.rules_of != self.workspace_id(cx) {
            self.refresh_rules(cx);
        }
    }

    fn delete_rule(&mut self, id: PermissionRuleId, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let request = daemon
            .read(cx)
            .request::<DeletePermissionRule>(RuleParams { id });
        self.permissions.rules.retain(|r| r.id != id);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.error = Some(format!("Couldn't delete the rule: {e}"));
                }
                this.refresh_rules(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The plan a plan-mode approval (`switch_mode`, Claude's
    /// `ExitPlanMode`) asks to approve: its text content.
    fn plan_of(&self, request: &PermissionRequest) -> Option<String> {
        let tc = request.tool_call.as_ref()?;
        let earlier = self.transcript.tool(&tc.id).map(|t| &t.call);
        let kind = tc.kind.or(earlier.map(|c| c.kind))?;
        if kind != ToolKind::SwitchMode {
            return None;
        }
        let content = tc
            .content
            .as_deref()
            .or(earlier.map(|c| c.content.as_slice()))?;
        let text: Vec<&str> = content
            .iter()
            .filter_map(|c| match c {
                ToolCallContent::Content {
                    content: ContentBlock::Text { text },
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let text = text.join("\n\n");
        (!text.trim().is_empty()).then_some(text)
    }

    fn ask_of(&self, request: &PermissionRequest) -> Ask {
        let tc = request.tool_call.as_ref();
        let earlier = tc
            .and_then(|tc| self.transcript.tool(&tc.id))
            .map(|t| &t.call);
        let kind = tc
            .and_then(|tc| tc.kind)
            .or(earlier.map(|c| c.kind))
            .unwrap_or_default();
        let raw = tc
            .and_then(|tc| tc.raw_input.as_ref())
            .or(earlier.and_then(|c| c.raw_input.as_ref()));
        let content = tc
            .and_then(|tc| tc.content.as_deref())
            .or(earlier.map(|c| c.content.as_slice()))
            .unwrap_or_default();
        let diff = content.iter().find_map(|c| match c {
            ToolCallContent::Diff { path, old_text, .. } => {
                Some((path.clone(), old_text.is_none()))
            }
            _ => None,
        });
        let path = diff.as_ref().map(|(p, _)| p.clone()).or_else(|| {
            let raw = raw?;
            ["file_path", "path", "notebook_path", "abs_path"]
                .iter()
                .find_map(|k| raw.get(*k)?.as_str().map(str::to_owned))
        });
        let command = raw.and_then(command_of);
        let own = || Ask {
            verb: request.title.clone(),
            subject: None,
            command: None,
        };
        let with_path = |verb: &str| match &path {
            Some(p) => Ask {
                verb: verb.into(),
                subject: Some(self.relative(p).to_owned()),
                command: None,
            },
            None => own(),
        };
        match kind {
            ToolKind::Execute => match command {
                Some(c) => Ask {
                    verb: "Wants to run".into(),
                    subject: Some(c.lines().next().unwrap_or_default().to_owned()),
                    command: Some(c),
                },
                None => own(),
            },
            ToolKind::Edit if diff.as_ref().is_some_and(|(_, created)| *created) => {
                with_path("Wants to create")
            }
            ToolKind::Edit => with_path("Wants to edit"),
            ToolKind::Delete => with_path("Wants to delete"),
            ToolKind::Move => with_path("Wants to move"),
            ToolKind::Read => with_path("Wants to read"),
            _ => own(),
        }
    }

    /// A permission request's row.
    pub(super) fn render_permission(
        &self,
        ix: usize,
        request: &PermissionRequest,
        outcome: Option<&PermissionOutcome>,
        by_rule: Option<&PermissionRule>,
        remembered: Option<&PermissionRule>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(rule) = by_rule {
            return self.render_rule_answer(ix, request, rule, cx);
        }
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let ask = self.ask_of(request);
        let pending = outcome.is_none() && !self.is_closed();
        let first = self.pending_asks().next().map(|r| r.id) == Some(request.id);
        let title = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(ask.verb.clone())
            .children(ask.subject.clone().map(|s| {
                div()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(6.))
                    .bg(p.surface_strong.hsla())
                    .font_family(m.font_mono.clone())
                    .font_weight(FontWeight::NORMAL)
                    .text_size(px(12.5))
                    .child(s)
            }));
        let plan = self.plan_of(request);
        let subtitle = if pending && plan.is_some() {
            format!("{} asks you to approve its plan", self.provider_label())
        } else if pending {
            format!("{} asks for permission", self.provider_label())
        } else {
            let mut label = super::resolved_label(request, outcome);
            if remembered.is_some() {
                label.push_str(" · remembered for this workspace");
            }
            label
        };
        let mut card = div()
            .id(("chat-permission", ix))
            .test_support()
            .flex()
            .flex_col()
            .gap(px(12.))
            .px(px(16.))
            .py(px(14.))
            .rounded(px(16.))
            .bg(if pending {
                p.sand.alpha(0.08)
            } else {
                p.surface.hsla()
            })
            .when(pending, |d| d.border_1().border_color(p.sand.alpha(0.3)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(icon(
                        Icon::ShieldAlert,
                        px(18.),
                        if pending {
                            p.sand.hsla()
                        } else {
                            p.text_subtle.hsla()
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(title)
                            .child(
                                div()
                                    .text_size(m.text_sm)
                                    .text_color(p.text_muted.hsla())
                                    .child(subtitle),
                            ),
                    ),
            );
        if !pending {
            return card.into_any_element();
        }
        if let Some(command) = &ask.command {
            card = card.child(
                div()
                    .id(("chat-permission-command", ix))
                    .test_support()
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .bg(p.panel.hsla())
                    .font_family(m.font_mono.clone())
                    .text_size(px(12.))
                    .child(format!("$ {command}")),
            );
        }
        let diffs = request
            .tool_call
            .as_ref()
            .and_then(|tc| tc.content.as_ref())
            .map(|c| diffs_of(c))
            .or_else(|| {
                let tc = request.tool_call.as_ref()?;
                Some(self.transcript.tool(&tc.id)?.diffs.clone())
            })
            .unwrap_or_default();
        for diff in &diffs {
            card = card.child(
                div()
                    .font_family(m.font_mono.clone())
                    .text_size(px(12.))
                    .child(self.render_diff(diff, cx)),
            );
        }
        if let Some(plan) = plan {
            card = card.child(
                div()
                    .id(("chat-permission-plan", ix))
                    .test_support()
                    .max_h(px(PLAN_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .bg(p.panel.hsla())
                    .child(markdown(("chat-permission-plan-md", ix), plan)),
            );
        }
        let mut options = request.options.clone();
        options.sort_by_key(|o| option_rank(o.kind));
        let mut buttons = div().flex().flex_wrap().items_center().gap(px(6.));
        for option in options {
            let base = div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(14.))
                .py(px(7.))
                .rounded(m.radius_control)
                .text_size(m.text_sm)
                .cursor_pointer();
            let base = if option.kind == PermissionOptionKind::AllowOnce {
                primary(base, cx)
            } else {
                raised(base, cx).text_color(p.text.hsla())
            };
            let key = match option.kind {
                PermissionOptionKind::AllowOnce if first => Some("Enter"),
                PermissionOptionKind::RejectOnce if first => Some("Esc"),
                _ => None,
            };
            let (id, request_id) = (option.id.clone(), request.id);
            buttons = buttons.child(
                base.id(ElementId::Name(
                    format!("chat-permission-{ix}-{}", option.id).into(),
                ))
                .test_support()
                .child(option.name.clone())
                .children(key.map(|k| kbd(k, cx)))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.respond(request_id, id.clone(), cx)),
                ),
            );
        }
        card = card.child(buttons);
        if let Some(spec) = &request.suggested_rule {
            let ticked = self.permissions.remember.contains(&request.id);
            let request_id = request.id;
            let tick = div()
                .size(px(16.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .border_1()
                .map(|d| {
                    if ticked {
                        d.bg(p.sage.hsla()).border_color(p.sage.hsla())
                    } else {
                        d.border_color(p.border_strong.hsla())
                    }
                })
                .when(ticked, |d| {
                    d.child(icon(Icon::Check, px(12.), p.ground.hsla()))
                });
            card = card.child(
                div()
                    .id(ElementId::Name(
                        format!("chat-permission-{ix}-remember").into(),
                    ))
                    .test_support()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .text_size(m.text_sm)
                    .text_color(p.text_muted.hsla())
                    .cursor_pointer()
                    .child(tick)
                    .child("Remember for this workspace:")
                    .child(rule_scope(spec))
                    .child(
                        div()
                            .px(px(6.))
                            .py(px(1.))
                            .rounded(px(6.))
                            .bg(p.surface_strong.hsla())
                            .text_color(p.text.hsla())
                            .font_family(m.font_mono.clone())
                            .text_size(px(12.))
                            .child(spec.pattern.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_remember(request_id, cx);
                    })),
            );
        }
        if first {
            card = card
                .key_context("ChatPermission")
                .track_focus(&self.permissions.focus)
                .on_action(cx.listener(|this, _: &AllowPermission, _, cx| {
                    if !this.answer_by_kind(&[PermissionOptionKind::AllowOnce], cx) {
                        cx.propagate();
                    }
                }))
                .on_action(cx.listener(|this, _: &RejectPermission, _, cx| {
                    // Without a "reject once", Esc stops the turn instead.
                    if !this.answer_by_kind(&[PermissionOptionKind::RejectOnce], cx) {
                        cx.propagate();
                    }
                }));
        }
        card.into_any_element()
    }

    /// A request a workspace rule answered: one line.
    fn render_rule_answer(
        &self,
        ix: usize,
        request: &PermissionRequest,
        rule: &PermissionRule,
        cx: &App,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let ask = self.ask_of(request);
        let (glyph, color, verdict) = match rule.decision {
            RuleDecision::Allow => (Icon::Check, p.sage.hsla(), "Allowed by a workspace rule"),
            RuleDecision::Reject => (Icon::X, p.rose.hsla(), "Denied by a workspace rule"),
        };
        let what = match &ask.subject {
            Some(s) => format!(
                "{} {s}",
                ask.verb.to_lowercase().trim_start_matches("wants to ")
            ),
            None => ask.verb.clone(),
        };
        div()
            .id(("chat-permission-auto", ix))
            .test_support()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .text_size(m.text_sm)
            .text_color(p.text_muted.hsla())
            .child(icon(glyph, px(13.), color))
            .child(div().text_color(color).child(verdict))
            .child(
                div()
                    .px(px(6.))
                    .rounded(px(6.))
                    .bg(p.surface_strong.hsla())
                    .font_family(m.font_mono.clone())
                    .text_size(px(12.))
                    .child(rule_text(&rule.spec())),
            )
            .child(div().truncate().child(what))
            .into_any_element()
    }

    /// The session panel's list of the workspace's rules.
    pub(super) fn render_rules(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let rules = &self.permissions.rules;
        let mut card = div()
            .id("chat-rules")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(14.))
            .bg(p.panel_raised.hsla())
            .text_size(m.text_sm)
            .child(section_label("Workspace rules", cx));
        if rules.is_empty() {
            card = card.child(
                div()
                    .px_2()
                    .text_color(p.text_subtle.hsla())
                    .child(SharedString::from(
                        "Every request is asked. Tick “Remember for this workspace” on one to add a rule.",
                    )),
            );
        }
        for rule in rules {
            let id = rule.id;
            let (verdict, color) = match rule.decision {
                RuleDecision::Allow => ("Allow", p.sage.hsla()),
                RuleDecision::Reject => ("Deny", p.rose.hsla()),
            };
            card = card.child(
                div()
                    .id(("chat-rule", id.0))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px_2()
                    .child(icon(kind_icon(rule.tool), px(13.), p.text_muted.hsla()))
                    .child(div().flex_none().text_color(color).child(verdict))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(m.font_mono.clone())
                            .text_size(px(12.))
                            .child(rule_text(&rule.spec())),
                    )
                    .child(
                        div()
                            .id(("chat-rule-delete", id.0))
                            .test_support()
                            .flex_none()
                            .p(px(3.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(|d| d.bg(p.surface.hsla()))
                            .child(icon(Icon::X, px(12.), p.text_subtle.hsla()))
                            .on_click(cx.listener(move |this, _, _, cx| this.delete_rule(id, cx))),
                    ),
            );
        }
        card.into_any_element()
    }
}
