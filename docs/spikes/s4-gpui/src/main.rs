//! Spike S4 (issue #11): can GPUI + gpui-component carry Coqo's editor, popovers and diff?
//!
//! Throwaway code. Run with:
//!   cargo run --release                   # interactive, editor scene
//!   cargo run --release -- --scene diff   # or `zen`
//!   cargo run --release -- --bench        # scripted measurements, prints JSON, exits

mod generate;

use std::{
    cell::RefCell,
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, Theme, ThemeMode,
    button::{Button, ButtonVariants as _},
    diff::{Diff, DiffFile, DiffMode, DiffState},
    h_flex,
    input::{
        Editor, EditorState, GutterDecoration, OverviewRulerDecoration, OverviewRulerLane,
        Position, RangeDecoration, RangeDecorationStyle, TextDecoration,
    },
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::*;

fn extra_cursors() -> u32 {
    std::env::var("SPIKE_CURSORS").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(50).max(2) - 1
}

fn editor_lines() -> usize {
    std::env::var("SPIKE_LINES").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000)
}
const DIFF_LINES: usize = 5_000;

// Zen palette (PLAN §14).
fn hex(rgb: u32) -> Hsla {
    rgb_to_hsla(rgb)
}
fn rgb_to_hsla(rgb: u32) -> Hsla {
    gpui_kit::rgb(rgb).into()
}
const GROUND: u32 = 0x16171A;
const PANEL: u32 = 0x1C1D21;
const PANEL_2: u32 = 0x1F2024;
const SAGE: u32 = 0x9DBF9E;
const SAND: u32 = 0xE2B97F;
const BLUE: u32 = 0x8FB3D9;
const ROSE: u32 = 0xD98E8E;
const LAVENDER: u32 = 0xB9A8D9;
const TEXT: u32 = 0xE6E3DD;
const TEXT_MUTED: u32 = 0xA09D97;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scene {
    Editor,
    Diff,
    Zen,
}

/// Timestamps written by a canvas painted after everything else in the frame.
#[derive(Default)]
struct Marks {
    last_paint: Option<Instant>,
}

#[derive(Default, Debug, Clone)]
struct Samples(Vec<f64>);
impl Samples {
    fn push(&mut self, d: Duration) {
        self.0.push(d.as_secs_f64() * 1000.0);
    }
    fn pct(&self, p: f64) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        let mut v = self.0.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let ix = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
        v[ix]
    }
    fn json(&self) -> String {
        format!(
            "{{\"n\":{},\"p50_ms\":{:.2},\"p95_ms\":{:.2},\"p99_ms\":{:.2},\"max_ms\":{:.2}}}",
            self.0.len(),
            self.pct(50.0),
            self.pct(95.0),
            self.pct(99.0),
            self.pct(100.0)
        )
    }
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<f64>().ok())
        })
        .map(|kb| kb / 1024.0)
        .unwrap_or(f64::NAN)
}

struct Spike {
    scene: Scene,
    editor: Entity<EditorState>,
    diff: Option<Entity<DiffState>>,
    /// Ghost values: byte offset of a line end + the value to show after it.
    ghosts: Vec<(usize, SharedString)>,
    /// The range the anchored question popover points at.
    question_range: Range<usize>,
    marks: Rc<RefCell<Marks>>,
}

