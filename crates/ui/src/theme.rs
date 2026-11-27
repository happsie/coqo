// SPDX-License-Identifier: Apache-2.0

//! Zen theme tokens (PLAN §14, design board 1). Documented in `docs/design/tokens.md`.
//!
//! Views read colours and metrics from [`Tokens`] through [`ActiveTokens::tokens`],
//! never from hard-coded hex values, so light and dark stay in sync.

use gpui_kit::{App, Global, Hsla, Pixels, SharedString, Window, WindowAppearance, px};

/// An opaque sRGB colour, `0xRRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u32);

impl Rgb {
    /// The colour as a GPUI colour.
    pub fn hsla(self) -> Hsla {
        gpui_kit::rgb(self.0).into()
    }

    /// The colour with an alpha channel.
    pub fn alpha(self, alpha: f32) -> Hsla {
        self.hsla().opacity(alpha)
    }

    /// This colour moved `t` (0 to 1) of the way towards `other`.
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let channel = |shift: u32| {
            let a = ((self.0 >> shift) & 0xFF) as f32;
            let b = ((other.0 >> shift) & 0xFF) as f32;
            ((a + (b - a) * t.clamp(0.0, 1.0)).round() as u32).min(255)
        };
        Rgb(channel(16) << 16 | channel(8) << 8 | channel(0))
    }

    /// WCAG 2.x relative luminance.
    pub fn luminance(self) -> f64 {
        let channel = |shift: u32| {
            let c = f64::from((self.0 >> shift) & 0xFF) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    /// WCAG 2.x contrast ratio between two colours (1.0 to 21.0).
    pub fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

/// Light or dark. `System` follows the OS and resolves to one of the other two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    #[default]
    System,
    Dark,
    Light,
}

impl Appearance {
    /// Resolve `System` against the window's current appearance.
    pub fn resolve(self, window: WindowAppearance) -> Appearance {
        match self {
            Appearance::System => match window {
                WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
            },
            other => other,
        }
    }
}

/// Colours of one appearance.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    /// The window background the panels float on.
    pub ground: Rgb,
    /// The main tab area panel.
    pub panel: Rgb,
    /// Side panels and inset fields (sidebar, command search).
    pub panel_raised: Rgb,
    /// Cards and popovers on a panel.
    pub surface: Rgb,
    /// Inline chips such as key caps.
    pub surface_strong: Rgb,
    /// Selected list rows.
    pub surface_selected: Rgb,
    /// Hairlines between regions.
    pub border: Rgb,
    /// Borders of raised controls.
    pub border_strong: Rgb,
    /// Top and bottom of the raised-control gradient.
    pub raised_top: Rgb,
    pub raised_bottom: Rgb,
    /// Primary, secondary and tertiary text.
    pub text: Rgb,
    pub text_muted: Rgb,
    pub text_subtle: Rgb,
    /// Primary / OK.
    pub sage: Rgb,
    /// Needs you.
    pub sand: Rgb,
    /// The ⚡ bolt (language intelligence on).
    pub bolt: Rgb,
    /// Info and links.
    pub blue: Rgb,
    /// Risk and removed lines.
    pub rose: Rgb,
    /// Syntax and file-type accents (Kotlin).
    pub lavender: Rgb,
    /// Syntax and file-type accents (Java).
    pub peach: Rgb,
    /// Added lines and passing tests.
    pub green: Rgb,
}

impl Palette {
    /// The zen dark palette, taken from design board 1.
    pub const DARK: Palette = Palette {
        ground: Rgb(0x16171A),
        panel: Rgb(0x1C1D21),
        panel_raised: Rgb(0x1F2024),
        surface: Rgb(0x26272C),
        surface_strong: Rgb(0x2A2B31),
        surface_selected: Rgb(0x2E2F35),
        border: Rgb(0x2C2D33),
        border_strong: Rgb(0x3A3B42),
        raised_top: Rgb(0x34353C),
        raised_bottom: Rgb(0x2B2C32),
        text: Rgb(0xE8E6E1),
        text_muted: Rgb(0xB4B1AA),
        text_subtle: Rgb(0x9A978F),
        sage: Rgb(0x9DBF9E),
        sand: Rgb(0xE2B97F),
        bolt: Rgb(0xE6C384),
        blue: Rgb(0x8FB3D9),
        rose: Rgb(0xD98E8E),
        lavender: Rgb(0xC4A7E7),
        peach: Rgb(0xE69875),
        green: Rgb(0xA7C080),
    };

