// SPDX-License-Identifier: Apache-2.0

//! The review model: patches, hunks, line selections, content hashes and
//! viewed marks. The view against a real repository and `coqod` is tested
//! with the Git tab (`git/tests.rs`).

use coqo_protocol::methods::git::{DiffLine, FileDiff, FileSide, Hunk, LineKind, LineSelection};
use coqo_ui::{
    diff::{LineRange, LineRef, Side},
    serde_json::json,
};

use super::{
    patch::{self, content_hash, hunks, selection, to_patch},
    viewed::{Viewed, viewed_key},
};

/// A file of `n` lines `line {i}`, with `changes` applied as
/// `(old line, replacement lines)`: each removes that line and adds the
/// replacements, in one hunk with `context` lines around changes.
pub fn file_diff(path: &str, n: u32, changes: &[(u32, &[&str])], context: u32) -> FileDiff {
    let mut all: Vec<DiffLine> = Vec::new();
    let (mut old, mut new) = (1u32, 1u32);
    while old <= n {
        match changes.iter().find(|(at, _)| *at == old) {
            Some((_, added)) => {
                all.push(line(
                    LineKind::Removed,
                    Some(old),
                    None,
                    &format!("line {old}"),
                ));
                for a in *added {
                    all.push(line(LineKind::Added, None, Some(new), a));
                    new += 1;
                }
            }
            None => {
                all.push(line(
                    LineKind::Context,
                    Some(old),
                    Some(new),
                    &format!("line {old}"),
                ));
                new += 1;
            }
        }
        old += 1;
    }
    // Keep only `context` lines around changes, in git's hunks.
    let changed: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind != LineKind::Context)
        .map(|(i, _)| i)
        .collect();
    let keep = |i: usize| changed.iter().any(|&c| i.abs_diff(c) <= context as usize);
    let mut hunks_out: Vec<Hunk> = Vec::new();
    let mut current: Option<Vec<DiffLine>> = None;
    for (i, l) in all.iter().enumerate() {
        if keep(i) {
            current.get_or_insert_with(Vec::new).push(l.clone());
        } else if let Some(lines) = current.take() {
            hunks_out.push(hunk(lines));
        }
    }
    if let Some(lines) = current.take() {
        hunks_out.push(hunk(lines));
    }
    let additions = all.iter().filter(|l| l.kind == LineKind::Added).count() as u32;
    let deletions = all.iter().filter(|l| l.kind == LineKind::Removed).count() as u32;
    FileDiff {
        path: path.into(),
        old: Some(FileSide {
            mode: 0o100644,
            oid: None,
        }),
        new: Some(FileSide {
            mode: 0o100644,
            oid: None,
        }),
        binary: false,
        hunks: hunks_out,
        additions,
        deletions,
    }
}

fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
    DiffLine {
        kind,
        old_line: old,
        new_line: new,
        text: text.into(),
        newline: true,
    }
}

fn hunk(lines: Vec<DiffLine>) -> Hunk {
    let first_old = lines.iter().find_map(|l| l.old_line).unwrap_or(0);
    let first_new = lines.iter().find_map(|l| l.new_line).unwrap_or(0);
    let old_lines = lines.iter().filter(|l| l.kind != LineKind::Added).count() as u32;
    let new_lines = lines.iter().filter(|l| l.kind != LineKind::Removed).count() as u32;
    Hunk {
        old_start: first_old,
        old_lines,
        new_start: first_new,
        new_lines,
        section: None,
        lines,
    }
}

fn sel(removed: &[u32], added: &[u32]) -> LineSelection {
    LineSelection {
        removed: removed.to_vec(),
        added: added.to_vec(),
    }
}

#[test]
fn a_file_diff_becomes_a_unified_patch() {
    let mut d = file_diff("src/a b.rs", 6, &[(3, &["three", "and a half"])], 1);
    d.hunks[0].section = Some("fn main()".into());
    d.hunks[0].lines.last_mut().unwrap().newline = false;
    // Header counts come from the lines, not from the provider.
    d.hunks[0].old_lines = 99;
    assert_eq!(
        to_patch(&d),
        "diff --git a/src/a b.rs b/src/a b.rs\n\
         --- a/src/a b.rs\n\
         +++ b/src/a b.rs\n\
         @@ -2,3 +2,4 @@ fn main()\n \
         line 2\n\
         -line 3\n\
         +three\n\
         +and a half\n \
         line 4\n\
         \\ No newline at end of file\n"
    );
}

