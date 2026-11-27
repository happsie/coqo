// SPDX-License-Identifier: Apache-2.0

//! What a file shows before its name (board 1): a coloured language letter
//! (`K` Kotlin in lavender, `J` Java in peach, …) for source files, and a
//! Lucide icon for everything else.

use coqo_ui::{
    ActiveTokens as _, Palette, Rgb,
    gpui::{
        AnyElement, App, FontWeight, IntoElement as _, ParentElement as _, Styled as _, div, px,
    },
    widgets::{Icon, icon},
};

/// An accent colour from the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accent {
    Lavender,
    Peach,
    Blue,
    Sand,
    Green,
    Rose,
    Sage,
}

impl Accent {
    fn rgb(self, p: &Palette) -> Rgb {
        match self {
            Accent::Lavender => p.lavender,
            Accent::Peach => p.peach,
            Accent::Blue => p.blue,
            Accent::Sand => p.sand,
            Accent::Green => p.green,
            Accent::Rose => p.rose,
            Accent::Sage => p.sage,
        }
    }
}

/// A file's glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// A language letter in an accent colour.
    Letter(&'static str, Accent),
    /// A muted icon.
    Icon(Icon),
}

/// The glyph for a file name, by extension.
pub fn for_name(name: &str) -> Glyph {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |(_, ext)| ext);
    match ext {
        "kt" | "kts" => Glyph::Letter("K", Accent::Lavender),
        "java" => Glyph::Letter("J", Accent::Peach),
        "rs" => Glyph::Letter("R", Accent::Peach),
        "ts" | "tsx" | "mts" | "cts" => Glyph::Letter("TS", Accent::Blue),
        "js" | "jsx" | "mjs" | "cjs" => Glyph::Letter("JS", Accent::Sand),
        "py" => Glyph::Letter("Py", Accent::Blue),
        "go" => Glyph::Letter("Go", Accent::Blue),
        "rb" => Glyph::Letter("Rb", Accent::Rose),
        "swift" => Glyph::Letter("S", Accent::Peach),
        "c" | "h" => Glyph::Letter("C", Accent::Blue),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Glyph::Letter("C+", Accent::Blue),
        "cs" => Glyph::Letter("C#", Accent::Green),
        "scala" | "sc" => Glyph::Letter("Sc", Accent::Rose),
        "sh" | "bash" | "zsh" | "fish" => Glyph::Letter("$", Accent::Sage),
        "html" | "htm" | "css" | "scss" | "less" | "vue" | "svelte" | "sql" | "lua" | "php"
        | "zig" | "ex" | "exs" | "erl" | "hs" | "ml" | "dart" | "groovy" | "gradle" => {
            Glyph::Icon(Icon::FileCode)
        }
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" | "avif" => {
            Glyph::Icon(Icon::Image)
        }
        "md" | "mdx" | "txt" | "rst" | "adoc" | "toml" | "yaml" | "yml" | "json" | "jsonc"
        | "json5" | "xml" | "lock" | "ini" | "cfg" | "conf" | "env" | "csv" | "tsv"
        | "properties" => Glyph::Icon(Icon::FileText),
        _ if ext.is_empty() || lower.starts_with('.') => Glyph::Icon(Icon::FileText),
        _ => Glyph::Icon(Icon::File),
    }
}

/// Render the glyph for `name` in a fixed-width slot, so names line up.
pub fn render(name: &str, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let p = &t.palette;
    let slot = div()
        .w(px(16.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center();
    match for_name(name) {
        Glyph::Letter(letter, accent) => slot
            .text_size(if letter.len() > 1 { px(10.) } else { px(12.) })
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(accent.rgb(p).hsla())
            .child(letter)
            .into_any_element(),
        Glyph::Icon(i) => slot
            .child(icon(i, px(13.), p.text_subtle.hsla()))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_get_letters_and_the_rest_icons() {
        assert_eq!(
            for_name("DiscountService.kt"),
            Glyph::Letter("K", Accent::Lavender)
        );
        assert_eq!(
            for_name("CheckoutService.java"),
            Glyph::Letter("J", Accent::Peach)
        );
        assert_eq!(for_name("main.RS"), Glyph::Letter("R", Accent::Peach));
        assert_eq!(for_name("pom.xml"), Glyph::Icon(Icon::FileText));
        assert_eq!(for_name("logo.png"), Glyph::Icon(Icon::Image));
        assert_eq!(for_name("Makefile"), Glyph::Icon(Icon::FileText));
        assert_eq!(for_name(".gitignore"), Glyph::Icon(Icon::FileText));
        assert_eq!(for_name("data.bin"), Glyph::Icon(Icon::File));
    }
}