impl Spike {
    fn new(scene: Scene, window: &mut Window, cx: &mut Context<Self>, report: &mut Report) -> Self {
        let text = generate::big_rust_file(editor_lines());
        report.editor_bytes = text.len();
        let t = Instant::now();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("rust")
                .line_number(true)
                .indent_guides(true)
                .soft_wrap(false)
                .default_value(text)
        });
        report.editor_construct_ms = ms(t.elapsed());

        let t = Instant::now();
        let (ghosts, question_range) = if std::env::var_os("SPIKE_NO_DECOR").is_some() {
            (Vec::new(), 0..0)
        } else {
            decorate(&editor, cx)
        };
        report.decorations_ms = ms(t.elapsed());

        let mut this = Self {
            scene,
            editor,
            diff: None,
            ghosts,
            question_range,
            marks: Rc::default(),
        };
        if scene == Scene::Diff {
            this.load_diff(cx, report);
        }
        this
    }

    fn load_diff(&mut self, cx: &mut Context<Self>, report: &mut Report) {
        let (patch, old_lines) = generate::big_patch(DIFF_LINES);
        report.diff_old_lines = old_lines;
        report.diff_patch_lines = patch.lines().count();
        let t = Instant::now();
        let files: Vec<DiffFile> = DiffFile::parse(&patch)
            .expect("patch parses")
            .into_iter()
            .map(|f| f.with_language("rust"))
            .collect();
        report.diff_parse_ms = ms(t.elapsed());
        let t = Instant::now();
        let state = cx.new(|cx| DiffState::new(files, cx).with_mode(DiffMode::Split));
        report.diff_construct_ms = ms(t.elapsed());
        self.diff = Some(state);
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.editor.read(cx);
        // Ghost values: overlay text after the line end, positioned from the editor's last layout.
        let ghosts = self.ghosts.iter().filter_map(|(offset, value)| {
            let b = editor.range_to_bounds(&(*offset..*offset))?;
            Some(
                deferred(
                    anchored()
                        .position(point(b.right() + px(16.), b.top()))
                        .child(
                            div()
                                .px_1p5()
                                .rounded(px(6.))
                                .bg(hex(BLUE).opacity(0.12))
                                .text_color(hex(BLUE))
                                .text_size(px(12.))
                                .font_family(cx.theme().mono_font_family.clone())
                                .child(value.clone()),
                        ),
                )
                .with_priority(1),
            )
        });
        let ghosts: Vec<_> = ghosts.collect();

        // Anchored question: a popover pinned under the decorated range.
        let question = editor.range_to_bounds(&self.question_range).map(|b| {
            deferred(
                anchored()
                    .position(point(b.left(), b.bottom() + px(6.)))
                    .snap_to_window()
                    .child(question_card()),
            )
            .with_priority(2)
        });

        div()
            .size_full()
            .child(
                Editor::new(&self.editor)
                    .bordered(false)
                    .h_full()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(13.)),
            )
            .children(ghosts)
            .children(question)
            .into_any_element()
    }

    fn render_diff(&self) -> AnyElement {
        match &self.diff {
            Some(diff) => Diff::new(diff).h_full().into_any_element(),
            None => div().child("diff not loaded").into_any_element(),
        }
    }
}

fn question_card() -> Div {
    v_flex()
        .w(px(360.))
        .gap_2()
        .p_3()
        .rounded(px(12.))
        .bg(hex(PANEL_2))
        .border_1()
        .border_color(hex(SAND).opacity(0.45))
        .shadow(vec![BoxShadow {
            color: hsla(0., 0., 0., 0.45),
            offset: point(px(0.), px(8.)),
            blur_radius: px(24.),
            spread_radius: px(0.),
            inset: false,
        }])
        .child(
            h_flex()
                .gap_2()
                .child(div().size(px(8.)).rounded_full().bg(hex(SAND)))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(hex(SAND))
                        .child("Agent question · compute_42"),
                ),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(hex(TEXT))
                .child("Should totals above 1,000,000 be halved, or capped? The spec only says \"large orders\"."),
        )
        .child(
            h_flex()
                .gap_2()
                .child(raised_button("Halve", SAGE))
                .child(raised_button("Cap", BLUE))
                .child(raised_button("Ask me later", TEXT_MUTED)),
        )
}

/// A "raised" button: subtle top highlight plus a soft drop shadow.
fn raised_button(label: &'static str, accent: u32) -> Stateful<Div> {
    div()
        .id(label)
        .px_3()
        .py_1()
        .rounded(px(8.))
        .bg(hex(0x2A2B30))
        .border_t_1()
        .border_color(hsla(0., 0., 1., 0.08))
        .shadow(vec![BoxShadow {
            color: hsla(0., 0., 0., 0.5),
            offset: point(px(0.), px(2.)),
            blur_radius: px(4.),
            spread_radius: px(0.),
            inset: false,
        }])
        .text_size(px(12.))
        .text_color(hex(accent))
        .hover(|s| s.bg(hex(0x313238)))
        .child(label)
}