#[test]
fn added_deleted_and_binary_files_get_their_headers() {
    let mut added = file_diff("new.txt", 1, &[(1, &["x"])], 3);
    added.old = None;
    assert!(to_patch(&added).contains("new file mode 100644\n--- /dev/null\n+++ b/new.txt\n"));
    let mut gone = file_diff("old.txt", 1, &[(1, &[])], 3);
    gone.new = None;
    assert!(to_patch(&gone).contains("deleted file mode 100644\n--- a/old.txt\n+++ /dev/null\n"));
    let mut bin = file_diff("logo.png", 1, &[(1, &["x"])], 3);
    bin.binary = true;
    assert!(to_patch(&bin).ends_with("Binary files a/logo.png and b/logo.png differ\n"));
    // Names git quotes stay quoted.
    let tab = file_diff("a\tb", 1, &[(1, &["x"])], 3);
    assert!(to_patch(&tab).starts_with("diff --git \"a/a\\tb\" \"b/a\\tb\"\n"));
}

#[test]
fn hunks_group_changes_the_way_git_does() {
    // Lines 5 and 12 are six unchanged lines apart: one hunk. Line 30 is
    // far away: a second one.
    let d = file_diff(
        "f",
        40,
        &[(5, &["five"]), (12, &["twelve", "twelve b"]), (30, &[])],
        10_000,
    );
    assert_eq!(d.hunks.len(), 1, "full context: one patch hunk");
    let h = hunks(&d);
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].first, LineRef::new("f", Side::Old, 5));
    assert_eq!(h[0].lines, sel(&[5, 12], &[5, 12, 13]));
    assert_eq!(h[1].first, LineRef::new("f", Side::Old, 30));
    assert_eq!(h[1].lines, sel(&[30], &[]));

    // Seven lines apart: two hunks, as with `git diff -U3`.
    let d = file_diff("f", 40, &[(5, &["five"]), (13, &["thirteen"])], 10_000);
    assert_eq!(hunks(&d).len(), 2);
    // And the same hunks from a patch with git's default context.
    let narrow = file_diff("f", 40, &[(5, &["five"]), (13, &["thirteen"])], 3);
    assert_eq!(narrow.hunks.len(), 2);
    assert_eq!(hunks(&narrow), hunks(&d));
    assert_eq!(patch::HUNK_GAP, 6);
}

#[test]
fn an_added_only_hunk_starts_on_the_new_side() {
    // Line 3 replaced by nothing on the old side is a removal; an insertion
    // shows as removed-then-added here, so build one by hand.
    let mut d = file_diff("f", 4, &[], 10_000);
    d.hunks = vec![hunk(vec![
        line(LineKind::Context, Some(1), Some(1), "a"),
        line(LineKind::Added, None, Some(2), "new"),
        line(LineKind::Context, Some(2), Some(3), "b"),
    ])];
    let h = hunks(&d);
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].first, LineRef::new("f", Side::New, 2));
    assert_eq!(h[0].lines, sel(&[], &[2]));
}

#[test]
fn a_selection_takes_the_changed_lines_it_covers() {
    let d = file_diff(
        "f",
        20,
        &[(5, &["five", "five b"]), (8, &["eight"])],
        10_000,
    );
    // Split mode, new column, lines 5..6: only those additions.
    let r = LineRange::lines("f", Side::New, 5, 6);
    assert_eq!(selection(&d, &r), sel(&[], &[5, 6]));
    // Old column, from line 4 (context) to 8: both removals.
    let r = LineRange::lines("f", Side::Old, 4, 8);
    assert_eq!(selection(&d, &r), sel(&[5, 8], &[]));
    // Unified: from removed line 5 to added line 9 (`eight`).
    let r = LineRange {
        path: "f".into(),
        start: (Side::Old, 5),
        end: (Side::New, 9),
    };
    assert_eq!(selection(&d, &r), sel(&[5, 8], &[5, 6, 9]));
    // Backwards ends, and context only.
    let r = LineRange::lines("f", Side::New, 6, 5);
    assert_eq!(selection(&d, &r), sel(&[], &[5, 6]));
    let r = LineRange::lines("f", Side::New, 1, 3);
    assert_eq!(selection(&d, &r), sel(&[], &[]));
    // Lines the diff does not have.
    let r = LineRange::lines("f", Side::New, 400, 401);
    assert_eq!(selection(&d, &r), LineSelection::default());
}

