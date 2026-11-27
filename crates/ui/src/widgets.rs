// SPDX-License-Identifier: Apache-2.0

//! Zen building blocks on top of GPUI primitives. Views compose these instead of
//! styling GPUI elements or gpui-component widgets directly.

use gpui_kit::{
    App, BoxShadow, Div, ElementId, Hsla, InteractiveElement as _, ParentElement as _, Pixels,
    SharedString, Stateful, Styled as _, Svg, div, hsla, linear_color_stop, linear_gradient, point,
    px, svg,
};

use crate::theme::{ActiveTokens as _, Appearance, Rgb};

/// Icons from the bundled Lucide set. Add variants as views need them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Search,
    Zap,
    Bell,
    Settings,
    Plus,
    MessageCircle,
    Columns,
    GitBranch,
    Folder,
    X,
    Rows,
    Sparkles,
    CircleAlert,
    ChevronRight,
    ChevronDown,
    File,
    FileText,
    FileCode,
    Image,
    FolderOpen,
    RotateCw,
    Check,
    Circle,
    CircleDot,
    LoaderCircle,
    Square,
    FilePen,
    Terminal,
    Brain,
    Globe,
    Trash,
    Eye,
    ShieldAlert,
    Wrench,
    ArrowRightLeft,
    CircleX,
    LogIn,
    ArrowUp,
}

impl Icon {
    /// Every variant, for tests that check the bundle.
    pub const ALL: [Icon; 38] = [
        Icon::Search,
        Icon::Zap,
        Icon::Bell,
        Icon::Settings,
        Icon::Plus,
        Icon::MessageCircle,
        Icon::Columns,
        Icon::GitBranch,
        Icon::Folder,
        Icon::X,
        Icon::Rows,
        Icon::Sparkles,
        Icon::CircleAlert,
        Icon::ChevronRight,
        Icon::ChevronDown,
        Icon::File,
        Icon::FileText,
        Icon::FileCode,
        Icon::Image,
        Icon::FolderOpen,
        Icon::RotateCw,
        Icon::Check,
        Icon::Circle,
        Icon::CircleDot,
        Icon::LoaderCircle,
        Icon::Square,
        Icon::FilePen,
        Icon::Terminal,
        Icon::Brain,
        Icon::Globe,
        Icon::Trash,
        Icon::Eye,
        Icon::ShieldAlert,
        Icon::Wrench,
        Icon::ArrowRightLeft,
        Icon::CircleX,
        Icon::LogIn,
        Icon::ArrowUp,
    ];

    pub(crate) fn path(self) -> &'static str {
        match self {
            Icon::Search => "icons/search.svg",
            Icon::Zap => "icons/zap.svg",
            Icon::Bell => "icons/bell.svg",
            Icon::Settings => "icons/settings.svg",
            Icon::Plus => "icons/plus.svg",
            Icon::MessageCircle => "icons/message-circle.svg",
            Icon::Columns => "icons/columns-2.svg",
            Icon::GitBranch => "icons/git-branch.svg",
            Icon::Folder => "icons/folder.svg",
            Icon::X => "icons/x.svg",
            Icon::Rows => "icons/rows-2.svg",
            Icon::Sparkles => "icons/sparkles.svg",
            Icon::CircleAlert => "icons/circle-alert.svg",
            Icon::ChevronRight => "icons/chevron-right.svg",
            Icon::ChevronDown => "icons/chevron-down.svg",
            Icon::File => "icons/file.svg",
            Icon::FileText => "icons/file-text.svg",
            Icon::FileCode => "icons/file-code.svg",
            Icon::Image => "icons/image.svg",
            Icon::FolderOpen => "icons/folder-open.svg",
            Icon::RotateCw => "icons/rotate-cw.svg",
            Icon::Check => "icons/check.svg",
            Icon::Circle => "icons/circle.svg",
            Icon::CircleDot => "icons/circle-dot.svg",
            Icon::LoaderCircle => "icons/loader-circle.svg",
            Icon::Square => "icons/square.svg",
            Icon::FilePen => "icons/file-pen.svg",
            Icon::Terminal => "icons/terminal.svg",
            Icon::Brain => "icons/brain.svg",
            Icon::Globe => "icons/globe.svg",
            Icon::Trash => "icons/trash.svg",
            Icon::Eye => "icons/eye.svg",
            Icon::ShieldAlert => "icons/shield-alert.svg",
            Icon::Wrench => "icons/wrench.svg",
            Icon::ArrowRightLeft => "icons/arrow-right-left.svg",
            Icon::CircleX => "icons/circle-x.svg",
            Icon::LogIn => "icons/log-in.svg",
            Icon::ArrowUp => "icons/arrow-up.svg",
        }
    }
}

/// An icon drawn in `color` at `size`.
pub fn icon(icon: Icon, size: Pixels, color: Hsla) -> Svg {
    svg()
        .path(icon.path())
        .size(size)
        .flex_none()
        .text_color(color)
}

fn shadow(color: Hsla, y: f32, blur: f32) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
        inset: false,
    }
}