/// Adds 500 decorations of four kinds and returns ghost-value and question anchors.
fn decorate(
    editor: &Entity<EditorState>,
    cx: &mut App,
) -> (Vec<(usize, SharedString)>, Range<usize>) {
    let text = editor.read(cx).value().to_string();
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let line_range = |line: usize| {
        let start = line_starts[line];
        let end = line_starts.get(line + 1).map(|e| e - 1).unwrap_or(text.len());
        start..end
    };
    // Half of each kind in the first screenful(s), half spread across the file.
    let spread = |i: usize, n: usize| {
        if i % 2 == 0 {
            (i * 3) % 200
        } else {
            (i * (editor_lines() / n)) % editor_lines()
        }
    };

    let range_decos: Vec<RangeDecoration> = (0..200)
        .map(|i| {
            let r = line_range(spread(i, 200));
            let (style, color) = if i % 3 == 0 {
                (RangeDecorationStyle::Frame, hex(SAND))
            } else {
                (RangeDecorationStyle::Fill, hex(SAGE).opacity(0.10))
            };
            RangeDecoration::new(r).with_style(style).with_color(color)
        })
        .collect();
    let text_decos: Vec<TextDecoration> = (0..150)
        .map(|i| {
            let r = line_range(spread(i, 150) + 1);
            let r = r.start + 4..(r.start + 20).min(r.end);
            TextDecoration::new(
                r,
                HighlightStyle {
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(hex(ROSE)),
                        wavy: true,
                    }),
                    ..Default::default()
                },
            )
        })
        .collect();
    let gutter_decos: Vec<GutterDecoration> = (0..100)
        .map(|i| {
            let line = spread(i, 100);
            let color = [SAGE, BLUE, ROSE][i % 3];
            GutterDecoration::new(line_range(line)).with_color(hex(color))
        })
        .collect();
    let ruler_decos: Vec<OverviewRulerDecoration> = (0..50)
        .map(|i| {
            OverviewRulerDecoration::new(line_range(spread(i, 50)))
                .with_lane(OverviewRulerLane::Right)
                .with_color(hex(LAVENDER))
        })
        .collect();
    editor.update(cx, |state, cx| {
        // Collections live until disposed; the spike never removes them.
        let _ = state.create_range_decorations_collection(range_decos, cx);
        let _ = state.create_decorations_collection(text_decos, cx);
        let _ = state.create_gutter_decorations_collection(gutter_decos, cx);
        let _ = state.create_overview_ruler_decorations_collection(ruler_decos, cx);
    });

    // Ghost values on the assignment lines of the first few functions.
    let ghosts = (0..6)
        .map(|block| {
            let line = block * 16 + 5; // `total = total.wrapping_add(...)`
            let end = line_range(line).end;
            (end, SharedString::from(format!("total = {}", 1_337 * (block + 1))))
        })
        .collect();
    // Question anchored to the `n if n > 1_000_000` arm of block 0.
    let arm = line_range(12);
    let question = arm.start + 8..arm.end;
    (ghosts, question)
}

impl Render for Spike {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let marks = self.marks.clone();
        let body = match self.scene {
            Scene::Editor => self.render_editor(cx),
            Scene::Diff => self.render_diff(),
            Scene::Zen => zen_workbench(self.render_editor(cx)),
        };
        let tab = |label: &'static str, scene: Scene, current: Scene| {
            Button::new(label)
                .label(label)
                .when(scene == current, |b| b.primary())
                .when(scene != current, |b| b.ghost())
                .on_click(move |_, _, cx| {
                    let _ = (scene, cx);
                })
        };
        let current = self.scene;
        v_flex()
            .size_full()
            .bg(hex(GROUND))
            .child(
                h_flex()
                    .gap_2()
                    .p_2()
                    .child(tab("Editor", Scene::Editor, current).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.scene = Scene::Editor;
                            cx.notify();
                        },
                    )))
                    .child(tab("Diff", Scene::Diff, current).on_click(cx.listener(
                        |this, _, _, cx| {
                            if this.diff.is_none() {
                                this.load_diff(cx, &mut Report::default());
                            }
                            this.scene = Scene::Diff;
                            cx.notify();
                        },
                    )))
                    .child(tab("Zen", Scene::Zen, current).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.scene = Scene::Zen;
                            cx.notify();
                        },
                    ))),
            )
            .child(div().flex_1().min_h_0().child(body))
            // Painted last: records when the frame's paint pass finished.
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, _, _| marks.borrow_mut().last_paint = Some(Instant::now()),
                )
                .absolute()
                .size_0(),
            )
    }
}