#[test]
fn the_content_hash_follows_the_change_not_the_context() {
    let narrow = file_diff("f", 40, &[(5, &["five"])], 3);
    let wide = file_diff("f", 40, &[(5, &["five"])], 10_000);
    assert_eq!(content_hash(&narrow), content_hash(&wide));
    let other = file_diff("f", 40, &[(5, &["FIVE"])], 3);
    assert_ne!(content_hash(&narrow), content_hash(&other));
    let more = file_diff("f", 40, &[(5, &["five"]), (30, &["thirty"])], 3);
    assert_ne!(content_hash(&narrow), content_hash(&more));
    let renamed = file_diff("g", 40, &[(5, &["five"])], 3);
    assert_ne!(content_hash(&narrow), content_hash(&renamed));
    // Stable across builds: a fixed value.
    assert_eq!(content_hash(&narrow).len(), 16);
    assert_eq!(content_hash(&narrow), content_hash(&narrow.clone()));
}

#[test]
fn viewed_marks_persist_and_reset_when_the_change_does() {
    let mut v = Viewed::default();
    assert_eq!(v.to_value(), json!(null));
    v.set("a.rs", "h1", true);
    v.set("b.rs", "h2", true);
    let stored = v.to_value();
    assert_eq!(stored, json!({"a.rs": "h1", "b.rs": "h2"}));
    let back = Viewed::from_value(Some(&stored));
    assert_eq!(back, v);
    assert!(back.is_viewed("a.rs", "h1"));
    assert!(
        !back.is_viewed("a.rs", "h-new"),
        "a new change is not viewed"
    );

    // a.rs changed, b.rs is not loaded yet, c.rs left the diff.
    v.set("c.rs", "h3", true);
    assert!(v.retain_current([("a.rs", Some("h-new")), ("b.rs", None)]));
    assert_eq!(v.to_value(), json!({"b.rs": "h2"}));
    assert!(!v.retain_current([("b.rs", Some("h2"))]));
    v.set("b.rs", "h2", false);
    assert_eq!(v.len(), 0);

    // Garbage reads as nothing viewed.
    assert_eq!(Viewed::from_value(Some(&json!([1, 2]))), Viewed::default());
    assert_eq!(
        Viewed::from_value(Some(&json!({"a": 1, "b": "h"}))).to_value(),
        json!({"b": "h"})
    );
}

#[test]
fn viewed_keys_are_short_and_per_identity() {
    let a = viewed_key("git:0:index..worktree");
    let b = viewed_key("git:0:HEAD..index");
    assert_ne!(a, b);
    assert!(a.starts_with("ui.diff.viewed."));
    assert!(viewed_key(&"x".repeat(1000)).len() <= 128);
}

// The view, on GPUI's headless platform with a fixed source.

mod view {
    use std::{cell::RefCell, rc::Rc, time::Instant};

    use coqo_protocol::methods::git::FileDiff;
    use coqo_ui::{
        Appearance,
        diff::{DiffMode, GutterMarker, LineRange, LineRef, LineTint, Side, Slot, SlotTarget},
        gpui::{
            AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Entity,
            InteractiveElement as _, IntoElement as _, ParentElement as _, Point, ScrollDelta,
            Styled as _, Window, WindowBounds, WindowOptions, div, point, px, size,
        },
        test::{TestAppContext, TestWindowExt as _},
        widgets::TestSupportExt as _,
    };

    use super::file_diff;
    use crate::diff::{DiffOverlay, DiffView, Status, source::StaticSource};

    struct Harness {
        cx: TestAppContext,
        window: AnyWindowHandle,
        view: Entity<DiffView>,
    }

