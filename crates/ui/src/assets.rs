// SPDX-License-Identifier: Apache-2.0

//! Coqo's asset source: the icons `coqo-ui` uses (Lucide, ISC licence, see
//! `assets/icons/LICENSE-LUCIDE.txt`), falling back to gpui-kit's default bundle
//! for the icons its own widgets draw.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[
            $((concat!("icons/", $name, ".svg"), include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*
        ];
    };
}

icons!(
    "search",
    "zap",
    "bell",
    "settings",
    "plus",
    "message-circle",
    "columns-2",
    "git-branch",
    "folder",
    "x",
    "rows-2",
    "sparkles",
    "circle-alert",
    "chevron-right",
    "chevron-down",
    "file",
    "file-text",
    "file-code",
    "image",
    "folder-open",
    "rotate-cw",
    "check",
    "circle",
    "circle-dot",
    "loader-circle",
    "square",
    "file-pen",
    "terminal",
    "brain",
    "globe",
    "trash",
    "eye",
    "shield-alert",
    "wrench",
    "arrow-right-left",
    "circle-x",
    "log-in",
    "arrow-up",
);

/// Serves Coqo's bundled icons, then gpui-kit's defaults.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoqoAssets;

impl AssetSource for CoqoAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        for (p, _) in ICONS {
            if p.starts_with(path) && !paths.iter().any(|existing| existing == p) {
                paths.push((*p).into());
            }
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::Icon;

    #[test]
    fn every_icon_is_bundled() {
        for icon in Icon::ALL {
            let bytes = CoqoAssets
                .load(icon.path())
                .unwrap_or_else(|e| panic!("{icon:?}: {e}"))
                .unwrap_or_else(|| panic!("{icon:?}: missing"));
            assert!(bytes.starts_with(b"<svg"), "{icon:?} is not an SVG");
        }
    }

    #[test]
    fn falls_back_to_the_component_bundle() {
        // gpui-component's own widgets draw this one.
        assert!(CoqoAssets.load("icons/check.svg").unwrap().is_some());
    }
}