/// Board 1 chrome in the zen language: rail, files panel, editor panel, chat panel.
fn zen_workbench(editor: AnyElement) -> AnyElement {
    let panel = || {
        v_flex()
            .rounded(px(16.))
            .bg(hex(PANEL))
            .shadow(vec![BoxShadow {
                color: hsla(0., 0., 0., 0.35),
                offset: point(px(0.), px(6.)),
                blur_radius: px(18.),
                spread_radius: px(0.),
            inset: false,
            }])
            .overflow_hidden()
    };
    let file = |name: &'static str, active: bool| {
        div()
            .px_3()
            .py_1()
            .rounded(px(8.))
            .text_size(px(13.))
            .text_color(hex(if active { TEXT } else { TEXT_MUTED }))
            .when(active, |d| d.bg(hex(PANEL_2)))
            .child(name)
    };
    h_flex()
        .size_full()
        .p(px(10.))
        .gap(px(10.))
        .bg(hex(GROUND))
        .child(
            panel()
                .w(px(56.))
                .h_full()
                .items_center()
                .py_3()
                .gap_3()
                .children([SAGE, SAND, BLUE].map(|c| {
                    div()
                        .size(px(32.))
                        .rounded(px(10.))
                        .bg(hex(PANEL_2))
                        .border_t_1()
                        .border_color(hsla(0., 0., 1., 0.06))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(div().size(px(8.)).rounded_full().bg(hex(c)))
                })),
        )
        .child(
            panel()
                .w(px(240.))
                .h_full()
                .p_2()
                .gap_1()
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .text_size(px(11.))
                        .text_color(hex(TEXT_MUTED))
                        .child("CHECKOUT-SERVICE"),
                )
                .child(file("pricing.rs", true))
                .child(file("DiscountService.kt", false))
                .child(file("CheckoutService.java", false))
                .child(file("PricingConfig.kt", false)),
        )
        .child(panel().flex_1().h_full().child(editor))
        .child(
            panel()
                .w(px(340.))
                .h_full()
                .p_3()
                .gap_3()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(hex(TEXT))
                        .child("Chat · Claude"),
                )
                .child(
                    div()
                        .p_3()
                        .rounded(px(12.))
                        .bg(hex(PANEL_2))
                        .text_size(px(13.))
                        .text_color(hex(TEXT_MUTED))
                        .child("I added an overflow guard to compute_*. One question is pinned in pricing.rs."),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(raised_button("Open walkthrough", SAGE))
                        .child(raised_button("Review diff", BLUE)),
                ),
        )
        .into_any_element()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[derive(Default, Debug)]
struct Report {
    editor_bytes: usize,
    editor_construct_ms: f64,
    decorations_ms: f64,
    first_frame_ms: f64,
    first_paint_after_open_ms: f64,
    rss_idle_mb: f64,
    vsync: Samples,
    type_cpu: Samples,
    type_frame: Samples,
    multi_type_cpu: Samples,
    multi_type_frame: Samples,
    cursors: usize,
    y_before: usize,
    cursors_measured: f64,
    scroll_cpu: Samples,
    scroll_interval: Samples,
    rss_after_scroll_mb: f64,
    diff_old_lines: usize,
    diff_patch_lines: usize,
    diff_parse_ms: f64,
    diff_construct_ms: f64,
    diff_first_paint_ms: f64,
    diff_scroll_cpu: Samples,
    diff_scroll_interval: Samples,
    rss_final_mb: f64,
    full_parse_ms: f64,
}