    fn open(files: Vec<FileDiff>, size_px: (f32, f32)) -> Harness {
        let cx = TestAppContext::single();
        let (window, view) = cx.update(|cx| {
            coqo_ui::init(Appearance::Dark, cx);
            crate::diff::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(size_px.0), px(size_px.1)),
                })),
                ..Default::default()
            };
            coqo_ui::open_window(options, cx, |window, cx| {
                let source = StaticSource {
                    identity: "test".into(),
                    base: "main".into(),
                    head: "topic".into(),
                    files,
                };
                cx.new(|cx| DiffView::new(Rc::new(source), None, window, cx))
            })
            .expect("open window")
        });
        let mut h = Harness { cx, window, view };
        h.settle();
        assert_eq!(
            h.cx.read(|cx| h.view.read(cx).status().clone()),
            Status::Ready
        );
        h
    }

    impl Harness {
        fn frame<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
            self.cx
                .update_window(self.window, |_, window, cx| {
                    window.render_frame(cx);
                    f(window, cx)
                })
                .unwrap()
        }

        /// Let loads and the body's first layouts finish.
        fn settle(&mut self) {
            for _ in 0..4 {
                self.cx.run_until_parked();
                self.frame(|_, _| ());
            }
        }

        fn focus_body(&mut self) {
            let focus = self
                .cx
                .read(|cx| self.view.read(cx).viewer().focus_handle(cx));
            self.frame(|window, cx| window.focus(&focus, cx));
        }

        fn press(&mut self, key: &str) {
            self.frame(|window, cx| window.press(key, cx));
            self.settle();
        }

        fn selected(&mut self) -> Option<String> {
            self.cx
                .read(|cx| self.view.read(cx).selected_file().map(str::to_owned))
        }
    }

    #[test]
    fn keys_move_between_files_and_mark_them_viewed() {
        let mut h = open(
            vec![
                file_diff("a.rs", 30, &[(4, &["four"])], 10_000),
                file_diff("b.rs", 30, &[(9, &["nine"])], 10_000),
            ],
            (1200., 800.),
        );
        assert_eq!(h.selected().as_deref(), Some("a.rs"));
        h.focus_body();
        h.press("shift-j");
        assert_eq!(h.selected().as_deref(), Some("b.rs"));
        h.press("shift-j");
        assert_eq!(
            h.selected().as_deref(),
            Some("b.rs"),
            "no wrap past the end"
        );
        h.press("v");
        assert!(h.cx.read(|cx| h.view.read(cx).is_viewed("b.rs")));
        assert_eq!(h.cx.read(|cx| h.view.read(cx).viewed_count()), (1, 2));
        h.press("shift-k");
        assert_eq!(h.selected().as_deref(), Some("a.rs"));
        h.press("m");
        assert_eq!(h.cx.read(|cx| h.view.read(cx).mode(cx)), DiffMode::Unified);
        // Hunk keys move the body; they need not change the file.
        h.press("j");
        h.press("k");
        assert_eq!(h.selected().as_deref(), Some("a.rs"));
        // The file list: clicking a row shows it, its checkbox marks it.
        h.frame(|window, cx| window.click(("diff-file", 1usize), cx));
        assert_eq!(h.selected().as_deref(), Some("b.rs"));
        h.frame(|window, cx| window.click(("diff-viewed", 0usize), cx));
        assert_eq!(h.cx.read(|cx| h.view.read(cx).viewed_count()), (2, 2));
        assert_eq!(
            h.selected().as_deref(),
            Some("b.rs"),
            "the checkbox only marks"
        );
        // A fixed source cannot stage: no hunk buttons.
        h.settle();
        assert!(h.frame(|window, _| window.try_find(("diff-stage-hunk", 0usize)).is_none()));
    }

    /// An overlay like the ones #94 and #100 add.
    #[derive(Default)]
    struct Notes {
        added: RefCell<Vec<LineRange>>,
    }

    impl DiffOverlay for Notes {
        fn markers(&self, path: &str, _: &FileDiff, _: &App) -> Vec<GutterMarker> {
            vec![GutterMarker {
                id: ("note-marker", 0usize).into(),
                at: LineRef::new(path.to_owned(), Side::New, 20),
                label: "Step 1".into(),
            }]
        }

        fn render_marker(&self, marker: &GutterMarker, _: &mut Window, _: &mut App) -> AnyElement {
            div()
                .id(marker.id.clone())
                .test_support()
                .size_full()
                .child("1")
                .into_any_element()
        }

        fn slots(&self, path: &str, _: &FileDiff, _: &App) -> Vec<Slot> {
            vec![Slot {
                id: ("note", 0usize).into(),
                at: SlotTarget::Line(LineRef::new(path.to_owned(), Side::New, 20)),
            }]
        }

        fn render_slot(&self, slot: &Slot, _: &mut Window, _: &mut App) -> AnyElement {
            div()
                .id(slot.id.clone())
                .test_support()
                .child("A comment thread")
                .into_any_element()
        }

        fn tints(&self, path: &str, _: &FileDiff, _: &App) -> Vec<LineTint> {
            vec![LineTint {
                range: LineRange::lines(path.to_owned(), Side::New, 19, 21),
                color: coqo_ui::gpui::hsla(0.1, 0.5, 0.5, 0.2),
            }]
        }

        fn wants_add(&self) -> bool {
            true
        }

        fn on_add(&self, range: &LineRange, _: &mut Window, _: &mut App) {
            self.added.borrow_mut().push(range.clone());
        }
    }

    #[test]
    fn overlays_put_markers_and_slots_on_lines() {
        let mut h = open(
            vec![
                file_diff("a.rs", 40, &[(20, &["twenty"])], 10_000),
                file_diff("b.rs", 40, &[(3, &["three"])], 10_000),
            ],
            (1200., 800.),
        );
        let notes = Rc::new(Notes::default());
        h.view
            .update(&mut h.cx, |v, cx| v.add_overlay(notes.clone(), cx));
        h.settle();
        assert!(h.frame(|window, _| window.try_find(("note-marker", 0usize)).is_some()));
        assert!(h.frame(|window, _| window.try_find(("note", 0usize)).is_some()));
        assert!(notes.added.borrow().is_empty());

        // A line address shows its file and line (a walkthrough step).
        let at = LineRef::new("b.rs", Side::New, 3);
        h.view.update(&mut h.cx, |v, cx| v.scroll_to(&at, cx));
        h.settle();
        assert_eq!(h.selected().as_deref(), Some("b.rs"));
        assert!(h.cx.read(|cx| h.view.read(cx).viewer().has_line(&at, cx)));
    }

    /// PLAN S4 / ADR 0002: a 5,000-line file side by side, with a hundred
    /// hunks, scrolls within a frame budget.
    #[test]
    fn a_five_thousand_line_diff_scrolls_side_by_side() {
        let changes: Vec<(u32, Vec<String>)> = (0..100)
            .map(|i| {
                let at = 20 + i * 49;
                (
                    at,
                    vec![format!("changed {at}"), format!("added after {at}")],
                )
            })
            .collect();
        let refs: Vec<(u32, Vec<&str>)> = changes
            .iter()
            .map(|(at, v)| (*at, v.iter().map(String::as_str).collect()))
            .collect();
        let refs: Vec<(u32, &[&str])> = refs.iter().map(|(at, v)| (*at, v.as_slice())).collect();
        let big = file_diff("big.rs", 5_000, &refs, 10_000);
        assert!(big.additions >= 200);
        let mut h = open(vec![big], (1440., 900.));
        assert_eq!(h.cx.read(|cx| h.view.read(cx).mode(cx)), DiffMode::Split);
        assert_eq!(h.cx.read(|cx| h.view.read(cx).hunks().len()), 100);
        // Open every fold: all 5,000 lines are rows.
        let viewer = h.cx.read(|cx| h.view.read(cx).viewer().clone());
        h.cx.update(|cx| viewer.expand_all(cx));
        h.settle();
        let mut frames = Vec::new();
        for _ in 0..120 {
            let start = Instant::now();
            h.frame(|window, cx| {
                window.scroll(
                    "diff-body",
                    ScrollDelta::Pixels(point(px(0.), px(-240.))),
                    cx,
                )
            });
            frames.push(start.elapsed());
        }
        frames.sort();
        let p50 = frames[frames.len() / 2];
        let p99 = frames[frames.len() * 99 / 100];
        println!("5k-line split diff scroll: p50 {p50:?}, p99 {p99:?} per frame");
        // The budget is for optimized builds; debug builds only catch the
        // pathological (a frame per line, a relayout per frame).
        let budget = if cfg!(debug_assertions) { 250 } else { 16 };
        assert!(
            p99.as_millis() <= budget,
            "p99 {p99:?} over {budget} ms (p50 {p50:?})"
        );
        let last = LineRef::new("big.rs", Side::New, 5_100);
        assert!(h.cx.read(|cx| viewer.has_line(&last, cx)));
    }

    /// A file diff with full context, from two texts.
    fn from_texts(path: &str, old: Option<&str>, new: Option<&str>) -> FileDiff {
        use coqo_protocol::methods::git::{DiffLine, FileSide, Hunk, LineKind};
        let diff = similar::TextDiff::from_lines(old.unwrap_or(""), new.unwrap_or(""));
        let mut lines = Vec::new();
        for change in diff.iter_all_changes() {
            let kind = match change.tag() {
                similar::ChangeTag::Equal => LineKind::Context,
                similar::ChangeTag::Delete => LineKind::Removed,
                similar::ChangeTag::Insert => LineKind::Added,
            };
            lines.push(DiffLine {
                kind,
                old_line: change.old_index().map(|i| i as u32 + 1),
                new_line: change.new_index().map(|i| i as u32 + 1),
                text: change.value().trim_end_matches('\n').to_owned(),
                newline: true,
            });
        }
        let count = |k: LineKind| lines.iter().filter(|l| l.kind == k).count() as u32;
        let (additions, deletions) = (count(LineKind::Added), count(LineKind::Removed));
        let side = |t: Option<&str>| {
            t.map(|_| FileSide {
                mode: 0o100644,
                oid: None,
            })
        };
        FileDiff {
            path: path.into(),
            old: side(old),
            new: side(new),
            binary: false,
            hunks: vec![Hunk {
                old_start: u32::from(old.is_some()),
                old_lines: count(LineKind::Context) + deletions,
                new_start: u32::from(new.is_some()),
                new_lines: count(LineKind::Context) + additions,
                section: None,
                lines,
            }],
            additions,
            deletions,
        }
    }

    const DISCOUNT_OLD: &str = "\
package com.acme.checkout.pricing