    /// The zen light palette: warm paper with darkened accents so every text pair
    /// still meets 4.5:1. Not on a design board yet; derived from the dark one.
    pub const LIGHT: Palette = Palette {
        ground: Rgb(0xECE9E3),
        panel: Rgb(0xF7F5F1),
        panel_raised: Rgb(0xFBFAF7),
        surface: Rgb(0xF0EDE7),
        surface_strong: Rgb(0xE8E5DE),
        surface_selected: Rgb(0xE4E0D8),
        border: Rgb(0xDEDAD2),
        border_strong: Rgb(0xCFCAC0),
        raised_top: Rgb(0xFFFFFF),
        raised_bottom: Rgb(0xF1EEE8),
        text: Rgb(0x23221F),
        text_muted: Rgb(0x55524C),
        text_subtle: Rgb(0x64615A),
        sage: Rgb(0x3D6840),
        sand: Rgb(0x7A5712),
        bolt: Rgb(0x765610),
        blue: Rgb(0x2E5D8C),
        rose: Rgb(0x9A3C3C),
        lavender: Rgb(0x664A96),
        peach: Rgb(0x94461F),
        green: Rgb(0x4A6721),
    };

    /// Pairs of (foreground, background) that appear as text in the UI. Each must
    /// meet WCAG AA (4.5:1). Used by the tests and `docs/design/tokens.md`.
    pub fn text_pairs(&self) -> Vec<(&'static str, Rgb, &'static str, Rgb)> {
        let backgrounds = [
            ("ground", self.ground),
            ("panel", self.panel),
            ("panel_raised", self.panel_raised),
            ("surface", self.surface),
            ("surface_strong", self.surface_strong),
            ("surface_selected", self.surface_selected),
            ("raised_bottom", self.raised_bottom),
        ];
        let foregrounds = [
            ("text", self.text),
            ("text_muted", self.text_muted),
            ("text_subtle", self.text_subtle),
            ("sage", self.sage),
            ("sand", self.sand),
            ("bolt", self.bolt),
            ("blue", self.blue),
            ("rose", self.rose),
            ("lavender", self.lavender),
            ("peach", self.peach),
            ("green", self.green),
        ];
        let mut pairs = Vec::new();
        for (fg_name, fg) in foregrounds {
            for (bg_name, bg) in backgrounds {
                pairs.push((fg_name, fg, bg_name, bg));
            }
        }
        pairs
    }
}

/// Terminal colours, derived from a [`Palette`]: the ANSI colours are the zen
/// accents, the bright ones lean towards the text colour, and the rest of the
/// 256-colour palette (the 6×6×6 cube and the grey ramp) is computed as xterm
/// does.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalPalette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    /// Selection highlight, drawn at [`TerminalPalette::SELECTION_ALPHA`].
    pub selection: Rgb,
    /// Black, red, green, yellow, blue, magenta, cyan, white, then the bright ones.
    pub ansi: [Rgb; 16],
}

impl TerminalPalette {
    pub const SELECTION_ALPHA: f32 = 0.35;

    /// Derive the terminal colours of `appearance` from `palette`.
    pub fn new(palette: &Palette, appearance: Appearance) -> Self {
        let p = palette;
        let light = appearance == Appearance::Light;
        let (black, white, bright_white) = if light {
            (p.text, p.border_strong, p.surface_strong)
        } else {
            (p.border_strong, p.text_muted, p.text)
        };
        let base = [
            black,
            p.rose,
            p.green,
            p.sand,
            p.blue,
            p.lavender,
            p.blue.mix(p.sage, 0.5),
            white,
        ];
        let mut ansi = [Rgb(0); 16];
        for (i, c) in base.iter().enumerate() {
            ansi[i] = *c;
            ansi[i + 8] = c.mix(p.text, 0.3);
        }
        ansi[8] = p.text_subtle;
        ansi[15] = bright_white;
        Self {
            foreground: p.text,
            background: p.panel,
            cursor: p.text,
            selection: p.blue,
            ansi,
        }
    }

