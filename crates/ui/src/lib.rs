// SPDX-License-Identifier: Apache-2.0

//! `coqo-ui`: Coqo's facade over GPUI and gpui-component (ADR 0002).
//!
//! Every other Coqo crate builds UI through this crate. It owns the exact
//! `gpui-kit` pin, the zen theme tokens ([`theme`]), the bundled fonts, and the
//! zen building blocks ([`widgets`]). GPUI itself, the element and layout
//! framework, is re-exported as [`gpui`]. **gpui-component widgets are wrapped here
//! and never used directly elsewhere**, so they can be swapped without touching
//! views or mods.

pub mod dock;
pub mod slots;
pub mod theme;
pub mod widgets;

mod assets;
mod fonts;

pub use assets::CoqoAssets;

/// JSON values, for tab state ([`dock::TabItem::save`]).
pub use serde_json;

/// GPUI: elements, layout, styling, entities and windows.
pub mod gpui {
    pub use gpui_kit::*;
}

pub use theme::{ActiveTokens, Appearance, Palette, Rgb, Tokens};

/// Test helpers: GPUI's headless test context and window queries.
#[cfg(feature = "test-support")]
pub mod test {
    pub use gpui_kit::TestAppContext;
    pub use gpui_kit::test::*;

    use gpui_kit::{Bounds, HeadlessAppContext, Point, Size, WindowBounds, WindowOptions, px};
    use image::RgbaImage;

    /// Render a view offscreen with the real GPU renderer (Metal on macOS, WGPU on
    /// Linux) and return the pixels. Nothing is shown on screen. Needs a GPU, so
    /// use it from `#[ignore]` tests or tools, not in CI.
    pub fn render_offscreen<V: gpui_kit::Render>(
        appearance: crate::Appearance,
        size: Size<f32>,
        build: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> gpui_kit::Entity<V>,
    ) -> gpui_kit::Result<RgbaImage> {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_kit::platform::current_platform(true).text_system(),
            std::sync::Arc::new(crate::CoqoAssets),
            gpui_kit::platform::current_headless_renderer,
        );
        cx.update(|cx| crate::init(appearance, cx));
        let (handle, _) = cx.update(|cx| {
            crate::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: gpui_kit::size(px(size.width), px(size.height)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                build,
            )
        })?;
        for _ in 0..2 {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))?;
            cx.run_until_parked();
        }
        cx.capture_screenshot(handle)
    }
}

use gpui_kit::{AnyWindowHandle, App, Application, Entity, Render, Result, Window, WindowOptions};

/// Name of this crate, used by the workspace smoke tests.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// The desktop application with Coqo's bundled assets (icons).
pub fn application() -> Application {
    gpui_kit::application().with_assets(CoqoAssets)
}

/// Initialise the UI layer: component library, bundled fonts and theme tokens.
/// Call once, before opening windows.
pub fn init(appearance: Appearance, cx: &mut App) {
    gpui_kit::init(cx);
    fonts::register(cx);
    theme::apply(appearance, None, cx);
    dock::init(cx);
    cx.default_global::<slots::Slots>();
    cx.default_global::<slots::SidebarPanes>();
}

/// Open a window whose root view is built by `build`. The view is wrapped in the
/// component library's root, so dialogs, menus and notifications work in it.
/// A `System` appearance is resolved against the new window, and follows the OS.
pub fn open_window<V: Render>(
    options: WindowOptions,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> Result<(AnyWindowHandle, Entity<V>)> {
    gpui_kit::open_window(options, cx, move |window, cx| {
        let choice = cx.global::<Tokens>().choice;
        theme::apply(choice, Some(window), cx);
        let view = build(window, cx);
        window
            .observe_window_appearance(theme::on_system_appearance_changed)
            .detach();
        view
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_set() {
        assert_eq!(CRATE_NAME, "coqo-ui");
    }
}