import java.math.BigDecimal

class DiscountService(
    private val config: PricingConfig,
    private val loyalty: LoyaltyClient,
    private val clock: Clock,
) {
    fun volumeDiscount(order: Order): BigDecimal {
        return BigDecimal.ZERO  // TODO ACME-1423
    }

    fun seasonalDiscount(order: Order): BigDecimal {
        val month = clock.today().monthValue
        return if (month in config.saleMonths) config.saleRate else BigDecimal.ZERO
    }
}
";

    const DISCOUNT_NEW: &str = "\
package com.acme.checkout.pricing

import java.math.BigDecimal

class DiscountService(
    private val config: PricingConfig,
    private val loyalty: LoyaltyClient,
    private val clock: Clock,
) {
    fun volumeDiscount(order: Order): BigDecimal {
        val units = order.lines.sumOf { it.quantity }
        if (units < config.volumeThreshold) return BigDecimal.ZERO

        var rate = config.volumeRate
        if (loyalty.isMember(order.customerId)) {
            rate += config.loyaltyBonus
        }
        val discount = order.subtotal * rate
        return discount.min(config.maxDiscount)
    }

    fun seasonalDiscount(order: Order): BigDecimal {
        val month = clock.today().monthValue
        return if (month in config.saleMonths) config.saleRate else BigDecimal.ZERO
    }
}
";

    fn board_sample() -> Vec<FileDiff> {
        let config_old = "data class PricingConfig(\n    val saleMonths: Set<Int>,\n    val saleRate: BigDecimal,\n)\n";
        let config_new = "data class PricingConfig(\n    val saleMonths: Set<Int>,\n    val saleRate: BigDecimal,\n    val volumeThreshold: Int = 10,\n    val volumeRate: BigDecimal = BigDecimal(\"0.05\"),\n    val loyaltyBonus: BigDecimal = BigDecimal(\"0.02\"),\n    val maxDiscount: BigDecimal = BigDecimal(\"50.00\"),\n)\n";
        let checkout_old = "public final class CheckoutService {\n    public Receipt checkout(Order order) {\n        BigDecimal discount = BigDecimal.ZERO;\n        return receipts.issue(order, discount);\n    }\n}\n";
        let checkout_new = "public final class CheckoutService {\n    public Receipt checkout(Order order) {\n        BigDecimal discount = discounts.volumeDiscount(order);\n        return receipts.issue(order, discount);\n    }\n}\n";
        let test_new = "class DiscountServiceTest {\n    @Test\n    fun `ten units get the volume rate`() {\n        val service = DiscountService(config(), members(), fixedClock())\n        assertEquals(BigDecimal(\"5.00\"), service.volumeDiscount(order(units = 10)))\n    }\n}\n";
        let pom_old = "<project>\n  <dependencies>\n  </dependencies>\n</project>\n";
        let pom_new = "<project>\n  <dependencies>\n    <dependency>\n      <groupId>org.junit.jupiter</groupId>\n      <artifactId>junit-jupiter</artifactId>\n      <scope>test</scope>\n    </dependency>\n  </dependencies>\n</project>\n";
        let k = "checkout-core/src/main/kotlin/com/acme/checkout/pricing";
        vec![
            from_texts(
                &format!("{k}/DiscountService.kt"),
                Some(DISCOUNT_OLD),
                Some(DISCOUNT_NEW),
            ),
            from_texts(
                &format!("{k}/PricingConfig.kt"),
                Some(config_old),
                Some(config_new),
            ),
            from_texts(
                "checkout-core/src/main/java/com/acme/checkout/CheckoutService.java",
                Some(checkout_old),
                Some(checkout_new),
            ),
            from_texts(
                "checkout-core/src/test/kotlin/com/acme/checkout/pricing/DiscountServiceTest.kt",
                None,
                Some(test_new),
            ),
            from_texts("pom.xml", Some(pom_old), Some(pom_new)),
        ]
    }

    #[test]
    fn the_board_sample_loads() {
        let mut h = open(board_sample(), (1440., 900.));
        let v = h.cx.read(|cx| {
            let v = h.view.read(cx);
            (v.files().len(), v.hunks().len())
        });
        assert_eq!(v, (5, 1));
        h.settle();
    }

    /// The review surface offscreen, for visual review against board 4.
    /// Needs a GPU:
    /// `COQO_SHOT_DIR=<dir> cargo test -p coqo -- --ignored diff_screenshots`.
    #[test]
    #[ignore = "needs a GPU; run manually"]
    fn diff_screenshots() {
        use coqo_ui::{
            ActiveTokens as _,
            gpui::{Context, IntoElement, Render},
            test::render_offscreen,
        };

        struct Root(Entity<DiffView>);
        impl Render for Root {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .bg(cx.tokens().palette.ground.hsla())
                    .font_family(cx.tokens().metrics.font_ui.clone())
                    .child(self.0.clone())
            }
        }

        let dir = std::env::var_os("COQO_SHOT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        for (name, appearance, mode) in [
            ("dark", Appearance::Dark, DiffMode::Split),
            ("light", Appearance::Light, DiffMode::Split),
            ("unified-dark", Appearance::Dark, DiffMode::Unified),
        ] {
            let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
                crate::diff::init(cx);
                let source = StaticSource {
                    identity: "screenshot".into(),
                    base: "main".into(),
                    head: "feature/volume-discount".into(),
                    files: board_sample(),
                };
                let (list, diffs) = (source.list(), source.files.clone());
                let view = cx.new(|cx| DiffView::new(Rc::new(source), None, window, cx));
                // Load now (the source would answer on the next tick), then
                // mark two files viewed.
                view.update(cx, |v, cx| {
                    v.loaded(list, diffs.into_iter().map(Ok).collect(), cx);
                    v.set_mode(mode, cx);
                    let paths: Vec<String> = v.files().iter().map(|f| f.path.clone()).collect();
                    v.set_viewed(&paths[1], true, cx);
                    v.set_viewed(&paths[3], true, cx);
                });
                cx.new(|_| Root(view))
            })
            .expect("offscreen render");
            let path = dir.join(format!("coqo-diff-{name}.png"));
            image.save(&path).expect("save png");
            println!("wrote {}", path.display());
        }
    }
}
