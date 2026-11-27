// SPDX-License-Identifier: Apache-2.0

//! Golden tests: anchors relocated without history after realistic
//! refactors, in Rust, TypeScript, Java and Kotlin.

use coqo_anchors::{Anchor, Snapshot, Status};
use coqo_syntax::{Document, Language};

fn parse(path: &str, text: &str) -> Document {
    let lang = Language::detect(path, text).expect("a bundled language");
    Document::new(lang, text)
}

/// Anchor the first occurrence of `needle` in `before`.
fn anchor(path: &str, before: &str, needle: &str) -> Anchor {
    let doc = parse(path, before);
    let snap = Snapshot::from_document(&doc);
    let start = before.find(needle).expect("needle in the text");
    Anchor::new(path, &snap, start..start + needle.len()).unwrap()
}

/// Anchor the whole lines containing `needle` (first occurrence).
fn anchor_lines(path: &str, before: &str, needle: &str) -> Anchor {
    let doc = parse(path, before);
    let snap = Snapshot::from_document(&doc);
    let start = before.find(needle).expect("needle in the text");
    let first = snap.line_of(start);
    let last = snap.line_of(start + needle.len() - 1);
    Anchor::lines(path, &snap, first..=last).unwrap()
}

/// Resolve without history; the status and the text now at the range.
fn resolve(path: &str, after: &str, anchor: &Anchor) -> (Status, Option<String>) {
    let doc = parse(path, after);
    let snap = Snapshot::from_document(&doc);
    let r = snap.resolve(anchor, None);
    (r.status, r.bytes().map(|b| after[b].to_owned()))
}

fn assert_moved(path: &str, after: &str, anchor: &Anchor) {
    let (status, text) = resolve(path, after, anchor);
    assert_eq!(status, Status::Moved, "{text:?}");
    assert_eq!(text.as_deref(), Some(anchor.text.as_str()));
}

fn assert_fuzzy(path: &str, after: &str, anchor: &Anchor, expected: &str) {
    let (status, text) = resolve(path, after, anchor);
    assert!(
        matches!(status, Status::Fuzzy { confidence } if confidence >= coqo_anchors::MIN_CONFIDENCE),
        "{status:?} {text:?}"
    );
    assert_eq!(text.as_deref(), Some(expected));
}

fn assert_orphaned(path: &str, after: &str, anchor: &Anchor) {
    let (status, text) = resolve(path, after, anchor);
    assert_eq!(status, Status::Orphaned, "{text:?}");
    assert_eq!(text, None);
}

// ---- Rust -------------------------------------------------------------------

const RS: &str = "src/orders.rs";

const RUST: &str = r#"use std::collections::HashMap;

/// Totals per customer.
pub fn totals(orders: &[Order]) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    for o in orders {
        *out.entry(o.customer.clone()).or_insert(0) += o.amount;
    }
    out
}

pub fn largest(orders: &[Order]) -> Option<&Order> {
    orders.iter().max_by_key(|o| o.amount)
}

impl Order {
    pub fn is_big(&self) -> bool {
        self.amount > 1_000
    }
}
"#;

const LARGEST: &str = "pub fn largest(orders: &[Order]) -> Option<&Order> {\n    orders.iter().max_by_key(|o| o.amount)\n}\n";

#[test]
fn rust_unchanged_is_exact() {
    let a = anchor_lines(RS, RUST, "*out.entry");
    let (status, text) = resolve(RS, RUST, &a);
    assert_eq!(status, Status::Exact);
    assert_eq!(text.as_deref(), Some(a.text.as_str()));
    assert_eq!(a.symbol.as_ref().unwrap().path, ["totals"]);
}

#[test]
fn rust_lines_inserted_above() {
    let a = anchor_lines(RS, RUST, "*out.entry");
    let after = RUST.replace(
        "use std::collections::HashMap;\n",
        "use std::collections::HashMap;\nuse std::fmt;\n\nconst LIMIT: u64 = 10;\n",
    );
    assert_moved(RS, &after, &a);
}

#[test]
fn rust_function_moved() {
    let a = anchor(RS, RUST, LARGEST);
    let after = format!("{}\n{LARGEST}", RUST.replace(&format!("{LARGEST}\n"), ""));
    assert_moved(RS, &after, &a);
    // A line inside it moves along.
    let a = anchor_lines(RS, RUST, "max_by_key");
    assert_moved(RS, &after, &a);
}

