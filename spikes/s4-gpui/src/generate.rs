//! Synthetic inputs for the spike: a large Rust file and a large patch.

use std::fmt::Write as _;

/// One 16-line block of plausible Rust, so tree-sitter has real work to do.
fn block(out: &mut String, i: usize) {
    let _ = write!(
        out,
        "/// Computes checksum variant {i} for the pricing pipeline.
pub fn compute_{i}(input: &[u64], factor: u64) -> Result<u64, String> {{
    let mut total: u64 = 0;
    for (idx, value) in input.iter().enumerate() {{
        if idx % 3 == 0 {{
            total = total.wrapping_add(value * factor);
        }} else {{
            total ^= value.rotate_left(idx as u32 % 7);
        }}
    }}
    match total {{
        0 => Err(format!(\"empty input at {i}\")),
        n if n > 1_000_000 => Ok(n / 2),
        n => Ok(n),
    }}
}}
"
    );
}

/// A Rust source file with exactly `lines` lines (rounded up to a whole block).
pub fn big_rust_file(lines: usize) -> String {
    let mut out = String::with_capacity(lines * 48);
    for i in 0..lines.div_ceil(16) {
        block(&mut out, i);
    }
    out
}

/// A unified patch for a `lines`-line file with a change every ~25 lines,
/// three inserted lines every 100 and two deleted lines every 150.
pub fn big_patch(lines: usize) -> (String, usize) {
    let old = big_rust_file(lines);
    let mut new = String::with_capacity(old.len() + old.len() / 10);
    for (n, line) in old.lines().enumerate() {
        if n % 150 == 7 || n % 150 == 8 {
            continue; // deleted
        }
        if n % 25 == 3 {
            let _ = writeln!(new, "{}", line.replace("total", "running_total"));
        } else {
            let _ = writeln!(new, "{line}");
        }
        if n % 100 == 50 {
            let _ = writeln!(new, "    // Added by the agent: guard against overflow ({n}).");
            let _ = writeln!(new, "    debug_assert!(factor < u64::MAX / 2);");
            let _ = writeln!(new, "    tracing::trace!(factor, \"computing\");");
        }
    }
    let diff = similar::TextDiff::from_lines(&old, &new);
    let mut patch = String::from("diff --git a/src/pricing.rs b/src/pricing.rs\n");
    let _ = write!(
        patch,
        "{}",
        diff.unified_diff()
            .context_radius(3)
            .header("a/src/pricing.rs", "b/src/pricing.rs")
    );
    let old_lines = old.lines().count();
    (patch, old_lines)
}
