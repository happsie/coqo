// SPDX-License-Identifier: Apache-2.0

//! Workbench layouts kept by the daemon, per workspace (#140, ADR 0004).
//!
//! A workspace's layout is the [`LAYOUT_KEY`] entry of its state document
//! (`workspace/getState` / `workspace/setState`): an opaque, versioned JSON
//! [`WorkbenchLayout`] that the daemon stores in `coqo.db` without reading.
//! [`DaemonLayoutStore`] is the app's [`LayoutStore`]:
//!
//! - **load** returns a copy saved while the daemon was unreachable (a file in
//!   the fallback store, newer by construction), else the daemon's copy, which
//!   the window fetched with the workspace and handed over with
//!   [`DaemonLayoutStore::fetched`].
//! - **save** sends `workspace/setState` when connected (and drops the
//!   fallback file, now stale), else writes the fallback file, which the next
//!   save after reattaching moves into the daemon.
//! - The window's no-workspace layout (`local_key`, `"default"`) stays in the
//!   fallback store. The shell moves it into the first workspace that has no
//!   layout of its own, which is also how a layout file from before #140 is
//!   imported, once.

use std::{cell::RefCell, collections::HashMap, io, rc::Rc};

use coqo_protocol::methods::workspace::{SetState, SetStateParams, WorkspaceId};
use coqo_ui::{
    dock::store::{Flush, LAYOUT_VERSION, LayoutStore, WorkbenchLayout},
    gpui::{App, ForegroundExecutor, Global},
    serde_json::{self, Value},
};
use futures_util::{
    FutureExt as _,
    future::{LocalBoxFuture, Shared},
};

use crate::daemon::{Daemon, DaemonHandle};

/// The workspace state entry that holds the layout.
pub const LAYOUT_KEY: &str = "ui.layout";

/// Layouts in the daemon, with a file store as the offline fallback. See the
/// module docs.
pub struct DaemonLayoutStore {
    fallback: Rc<dyn LayoutStore>,
    local_key: String,
    daemon: Option<DaemonHandle>,
    executor: ForegroundExecutor,
    /// The daemon's copies, by workspace id: `None` when it has none.
    remote: RefCell<HashMap<String, Option<WorkbenchLayout>>>,
    /// The latest save per workspace still in flight, for [`flush`].
    ///
    /// [`flush`]: LayoutStore::flush
    writes: RefCell<HashMap<String, Shared<LocalBoxFuture<'static, ()>>>>,
}

impl std::fmt::Debug for DaemonLayoutStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonLayoutStore")
            .field("local_key", &self.local_key)
            .field("daemon", &self.daemon)
            .finish_non_exhaustive()
    }
}

struct GlobalLayoutStore(Rc<DaemonLayoutStore>);

impl Global for GlobalLayoutStore {}

/// Parse a stored layout; `None` if it isn't one this build reads.
pub fn parse(value: &Value) -> Option<WorkbenchLayout> {
    serde_json::from_value::<WorkbenchLayout>(value.clone())
        .ok()
        .filter(|l| l.version == LAYOUT_VERSION)
}

impl DaemonLayoutStore {
    /// A store over the app's daemon (if [`daemon::init`](crate::daemon::init)
    /// ran) and `fallback`. `local_key` is the key of the no-workspace layout.
    pub fn new(fallback: Rc<dyn LayoutStore>, local_key: &str, cx: &App) -> Self {
        Self {
            fallback,
            local_key: local_key.to_owned(),
            daemon: Daemon::global(cx).map(|d| d.read(cx).handle()),
            executor: cx.foreground_executor().clone(),
            remote: RefCell::default(),
            writes: RefCell::default(),
        }
    }

    /// Create the app's store and make it [`global`](Self::global), so
    /// windows hand it what they fetch.
    pub fn install(
        fallback: Rc<dyn LayoutStore>,
        local_key: &str,
        cx: &mut App,
    ) -> Rc<DaemonLayoutStore> {
        let store = Rc::new(Self::new(fallback, local_key, cx));
        cx.set_global(GlobalLayoutStore(store.clone()));
        store
    }

    /// The app's store, once [`install`](Self::install) ran.
    pub fn global(cx: &App) -> Option<Rc<DaemonLayoutStore>> {
        cx.try_global::<GlobalLayoutStore>().map(|g| g.0.clone())
    }

    /// Record the daemon's copy of `workspace`'s layout (its [`LAYOUT_KEY`]
    /// state entry, or `None`), as fetched with the workspace.
    pub fn fetched(&self, workspace: &str, value: Option<&Value>) {
        self.remote
            .borrow_mut()
            .insert(workspace.to_owned(), value.and_then(parse));
    }

    fn connected(&self) -> Option<&DaemonHandle> {
        self.daemon.as_ref().filter(|d| d.is_connected())
    }
}

impl LayoutStore for DaemonLayoutStore {
    fn load(&self, workspace: &str) -> io::Result<Option<WorkbenchLayout>> {
        if let Some(offline) = self.fallback.load(workspace)? {
            return Ok(Some(offline));
        }
        if workspace == self.local_key {
            return Ok(None);
        }
        Ok(self.remote.borrow().get(workspace).cloned().flatten())
    }

    fn save(&self, workspace: &str, layout: &WorkbenchLayout) -> io::Result<()> {
        if workspace == self.local_key {
            return self.fallback.save(workspace, layout);
        }
        let Some(daemon) = self.connected() else {
            return self.fallback.save(workspace, layout);
        };
        let value = serde_json::to_value(layout).map_err(io::Error::other)?;
        let write = daemon.request::<SetState>(SetStateParams {
            id: WorkspaceId(workspace.to_owned()),
            key: LAYOUT_KEY.to_owned(),
            value,
        });
        self.remote
            .borrow_mut()
            .insert(workspace.to_owned(), Some(layout.clone()));
        // If the write fails, put the layout in the fallback store, so the
        // next save after reattaching retries it.
        let (fallback, key, layout) = (self.fallback.clone(), workspace.to_owned(), layout.clone());
        let write = async move {
            if let Err(err) = write.await {
                eprintln!("coqo: couldn't save the layout in coqod: {err}");
                if let Err(err) = fallback.save(&key, &layout) {
                    eprintln!("coqo: couldn't save the layout: {err}");
                }
            }
        }
        .boxed_local()
        .shared();
        self.executor.spawn(write.clone()).detach();
        self.writes.borrow_mut().insert(workspace.to_owned(), write);
        self.fallback.remove(workspace)
    }

    fn remove(&self, workspace: &str) -> io::Result<()> {
        self.fallback.remove(workspace)
    }

    fn flush(&self) -> Flush {
        let writes: Vec<_> = self.writes.borrow_mut().drain().map(|(_, w)| w).collect();
        Box::pin(futures_util::future::join_all(writes).map(|_| ()))
    }
}
