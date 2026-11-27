// SPDX-License-Identifier: Apache-2.0

//! Rich text: Markdown rendered with the zen theme (gpui-component's text
//! view, wrapped so views never use it directly).

use gpui_kit::component::text::TextView;
use gpui_kit::{ElementId, IntoElement, SharedString, Styled as _};

/// Markdown `source`, selectable. Keep `id` stable across renders so a
/// streaming message re-parses in place instead of starting over.
pub fn markdown(id: impl Into<ElementId>, source: impl Into<SharedString>) -> impl IntoElement {
    TextView::markdown(id, source).selectable(true).w_full()
}