    /// A colour of the 256-colour palette.
    pub fn indexed(&self, index: u8) -> Rgb {
        match index {
            0..=15 => self.ansi[usize::from(index)],
            16..=231 => {
                let i = u32::from(index - 16);
                let level = |v: u32| if v == 0 { 0 } else { 55 + v * 40 };
                Rgb(level(i / 36) << 16 | level(i / 6 % 6) << 8 | level(i % 6))
            }
            232..=255 => {
                let v = 8 + 10 * u32::from(index - 232);
                Rgb(v << 16 | v << 8 | v)
            }
        }
    }
}

/// Spacing, radii and type scale shared by both appearances.
#[derive(Clone, Debug, PartialEq)]
pub struct Metrics {
    /// Gap between floating panels, and the window padding around them.
    pub gap: Pixels,
    /// Corner radius of floating panels.
    pub radius_panel: Pixels,
    /// Corner radius of tabs and buttons.
    pub radius_control: Pixels,
    /// Corner radius of fields (command search) and pills.
    pub radius_field: Pixels,
    /// Corner radius of workspace-rail buttons.
    pub radius_rail: Pixels,
    /// Width of the workspace rail.
    pub rail_width: Pixels,
    /// Size of a workspace-rail button.
    pub rail_button: Pixels,
    /// Default width of the sidebar panel.
    pub sidebar_width: Pixels,
    pub font_ui: SharedString,
    pub font_mono: SharedString,
    /// Body text.
    pub text_md: Pixels,
    /// Tabs, controls and list rows.
    pub text_sm: Pixels,
    /// Status bar.
    pub text_xs: Pixels,
    /// Upper-case section labels.
    pub text_label: Pixels,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            gap: px(10.),
            radius_panel: px(16.),
            radius_control: px(10.),
            radius_field: px(12.),
            radius_rail: px(14.),
            rail_width: px(52.),
            rail_button: px(42.),
            sidebar_width: px(250.),
            font_ui: crate::fonts::UI_FAMILY.into(),
            font_mono: crate::fonts::MONO_FAMILY.into(),
            text_md: px(13.),
            text_sm: px(12.5),
            text_xs: px(11.5),
            text_label: px(11.),
        }
    }
}

/// The active theme: the user's appearance choice, its resolved palette and metrics.
#[derive(Clone, Debug, PartialEq)]
pub struct Tokens {
    pub choice: Appearance,
    pub appearance: Appearance,
    pub palette: Palette,
    pub metrics: Metrics,
}

impl Tokens {
    /// The terminal colours for this appearance.
    pub fn terminal(&self) -> TerminalPalette {
        TerminalPalette::new(&self.palette, self.appearance)
    }

    pub fn new(choice: Appearance, resolved: Appearance) -> Self {
        let appearance = match resolved {
            Appearance::Light => Appearance::Light,
            Appearance::Dark | Appearance::System => Appearance::Dark,
        };
        let palette = match appearance {
            Appearance::Light => Palette::LIGHT,
            _ => Palette::DARK,
        };
        Self {
            choice,
            appearance,
            palette,
            metrics: Metrics::default(),
        }
    }
}

impl Global for Tokens {}

/// Read the active [`Tokens`] from any context.
pub trait ActiveTokens {
    fn tokens(&self) -> &Tokens;
}

impl ActiveTokens for App {
    fn tokens(&self) -> &Tokens {
        self.global::<Tokens>()
    }
}

/// Install the tokens for `choice` (resolved against `window` when it is `System`)
/// and map them onto gpui-component's theme so its widgets match.
pub fn apply(choice: Appearance, window: Option<&Window>, cx: &mut App) {
    let system = window
        .map(Window::appearance)
        .unwrap_or(WindowAppearance::Dark);
    let tokens = Tokens::new(choice, choice.resolve(system));
    sync_component_theme(&tokens, cx);
    cx.set_global(tokens);
    cx.refresh_windows();
}

/// Re-resolve a `System` choice after the OS appearance changed.
pub fn on_system_appearance_changed(window: &mut Window, cx: &mut App) {
    let choice = cx.tokens().choice;
    if choice == Appearance::System {
        apply(choice, Some(&*window), cx);
    }
}

