// SPDX-License-Identifier: Apache-2.0

//! Bundled fonts: Inter for UI, JetBrains Mono for code. Both are under the SIL
//! Open Font License 1.1; the licence texts sit next to the files in `assets/fonts/`.

use std::borrow::Cow;

use gpui_kit::App;

pub const UI_FAMILY: &str = "Inter";
pub const MONO_FAMILY: &str = "JetBrains Mono";

const FONTS: [&[u8]; 6] = [
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
];

/// Make the bundled fonts available to GPUI's text system.
pub fn register(cx: &mut App) {
    let fonts = FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        // Not fatal: GPUI falls back to system fonts.
        eprintln!("coqo-ui: failed to load bundled fonts: {err:#}");
    }
}