#[test]
fn rust_function_renamed() {
    let a = anchor(
        RS,
        RUST,
        "pub fn totals(orders: &[Order]) -> HashMap<String, u64> {",
    );
    let after = RUST.replace("fn totals(", "fn totals_by_customer(");
    assert_fuzzy(
        RS,
        &after,
        &a,
        "pub fn totals_by_customer(orders: &[Order]) -> HashMap<String, u64> {",
    );
    // Just the name: the line is found, and the name within it.
    let a = anchor(RS, RUST, "totals");
    assert_fuzzy(RS, &after, &a, "totals_by_customer");
}

#[test]
fn rust_body_reformatted() {
    let a = anchor(RS, RUST, LARGEST);
    let reformatted = "pub fn largest(orders: &[Order]) -> Option<&Order> {\n    orders\n        .iter()\n        .max_by_key(|o| o.amount)\n}\n";
    let after = RUST.replace(LARGEST, reformatted);
    assert_fuzzy(RS, &after, &a, reformatted);
}

#[test]
fn rust_line_edited_in_place() {
    let a = anchor_lines(RS, RUST, "self.amount > 1_000");
    let after = RUST.replace("self.amount > 1_000", "self.amount > 10_000");
    assert_fuzzy(RS, &after, &a, "        self.amount > 10_000\n");
}

#[test]
fn rust_anchored_line_deleted() {
    let a = anchor_lines(RS, RUST, "*out.entry");
    let after = RUST.replace(
        "        *out.entry(o.customer.clone()).or_insert(0) += o.amount;\n",
        "",
    );
    assert_orphaned(RS, &after, &a);
}

#[test]
fn rust_function_deleted() {
    let a = anchor(RS, RUST, LARGEST);
    let after = RUST.replace(&format!("{LARGEST}\n"), "");
    assert_orphaned(RS, &after, &a);
}

// ---- TypeScript -------------------------------------------------------------

const TS: &str = "src/cart.ts";

const TYPESCRIPT: &str = r#"import { Item } from "./item";

export class Cart {
  private items: Item[] = [];

  add(item: Item): void {
    this.items.push(item);
  }

  total(): number {
    return this.items.reduce((sum, i) => sum + i.price * i.qty, 0);
  }

  clear(): void {
    this.items = [];
  }
}
"#;

const TOTAL: &str = "  total(): number {\n    return this.items.reduce((sum, i) => sum + i.price * i.qty, 0);\n  }\n";

#[test]
fn ts_method_moved_within_class() {
    let a = anchor(TS, TYPESCRIPT, TOTAL);
    assert_eq!(a.symbol.as_ref().unwrap().path, ["Cart", "total"]);
    let clear = "  clear(): void {\n    this.items = [];\n  }\n";
    let after = TYPESCRIPT
        .replace(&format!("{TOTAL}\n"), "")
        .replace(clear, &format!("{clear}\n{TOTAL}"));
    assert!(after.find("clear()") < after.find("total()"));
    assert_moved(TS, &after, &a);
}

#[test]
fn ts_lines_inserted_above_and_inside() {
    let a = anchor_lines(TS, TYPESCRIPT, "return this.items.reduce");
    let after = TYPESCRIPT.replace(
        "  private items: Item[] = [];\n",
        "  private items: Item[] = [];\n  private discount = 0;\n\n  setDiscount(d: number): void {\n    this.discount = d;\n  }\n",
    );
    assert_moved(TS, &after, &a);
}

#[test]
fn ts_method_renamed() {
    let a = anchor(TS, TYPESCRIPT, TOTAL);
    let after = TYPESCRIPT.replace("total(): number", "subtotal(): number");
    assert_fuzzy(TS, &after, &a, &TOTAL.replace("total()", "subtotal()"));
}

#[test]
fn ts_anchored_line_deleted() {
    let a = anchor_lines(TS, TYPESCRIPT, "this.items.push(item);");
    let after = TYPESCRIPT.replace("    this.items.push(item);\n", "");
    assert_orphaned(TS, &after, &a);
}

// ---- Java -------------------------------------------------------------------

const JV: &str = "src/main/java/shop/Inventory.java";

const JAVA: &str = r#"package shop;

import java.util.HashMap;
import java.util.Map;

public class Inventory {
    private final Map<String, Integer> stock = new HashMap<>();

    public void receive(String sku, int qty) {
        stock.merge(sku, qty, Integer::sum);
    }

    public boolean reserve(String sku, int qty) {
        int have = stock.getOrDefault(sku, 0);
        if (have < qty) {
            return false;
        }
        stock.put(sku, have - qty);
        return true;
    }
}
"#;

const RESERVE: &str = "    public boolean reserve(String sku, int qty) {\n        int have = stock.getOrDefault(sku, 0);\n        if (have < qty) {\n            return false;\n        }\n        stock.put(sku, have - qty);\n        return true;\n    }\n";