impl Report {
    fn print(&self) {
        println!("{{");
        println!("  \"editor_lines\": {}, \"editor_bytes\": {},", editor_lines(), self.editor_bytes);
        println!("  \"editor_construct_ms\": {:.1},", self.editor_construct_ms);
        println!("  \"decorations_500_ms\": {:.1},", self.decorations_ms);
        println!("  \"launch_to_first_frame_ms\": {:.1},", self.first_frame_ms);
        println!("  \"open_to_first_paint_ms\": {:.1},", self.first_paint_after_open_ms);
        println!("  \"rss_idle_mb\": {:.0},", self.rss_idle_mb);
        println!("  \"vsync_interval\": {},", self.vsync.json());
        println!("  \"type_edit_to_paint\": {},", self.type_cpu.json());
        println!("  \"type_edit_to_next_frame\": {},", self.type_frame.json());
        println!("  \"cursors_requested\": {}, \"cursors_measured\": {:.1},", self.cursors + 1, self.cursors_measured);
        println!("  \"multi_cursor_edit_to_paint\": {},", self.multi_type_cpu.json());
        println!("  \"multi_cursor_edit_to_next_frame\": {},", self.multi_type_frame.json());
        println!("  \"scroll_frame_cpu\": {},", self.scroll_cpu.json());
        println!("  \"scroll_frame_interval\": {},", self.scroll_interval.json());
        println!("  \"rss_after_scroll_mb\": {:.0},", self.rss_after_scroll_mb);
        println!(
            "  \"diff_old_lines\": {}, \"diff_patch_lines\": {},",
            self.diff_old_lines, self.diff_patch_lines
        );
        println!("  \"diff_parse_ms\": {:.1},", self.diff_parse_ms);
        println!("  \"diff_construct_ms\": {:.1},", self.diff_construct_ms);
        println!("  \"diff_open_to_first_paint_ms\": {:.1},", self.diff_first_paint_ms);
        println!("  \"diff_scroll_frame_cpu\": {},", self.diff_scroll_cpu.json());
        println!("  \"diff_scroll_frame_interval\": {},", self.diff_scroll_interval.json());
        println!("  \"tree_sitter_full_parse_ms\": {:.1},", self.full_parse_ms);
        println!("  \"rss_final_mb\": {:.0}", self.rss_final_mb);
        println!("}}");
    }
}

// ---------------------------------------------------------------------------
// Scripted benchmark: a chain of next-frame callbacks.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Step {
    Settle(u32),
    Vsync(u32),
    Type(u32),
    AddCursors(u32),
    MultiType(u32),
    Scroll(u32),
    OpenDiff,
    DiffSettle(u32),
    DiffScroll(u32),
    Done,
}

struct Bench {
    view: Entity<Spike>,
    report: Report,
    step: Step,
    last_frame: Option<Instant>,
    pending_edit: Option<(Instant, bool)>,
    scroll_y: f32,
    diff_open: Option<Instant>,
}

