// SPDX-License-Identifier: Apache-2.0

//! The empty workbench frame: design board 1's chrome (top bar, workspace rail,
//! sidebar panel, main tab panel, status bar) with honest empty states. Docking,
//! tabs and real content arrive with the workbench issues (#24 onwards).

use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        App, BoxShadow, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
        ParentElement as _, Render, SharedString, Stateful, Styled as _, Window, div, hsla, point,
        px,
    },
    widgets::{
        Icon, TestSupportExt as _, icon, kbd, panel, raised, section_label, status_dot, tab,
    },
};

/// The root view of a workbench window.
pub struct Shell;

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        div()
            .id("workbench")
            .size_full()
            .flex()
            .flex_col()
            .p(m.gap)
            .gap(m.gap)
            .bg(p.ground.hsla())
            .font_family(m.font_ui.clone())
            .text_size(m.text_md)
            .text_color(p.text.hsla())
            .child(top_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap(m.gap)
                    .child(rail(cx))
                    .child(sidebar(cx))
                    .child(main_panel(cx)),
            )
            .child(status_bar(cx))
    }
}

fn region(id: &'static str, el: Div) -> impl IntoElement {
    el.id(id).test_support()
}

fn shortcut(key: &str) -> SharedString {
    if cfg!(target_os = "macos") {
        format!("⌘{key}").into()
    } else {
        format!("Ctrl {key}").into()
    }
}

fn top_bar(cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);

    let brand = div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(
            div()
                .size(px(18.))
                .rounded_full()
                .border_2()
                .border_color(p.sage.hsla()),
        )
        .child(div().font_weight(FontWeight::SEMIBOLD).child("Coqo"))
        .child(
            div()
                .text_color(p.text_subtle.hsla())
                .child("No workspace open"),
        );

    let search = div()
        .id("command-search")
        .test_support()
        .flex_1()
        .max_w(px(520.))
        .mx_auto()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .py(px(8.))
        .rounded(m.radius_field)
        .bg(p.panel_raised.hsla())
        .border_1()
        .border_color(p.border.hsla())
        .shadow(vec![BoxShadow {
            color: hsla(0., 0., 0., 0.35),
            offset: point(px(0.), px(1.)),
            blur_radius: px(2.),
            spread_radius: px(0.),
            inset: true,
        }])
        .text_size(m.text_sm)
        .text_color(p.text_muted.hsla())
        .child(icon(Icon::Search, px(14.), p.text_muted.hsla()))
        .child(
            div()
                .flex_1()
                .child("Search files, symbols, commands, agents"),
        )
        .child(kbd(shortcut("K"), cx));

    // Nothing needs attention yet, so the bolt is calm (no sand glow).
    let bolt = raised(
        div()
            .flex()
            .items_center()
            .gap_2()
            .px(px(14.))
            .py(px(7.))
            .rounded(m.radius_field)
            .text_size(m.text_sm)
            .text_color(p.text_muted.hsla()),
        cx,
    )
    .id("bolt")
    .test_support()
    .child(icon(Icon::Zap, px(15.), p.text_muted.hsla()))
    .child("Intellisense · off");

    let daemon = div()
        .id("daemon-status")
        .test_support()
        .flex()
        .items_center()
        .gap(px(7.))
        .px(px(12.))
        .py(px(7.))
        .rounded(m.radius_field)
        .bg(p.panel_raised.hsla())
        .text_size(px(12.))
        .text_color(p.text_muted.hsla())
        .child(status_dot(p.text_subtle, false))
        .child("coqod · not connected");

    region(
        "top-bar",
        div()
            .flex()
            .items_center()
            .gap(px(14.))
            .px(px(6.))
            .py(px(2.))
            .child(brand)
            .child(search)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(bolt)
                    .child(daemon),
            ),
    )
}

fn rail_button(id: &'static str, cx: &App) -> Stateful<Div> {
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

fn rail(cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let muted = p.text_muted.hsla();
    region(
        "workspace-rail",
        div()
            .w(m.rail_width)
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.))
            .py(px(6.))
            .child(
                rail_button("add-workspace", cx)
                    .border_1()
                    .border_dashed()
                    .border_color(p.border_strong.hsla())
                    .child(icon(Icon::Plus, px(18.), muted)),
            )
            .child(div().flex_1())
            .child(rail_button("alerts", cx).child(icon(Icon::Bell, px(20.), muted)))
            .child(rail_button("settings", cx).child(icon(Icon::Settings, px(20.), muted))),
    )
}

fn empty_note(text: &'static str, cx: &App) -> Div {
    let t = cx.tokens();
    div()
        .px_2()
        .py_1()
        .text_size(t.metrics.text_sm)
        .text_color(t.palette.text_subtle.hsla())
        .child(text)
}