fn top_highlight(alpha: f32) -> BoxShadow {
    BoxShadow {
        color: hsla(0., 0., 1., alpha),
        offset: point(px(0.), px(1.)),
        blur_radius: px(0.),
        spread_radius: px(0.),
        inset: true,
    }
}

fn is_light(cx: &App) -> bool {
    cx.tokens().appearance == Appearance::Light
}

/// A floating panel: rounded, filled, with a soft drop shadow.
pub fn panel(fill: Rgb, cx: &App) -> Div {
    let t = cx.tokens();
    let shadow_alpha = if is_light(cx) { 0.08 } else { 0.25 };
    div()
        .flex()
        .flex_col()
        .bg(fill.hsla())
        .rounded(t.metrics.radius_panel)
        .shadow(vec![shadow(hsla(0., 0., 0., shadow_alpha), 4., 14.)])
        .overflow_hidden()
}

/// The raised ("tactile") control surface: a top-lit gradient, a hairline border,
/// an inner top highlight and a small drop shadow.
pub fn raised(div: Div, cx: &App) -> Div {
    let t = cx.tokens();
    let p = &t.palette;
    let (highlight, drop) = if is_light(cx) {
        (0.9, 0.12)
    } else {
        (0.07, 0.35)
    };
    div.bg(linear_gradient(
        180.,
        linear_color_stop(p.raised_top.hsla(), 0.),
        linear_color_stop(p.raised_bottom.hsla(), 1.),
    ))
    .border_1()
    .border_color(p.border_strong.hsla())
    .shadow(vec![
        top_highlight(highlight),
        shadow(hsla(0., 0., 0., drop), 1., 3.),
    ])
}

/// The primary ("go") control: a sage gradient with dark text in the dark
/// appearance and light text in the light one, as on the Send button.
pub fn primary(div: Div, cx: &App) -> Div {
    let t = cx.tokens();
    let p = &t.palette;
    let sage = p.sage.hsla();
    let top = gpui_kit::Hsla {
        l: (sage.l + 0.06).min(1.),
        ..sage
    };
    let (text, highlight, drop) = if is_light(cx) {
        (p.panel_raised.hsla(), 0.2, 0.12)
    } else {
        (p.ground.hsla(), 0.35, 0.4)
    };
    div.bg(linear_gradient(
        180.,
        linear_color_stop(top, 0.),
        linear_color_stop(sage, 1.),
    ))
    .border_1()
    .border_color(gpui_kit::Hsla {
        l: (sage.l - 0.08).max(0.),
        ..sage
    })
    .text_color(text)
    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
    .shadow(vec![
        top_highlight(highlight),
        shadow(hsla(0., 0., 0., drop), 2., 6.),
    ])
}

/// A tab in a tab strip. The selected tab is raised; the others are flat.
pub fn tab(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    tab_frame(id, selected, cx).child(label.into())
}

/// An empty tab, for tabs with their own content (icon, title, close button).
pub fn tab_frame(id: impl Into<ElementId>, selected: bool, cx: &App) -> Stateful<Div> {
    let t = cx.tokens();
    let p = &t.palette;
    let base = div()
        .flex()
        .flex_none()
        .items_center()
        .gap_2()
        .px_3()
        .py(px(6.))
        .rounded(t.metrics.radius_control)
        .text_size(t.metrics.text_sm)
        .whitespace_nowrap();
    let base = if selected {
        raised(base, cx)
            .text_color(p.text.hsla())
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
    } else {
        base.border_1()
            .border_color(gpui_kit::transparent_black())
            .text_color(p.text_muted.hsla())
    };
    base.id(id)
}

/// A key cap such as `Ctrl K`.
pub fn kbd(keys: impl Into<SharedString>, cx: &App) -> Div {
    let t = cx.tokens();
    div()
        .px(px(6.))
        .py(px(2.))
        .rounded(px(6.))
        .bg(t.palette.surface_strong.hsla())
        .text_color(t.palette.text_subtle.hsla())
        .font_family(t.metrics.font_mono.clone())
        .text_size(t.metrics.text_label)
        .child(keys.into())
}

/// A small status dot with a soft halo, as on the `coqod` pill.
pub fn status_dot(color: Rgb, halo: bool) -> Div {
    div()
        .size(px(8.))
        .flex_none()
        .rounded_full()
        .bg(color.hsla())
        .when(halo, |d| {
            d.shadow(vec![BoxShadow {
                color: color.alpha(0.18),
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(3.),
                inset: false,
            }])
        })
}

/// An upper-case section label (`CHATS`, `PLAN`, …).
pub fn section_label(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = cx.tokens();
    let text: SharedString = text.into();
    div()
        .px_2()
        .py_1()
        .text_size(t.metrics.text_label)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(t.palette.text_subtle.hsla())
        .child(SharedString::from(text.to_uppercase()))
}

/// Re-export so views can build elements without naming GPUI's prelude.
pub use gpui_kit::prelude::FluentBuilder;

/// Mark an element so UI tests can find it by id; a no-op outside tests.
pub use gpui_kit::base::TestSupportExt;