fn sync_component_theme(tokens: &Tokens, cx: &mut App) {
    use gpui_kit::component::{Theme, ThemeMode};

    let mode = match tokens.appearance {
        Appearance::Light => ThemeMode::Light,
        _ => ThemeMode::Dark,
    };
    Theme::change(mode, None, cx);
    let p = &tokens.palette;
    let theme = Theme::global_mut(cx);
    theme.font_family = tokens.metrics.font_ui.clone();
    theme.mono_font_family = tokens.metrics.font_mono.clone();
    theme.radius = tokens.metrics.radius_control;
    theme.radius_lg = tokens.metrics.radius_panel;
    theme.background = p.panel.hsla();
    theme.foreground = p.text.hsla();
    theme.muted = p.surface.hsla();
    theme.muted_foreground = p.text_muted.hsla();
    theme.border = p.border.hsla();
    theme.primary = p.sage.hsla();
    theme.popover = p.surface.hsla();
    theme.popover_foreground = p.text.hsla();
    theme.secondary = p.surface.hsla();
    theme.secondary_foreground = p.text.hsla();
    theme.ring = p.blue.hsla();
}

#[cfg(test)]
mod tests {
    use super::*;

    const AA: f64 = 4.5;

    fn failing(palette: &Palette) -> Vec<String> {
        palette
            .text_pairs()
            .into_iter()
            .filter_map(|(fg_name, fg, bg_name, bg)| {
                let ratio = fg.contrast(bg);
                (ratio < AA).then(|| format!("{fg_name} on {bg_name}: {ratio:.2}:1"))
            })
            .collect()
    }

    #[test]
    fn dark_text_meets_wcag_aa() {
        assert_eq!(failing(&Palette::DARK), Vec::<String>::new());
    }

    #[test]
    fn light_text_meets_wcag_aa() {
        assert_eq!(failing(&Palette::LIGHT), Vec::<String>::new());
    }

    #[test]
    fn contrast_matches_known_values() {
        assert!((Rgb(0x000000).contrast(Rgb(0xFFFFFF)) - 21.0).abs() < 1e-9);
        assert!((Rgb(0x777777).contrast(Rgb(0xFFFFFF)) - 4.48).abs() < 0.01);
        assert_eq!(Rgb(0x123456).contrast(Rgb(0x123456)), 1.0);
    }

    #[test]
    fn system_resolves_against_the_window() {
        assert_eq!(
            Appearance::System.resolve(WindowAppearance::VibrantLight),
            Appearance::Light
        );
        assert_eq!(
            Appearance::System.resolve(WindowAppearance::Dark),
            Appearance::Dark
        );
        assert_eq!(
            Appearance::Light.resolve(WindowAppearance::Dark),
            Appearance::Light
        );
    }

    #[test]
    fn terminal_colours_come_from_the_palette_and_stay_readable() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let t = Tokens::new(appearance, appearance).terminal();
            assert_eq!(
                t.background,
                Tokens::new(appearance, appearance).palette.panel
            );
            // Red to cyan, plain and bright, are readable on the background.
            for i in (1..=6).chain(9..=14) {
                let ratio = t.ansi[i].contrast(t.background);
                assert!(ratio >= 4.5, "{appearance:?} ansi {i}: {ratio:.2}:1");
            }
            assert!(t.ansi[0].contrast(t.ansi[7]) > 1.5);
        }
        let t = Tokens::new(Appearance::Dark, Appearance::Dark).terminal();
        assert_eq!(t.indexed(16), Rgb(0x000000));
        assert_eq!(t.indexed(196), Rgb(0xFF0000));
        assert_eq!(t.indexed(231), Rgb(0xFFFFFF));
        assert_eq!(t.indexed(232), Rgb(0x080808));
        assert_eq!(t.indexed(255), Rgb(0xEEEEEE));
        assert_eq!(Rgb(0x000000).mix(Rgb(0xFFFFFF), 0.5), Rgb(0x808080));
    }

    #[test]
    fn tokens_pick_the_palette_for_the_resolved_appearance() {
        let t = Tokens::new(Appearance::System, Appearance::Light);
        assert_eq!(t.choice, Appearance::System);
        assert_eq!(t.appearance, Appearance::Light);
        assert_eq!(t.palette, Palette::LIGHT);
        assert_eq!(
            Tokens::new(Appearance::Dark, Appearance::Dark).palette,
            Palette::DARK
        );
    }
}