fn run_bench(bench: Rc<RefCell<Bench>>, window: &mut Window, cx: &mut App) {
    let now = Instant::now();
    let mut guard = bench.borrow_mut();
    let b = &mut *guard;
    let view = b.view.clone();
    let marks = view.read(cx).marks.clone();
    let paint = marks.borrow().last_paint;
    let interval = b.last_frame.map(|l| now - l);
    b.last_frame = Some(now);

    // Close out a pending edit measurement from the previous frame.
    if let Some((t0, multi)) = b.pending_edit.take() {
        let frame = now - t0;
        let cpu = paint.filter(|p| *p >= t0).map(|p| p - t0);
        let (cpu_s, frame_s) = if multi {
            (&mut b.report.multi_type_cpu, &mut b.report.multi_type_frame)
        } else {
            (&mut b.report.type_cpu, &mut b.report.type_frame)
        };
        frame_s.push(frame);
        if let Some(cpu) = cpu {
            cpu_s.push(cpu);
        }
    }

    let editor = view.read(cx).editor.clone();
    b.step = match b.step {
        Step::Settle(0) => {
            b.report.rss_idle_mb = rss_mb();
            Step::Vsync(120)
        }
        Step::Settle(n) => Step::Settle(n - 1),
        Step::Vsync(n) => {
            if let Some(i) = interval
                && n < 120
            {
                b.report.vsync.push(i);
            }
            if n == 0 { Step::Type(150) } else { Step::Vsync(n - 1) }
        }
        Step::Type(n) => {
            if n == 150 {
                editor.update(cx, |s, cx| {
                    s.focus(window, cx);
                    s.set_cursor_position(Position::new(40, 4), window, cx)
                });
            }
            let t0 = Instant::now();
            window.dispatch_keystroke(Keystroke::parse("x").unwrap(), cx);
            b.pending_edit = Some((t0, false));
            if n == 0 { Step::AddCursors(extra_cursors()) } else { Step::Type(n - 1) }
        }
        Step::AddCursors(n) => {
            if n == extra_cursors() {
                b.report.y_before = editor.read(cx).value().matches('y').count();
            }
            if n == extra_cursors() {
                let focused = editor.read(cx).focus_handle(cx).is_focused(window);
                eprintln!("editor focused before adding cursors: {focused}");
                if !focused {
                    editor.update(cx, |s, cx| s.focus(window, cx));
                }
            }
            let handled = window.dispatch_keystroke(Keystroke::parse("shift-alt-down").unwrap(), cx);
            if n == extra_cursors() {
                eprintln!("shift-alt-down handled: {handled}");
            }
            b.report.cursors = (extra_cursors() + 1 - n) as usize;
            if n == 1 { Step::MultiType(100) } else { Step::AddCursors(n - 1) }
        }
        Step::MultiType(n) => {
            let t0 = Instant::now();
            // Real keystroke path (input handler), so the edit applies at every cursor.
            window.dispatch_keystroke(Keystroke::parse("y").unwrap(), cx);
            b.pending_edit = Some((t0, true));
            if n == 0 { Step::Scroll(600) } else { Step::MultiType(n - 1) }
        }
        Step::Scroll(n) => {
            if n == 600 {
                let after = editor.read(cx).value().matches('y').count();
                b.report.cursors_measured = (after - b.report.y_before) as f64 / 101.0;
            }
            if n < 600 {
                if let Some(i) = interval {
                    b.report.scroll_interval.push(i);
                }
            }
            if let (Some(p), Some(i)) = (paint, interval) {
                // CPU spent on the previous frame: from that frame's callback to its paint end.
                let prev_cb = now - i;
                if p >= prev_cb {
                    b.report.scroll_cpu.push(p - prev_cb);
                }
            }
            b.scroll_y -= 37.0;
            let y = b.scroll_y;
            editor.update(cx, |s, cx| s.set_scroll_offset(point(px(0.), px(y)), cx));
            if n == 0 {
                b.report.rss_after_scroll_mb = rss_mb();
                Step::OpenDiff
            } else {
                Step::Scroll(n - 1)
            }
        }
        Step::OpenDiff => {
            b.diff_open = Some(Instant::now());
            let mut report = std::mem::take(&mut b.report);
            view.update(cx, |v, cx| {
                v.load_diff(cx, &mut report);
                v.scene = Scene::Diff;
                cx.notify();
            });
            b.report = report;
            Step::DiffSettle(30)
        }
        Step::DiffSettle(n) => {
            if n == 30 {
                if let (Some(t0), Some(p)) = (b.diff_open, paint) {
                    b.report.diff_first_paint_ms = ms(p - t0);
                }
            }
            if n == 0 { Step::DiffScroll(300) } else { Step::DiffSettle(n - 1) }
        }
        Step::DiffScroll(n) => {
            if let (Some(p), Some(i)) = (paint, interval) {
                let prev_cb = now - i;
                if p >= prev_cb && n < 300 {
                    b.report.diff_scroll_cpu.push(p - prev_cb);
                    b.report.diff_scroll_interval.push(i);
                }
            }
            let line = (300 - n as usize) * 12;
            if let Some(diff) = view.read(cx).diff.clone() {
                diff.update(cx, |d, cx| {
                    d.scroll_to_line(
                        gpui_kit::component::diff::DiffLinePosition::new(
                            "src/pricing.rs",
                            gpui_kit::component::diff::DiffSide::Modified,
                            line + 1,
                        ),
                        cx,
                    )
                });
            }
            if n == 0 { Step::Done } else { Step::DiffScroll(n - 1) }
        }
        Step::Done => {
            b.report.rss_final_mb = rss_mb();
            let text = generate::big_rust_file(editor_lines());
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
            let t = Instant::now();
            let tree = parser.parse(&text, None).expect("parse");
            b.report.full_parse_ms = ms(t.elapsed());
            drop(tree);
            b.report.print();
            cx.quit();
            return;
        }
    };
    view.update(cx, |_, cx| cx.notify());
    drop(guard);
    let bench = bench.clone();
    window.on_next_frame(move |window, cx| run_bench(bench, window, cx));
}

