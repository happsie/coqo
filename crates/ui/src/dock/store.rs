// SPDX-License-Identifier: Apache-2.0

//! Saving and loading the workbench layout, per workspace.
//!
//! The UI owns its layout (PLAN §4.2). Until the daemon can hold it, layouts are
//! JSON files in the UI's data directory; [`LayoutStore`] is the seam the daemon
//! store will plug into.

use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    rc::Rc,
};

use serde::{Deserialize, Serialize};

use super::tree::PaneNode;

/// The layout format version. A file with another version is ignored and the
/// default layout is used.
pub const LAYOUT_VERSION: u32 = 1;

/// Everything about a workbench window's arrangement that survives a restart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkbenchLayout {
    pub version: u32,
    pub sidebar: SidebarLayout,
    /// The main tab area.
    pub dock: PaneNode,
}

/// The sidebar panel's width and shown pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SidebarLayout {
    /// Width in logical pixels.
    pub width: f32,
    /// Id of the shown sidebar pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

/// Loads and saves layouts by workspace key.
pub trait LayoutStore {
    /// `Ok(None)` when nothing usable is saved for this workspace.
    fn load(&self, workspace: &str) -> io::Result<Option<WorkbenchLayout>>;
    fn save(&self, workspace: &str, layout: &WorkbenchLayout) -> io::Result<()>;
}

/// Keeps layouts in memory. Clones share the same storage, so a test can
/// "restart" the UI against it.
#[derive(Clone, Debug, Default)]
pub struct MemoryLayoutStore(Rc<RefCell<HashMap<String, WorkbenchLayout>>>);

impl LayoutStore for MemoryLayoutStore {
    fn load(&self, workspace: &str) -> io::Result<Option<WorkbenchLayout>> {
        Ok(self.0.borrow().get(workspace).cloned())
    }

    fn save(&self, workspace: &str, layout: &WorkbenchLayout) -> io::Result<()> {
        self.0
            .borrow_mut()
            .insert(workspace.to_owned(), layout.clone());
        Ok(())
    }
}

/// One `<workspace>.json` per workspace in a directory, written atomically.
#[derive(Clone, Debug)]
pub struct JsonFileLayoutStore {
    dir: PathBuf,
}

impl JsonFileLayoutStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file for a workspace key. Keys are reduced to a safe file name:
    /// characters outside `[A-Za-z0-9_-]` become `_xx` hex escapes.
    pub fn path(&self, workspace: &str) -> PathBuf {
        let mut name = String::new();
        for b in workspace.bytes() {
            if b.is_ascii_alphanumeric() || b == b'-' {
                name.push(b as char);
            } else {
                name.push_str(&format!("_{b:02x}"));
            }
        }
        if name.is_empty() {
            name.push('_');
        }
        self.dir.join(format!("{name}.json"))
    }
}

impl LayoutStore for JsonFileLayoutStore {
    fn load(&self, workspace: &str) -> io::Result<Option<WorkbenchLayout>> {
        let bytes = match std::fs::read(self.path(workspace)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        // A corrupt or newer file falls back to the default layout rather than
        // failing the window.
        Ok(serde_json::from_slice::<WorkbenchLayout>(&bytes)
            .ok()
            .filter(|l| l.version == LAYOUT_VERSION))
    }

    fn save(&self, workspace: &str, layout: &WorkbenchLayout) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(workspace);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(layout).map_err(io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dock::tree::{Edge, PaneTree, TabId, TabState, TabTarget};

    fn layout() -> WorkbenchLayout {
        let mut tree = PaneTree::new();
        let g = tree.root().id();
        for (kind, target) in [
            (
                "welcome",
                TabTarget::Group {
                    group: g,
                    index: None,
                },
            ),
            (
                "chat",
                TabTarget::Split {
                    group: g,
                    edge: Edge::Right,
                },
            ),
        ] {
            let id = TabId(tree.alloc_id());
            tree.insert(
                TabState {
                    id,
                    kind: kind.into(),
                    state: serde_json::json!({ "n": id.0 }),
                },
                target,
            );
        }
        WorkbenchLayout {
            version: LAYOUT_VERSION,
            sidebar: SidebarLayout {
                width: 300.,
                pane: Some("files".into()),
            },
            dock: tree.root().clone(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("coqo-layout-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn json_file_store_round_trips() {
        let dir = scratch("round-trip");
        let store = JsonFileLayoutStore::new(&dir);
        assert_eq!(store.load("default").unwrap(), None);
        store.save("default", &layout()).unwrap();
        assert_eq!(store.load("default").unwrap(), Some(layout()));
        assert!(store.path("default").ends_with("default.json"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn json_file_store_ignores_corrupt_and_future_files() {
        let dir = scratch("corrupt");
        let store = JsonFileLayoutStore::new(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(store.path("a"), b"{ not json").unwrap();
        assert_eq!(store.load("a").unwrap(), None);
        let mut future = layout();
        future.version = LAYOUT_VERSION + 1;
        store.save("b", &future).unwrap();
        assert_eq!(store.load("b").unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_keys_become_safe_file_names() {
        let store = JsonFileLayoutStore::new("/x");
        assert_eq!(store.path("default"), PathBuf::from("/x/default.json"));
        assert_eq!(
            store.path("../etc/passwd"),
            PathBuf::from("/x/_2e_2e_2fetc_2fpasswd.json")
        );
        assert_eq!(store.path(""), PathBuf::from("/x/_.json"));
    }

    #[test]
    fn memory_store_clones_share_storage() {
        let a = MemoryLayoutStore::default();
        let b = a.clone();
        a.save("w", &layout()).unwrap();
        assert_eq!(b.load("w").unwrap(), Some(layout()));
    }
}