fn sidebar(cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    region(
        "sidebar",
        panel(p.panel_raised, cx)
            .w(m.sidebar_width)
            .flex_none()
            .child(
                div()
                    .flex()
                    .gap_1()
                    .p(m.gap)
                    .child(tab("sidebar-files", "Files", true, cx))
                    .child(tab("sidebar-git", "Git", false, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(m.gap)
                    .gap_1()
                    .child(section_label("Explorer", cx))
                    .child(empty_note("No folder open", cx)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(m.gap)
                    .pb(px(14.))
                    .gap_1()
                    .child(section_label("Chats", cx))
                    .child(empty_note("No chats yet", cx)),
            ),
    )
}

fn main_panel(cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let tab_strip = div()
        .id("tab-strip")
        .test_support()
        .flex()
        .items_center()
        .gap(px(6.))
        .p(px(8.))
        .min_h(px(48.))
        .child(div().flex_1())
        .child(
            div()
                .id("split")
                .size(px(32.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(m.radius_control)
                .child(icon(Icon::Columns, px(16.), p.text_muted.hsla())),
        );
    let empty =
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(icon(Icon::MessageCircle, px(28.), p.text_subtle.hsla()))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Nothing open yet"),
            )
            .child(div().text_color(p.text_muted.hsla()).child(
                "Open a folder to start a workspace, then chat with an agent or open a file.",
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(m.text_sm)
                    .text_color(p.text_subtle.hsla())
                    .child("Command palette")
                    .child(kbd(shortcut("K"), cx)),
            );
    region(
        "main",
        panel(p.panel, cx)
            .flex_1()
            .min_w_0()
            .child(tab_strip)
            .child(empty),
    )
}

fn status_bar(cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    region(
        "status-bar",
        div()
            .flex()
            .items_center()
            .gap(px(18.))
            .px(px(8.))
            .text_size(m.text_xs)
            .text_color(p.text_muted.hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(icon(Icon::GitBranch, px(12.), p.text_muted.hsla()))
                    .child("No repository"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(status_dot(p.sage, false))
                    .child("0 problems"),
            )
            .child(div().flex_1())
            .child("0 agents"),
    )
}

#[cfg(test)]
mod tests {
    use coqo_ui::{
        Appearance, Palette, Tokens,
        gpui::{AppContext as _, Bounds, Point, WindowBounds, WindowOptions, px, size},
        test::{TestAppContext, TestWindowExt as _},
    };

    use super::Shell;

    fn open(appearance: Appearance) -> (TestAppContext, coqo_ui::gpui::AnyWindowHandle) {
        let cx = TestAppContext::single();
        let handle = cx.update(|cx| {
            coqo_ui::init(appearance, cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1440.), px(900.)),
                })),
                ..Default::default()
            };
            coqo_ui::open_window(options, cx, |_, cx| cx.new(|_| Shell))
                .expect("open shell window")
                .0
        });
        (cx, handle)
    }

    #[test]
    fn renders_board_one_chrome() {
        let (mut cx, handle) = open(Appearance::Dark);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            for id in [
                "top-bar",
                "command-search",
                "bolt",
                "daemon-status",
                "workspace-rail",
                "sidebar",
                "main",
                "tab-strip",
                "status-bar",
            ] {
                let el = window.find(id);
                assert!(el.visible(), "{id} is not visible");
                assert!(el.bounds().size.width > px(0.), "{id} has no width");
            }

            let top = window.find("top-bar").bounds();
            let rail = window.find("workspace-rail").bounds();
            let sidebar = window.find("sidebar").bounds();
            let main = window.find("main").bounds();
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
        })
        .unwrap();
    }

    #[test]
    fn explicit_appearance_selects_its_palette() {
        let (cx, _) = open(Appearance::Light);
        cx.update(|cx| {
            let tokens = cx.global::<Tokens>();
            assert_eq!(tokens.choice, Appearance::Light);
            assert_eq!(tokens.palette, Palette::LIGHT);
        });
        let (cx, _) = open(Appearance::Dark);
        cx.update(|cx| assert_eq!(cx.global::<Tokens>().palette, Palette::DARK));
    }
}

/// Renders the shell offscreen to PNGs for visual review against board 1.
/// Needs a GPU: `cargo test -p coqo -- --ignored shell_screenshots`.
/// Writes to `$COQO_SHOT_DIR` (default: the system temp dir).
#[cfg(test)]
mod screenshots {
    use coqo_ui::{
        Appearance,
        gpui::{AppContext as _, size},
        test::render_offscreen,
    };

    use super::Shell;

    #[test]
    #[ignore = "needs a GPU; run manually"]
    fn shell_screenshots() {
        let dir = std::env::var_os("COQO_SHOT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
            let image = render_offscreen(appearance, size(1440., 900.), |_, cx| cx.new(|_| Shell))
                .expect("offscreen render");
            let path = dir.join(format!("coqo-shell-{name}.png"));
            image.save(&path).expect("save png");
            println!("wrote {}", path.display());
        }
    }
}