fn zen_theme(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let theme = Theme::global_mut(cx);
    theme.background = hex(PANEL);
    theme.foreground = hex(TEXT);
    theme.muted_foreground = hex(TEXT_MUTED);
    theme.border = hex(PANEL_2);
    theme.primary = hex(SAGE);
    theme.popover = hex(PANEL_2);
    theme.mono_font_family = "JetBrainsMono Nerd Font Mono".into();
}

/// Render a scene offscreen (no real window, no screen capture) and save it as a PNG.
fn shot(scene: Scene, out: &str) {
    let mut cx = HeadlessAppContext::with_platform(
        gpui_kit::platform::current_platform(true).text_system(),
        std::sync::Arc::new(gpui_kit::assets::Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        zen_theme(cx);
    });
    let handle = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Default::default(),
                        size: size(px(1440.), px(900.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                move |window, cx| cx.new(|cx| Spike::new(scene, window, cx, &mut Report::default())),
            )
        })
        .expect("open offscreen window");
    let handle: AnyWindowHandle = handle.0.into();
    // Several frames: overlays are positioned from the previous frame's layout.
    // Big files are highlighted on a background thread; give it time (SPIKE_SHOT_WAIT_MS).
    let wait_ms: u64 = std::env::var("SPIKE_SHOT_WAIT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let start = Instant::now();
    loop {
        cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
        if start.elapsed() >= Duration::from_millis(wait_ms.max(1)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
    let image = cx.capture_screenshot(handle).expect("offscreen renderer");
    image.save(out).expect("save png");
    println!("wrote {out} ({}x{})", image.width(), image.height());
}

/// Real window, real executor: wait, then render the window to an image (no screen capture).
fn live_shot(started: Instant, wait: Duration, out: String, window: &mut Window, cx: &mut App) {
    if started.elapsed() < wait {
        window.on_next_frame(move |window, cx| live_shot(started, wait, out, window, cx));
        window.refresh();
        return;
    }
    let image = window.render_to_image().expect("render_to_image");
    image.save(&out).expect("save png");
    println!("wrote {out} ({}x{})", image.width(), image.height());
    cx.quit();
}

fn main() {
    let launch = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let bench = args.iter().any(|a| a == "--bench");
    let scene = match args
        .iter()
        .position(|a| a == "--scene")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
    {
        Some("diff") => Scene::Diff,
        Some("zen") => Scene::Zen,
        _ => Scene::Editor,
    };
    let live = args
        .iter()
        .position(|a| a == "--live-shot")
        .and_then(|i| Some((args.get(i + 1)?.clone(), Duration::from_millis(args.get(i + 2)?.parse().ok()?))));
    if let Some(out) = args.iter().position(|a| a == "--shot").and_then(|i| args.get(i + 1)) {
        shot(scene, out);
        return;
    }

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            zen_theme(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                // GPUI caps unfocused windows at 30 fps; measure the uncapped cadence.
                inactive_frame_interval: None,
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, move |window, cx| {
                let mut report = Report::default();
                let view = cx.new(|cx| Spike::new(scene, window, cx, &mut report));
                let open_start = Instant::now() - Duration::from_secs_f64(report.editor_construct_ms / 1000.0);
                let bench_state = Rc::new(RefCell::new(Bench {
                    view: view.clone(),
                    report,
                    step: Step::Settle(60),
                    last_frame: None,
                    pending_edit: None,
                    scroll_y: 0.0,
                    diff_open: None,
                }));
                window.on_next_frame(move |window, cx| {
                    let marks = bench_state.borrow().view.read(cx).marks.clone();
                    {
                        let mut b = bench_state.borrow_mut();
                        b.report.first_frame_ms = ms(launch.elapsed());
                        if let Some(p) = marks.borrow().last_paint {
                            b.report.first_paint_after_open_ms = ms(p - open_start);
                        }
                    }
                    if bench {
                        run_bench(bench_state, window, cx);
                    } else if let Some((out, wait)) = live.clone() {
                        live_shot(Instant::now(), wait, out, window, cx);
                    }
                });
                view
            })
            .expect("open window");
        });
}