#[test]
fn java_method_renamed() {
    let a = anchor(JV, JAVA, RESERVE);
    assert_eq!(a.symbol.as_ref().unwrap().path, ["Inventory", "reserve"]);
    let after = JAVA.replace("boolean reserve(", "boolean tryReserve(");
    assert_fuzzy(JV, &after, &a, &RESERVE.replace("reserve(", "tryReserve("));
}

#[test]
fn java_method_moved_above_another() {
    let a = anchor_lines(JV, JAVA, "stock.put(sku, have - qty);");
    let receive = "    public void receive(String sku, int qty) {\n        stock.merge(sku, qty, Integer::sum);\n    }\n\n";
    let after = JAVA.replace(receive, "").replace(
        "    }\n}\n",
        &format!(
            "    }}\n\n{}}}\n",
            receive.trim_end_matches('\n').to_owned() + "\n"
        ),
    );
    assert_moved(JV, &after, &a);
}

#[test]
fn java_body_edited_in_place() {
    let a = anchor(JV, JAVA, RESERVE);
    let edited = RESERVE.replace(
        "        if (have < qty) {\n            return false;\n        }\n",
        "        if (have < qty) {\n            log.warn(\"short of {}\", sku);\n            return false;\n        }\n",
    );
    let after = JAVA.replace(RESERVE, &edited);
    assert_fuzzy(JV, &after, &a, &edited);
}

#[test]
fn java_anchored_line_deleted() {
    let a = anchor_lines(JV, JAVA, "stock.put(sku, have - qty);");
    let after = JAVA.replace("        stock.put(sku, have - qty);\n", "");
    assert_orphaned(JV, &after, &a);
}

// ---- Kotlin -----------------------------------------------------------------

const KT: &str = "src/main/kotlin/shop/Pricing.kt";

const KOTLIN: &str = r#"package shop

object Pricing {
    fun price(base: Double, qty: Int): Double {
        val gross = base * qty
        return if (qty >= 10) gross * 0.9 else gross
    }

    fun tax(amount: Double): Double = amount * 0.25
}
"#;

#[test]
fn kotlin_body_reformatted() {
    let line = "        return if (qty >= 10) gross * 0.9 else gross\n";
    let a = anchor(KT, KOTLIN, line);
    let reformatted = "        return if (qty >= 10) {\n            gross * 0.9\n        } else {\n            gross\n        }\n";
    let after = KOTLIN.replace(line, reformatted);
    assert_fuzzy(KT, &after, &a, reformatted);
}

#[test]
fn kotlin_lines_inserted_above() {
    let a = anchor(
        KT,
        KOTLIN,
        "fun tax(amount: Double): Double = amount * 0.25",
    );
    assert_eq!(a.symbol.as_ref().unwrap().path, ["Pricing", "tax"]);
    let after = KOTLIN.replace(
        "object Pricing {\n",
        "object Pricing {\n    private const val VERSION = 2\n\n",
    );
    assert_moved(KT, &after, &a);
}

#[test]
fn kotlin_anchored_line_deleted() {
    let a = anchor_lines(KT, KOTLIN, "val gross = base * qty");
    let after = KOTLIN.replace("        val gross = base * qty\n", "");
    assert_orphaned(KT, &after, &a);
}

// ---- ambiguity --------------------------------------------------------------

#[test]
fn identical_copies_are_not_guessed() {
    // `a` was removed, and the anchored line now exists twice, neither
    // where it was, in places that fit its context equally well.
    let before = "// top\n\nfn a() {\n    check();\n}\n\n// end\n";
    let a = anchor_lines("src/a.rs", before, "check();");
    let after = "use x;\n// top\n\nfn b() {\n    check();\n}\n\n// end\n// top\n\nfn c() {\n    check();\n}\n\n// end\n";
    assert_orphaned("src/a.rs", after, &a);
}

#[test]
fn context_picks_the_right_copy() {
    let before = "fn a() {\n    let x = load();\n    check(x);\n}\n\nfn b() {\n    let y = load();\n    check(y);\n}\n";
    let a = anchor_lines("src/a.rs", before, "let y = load();");
    let after = format!("// header\n\n{before}");
    let (status, text) = resolve("src/a.rs", &after, &a);
    assert_eq!(status, Status::Moved);
    assert_eq!(text.as_deref(), Some("    let y = load();\n"));
    // `check(` occurs four times; the context tells them apart.
    let a = anchor("src/a.rs", before, "check(y)");
    let (status, text) = resolve("src/a.rs", &after, &a);
    assert_eq!(status, Status::Moved);
    let at = after.find("check(y)").unwrap();
    assert_eq!(text.as_deref(), Some(&after[at..at + 8]));
}
