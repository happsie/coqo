// SPDX-License-Identifier: Apache-2.0

//! The guard for #266: every key binding the app registers that starts with
//! a plain key (a letter, an arrow, `enter`, `escape`…) is kept out of text
//! fields and editors, so typing never fires a view's shortcut. Bindings are
//! made with [`coqo_ui::keys::binding`]; a `KeyBinding::new` with a plain key
//! fails here.

use std::rc::Rc;

use coqo_client::DaemonPaths;
use coqo_ui::{Appearance, dock::store::MemoryLayoutStore, keys, test::TestAppContext};

use crate::{daemon, shell, terminal, windows, workspace};

#[test]
fn no_plain_key_binding_fires_inside_a_text_field() {
    let tmp = tempfile::tempdir().unwrap();
    let cx = TestAppContext::single();
    cx.update(|cx| {
        // Everything `main` binds keys in.
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = daemon::DaemonConfig::at(DaemonPaths::at(tmp.path().join("run")));
        config.spawn = false;
        let daemon = daemon::init(config, cx);
        workspace::init(None, cx);
        shell::init(cx);
        let handle = daemon.read(cx).handle();
        terminal::init(Rc::new(terminal::DaemonBackend::new(handle)), cx);
        windows::init(Rc::new(MemoryLayoutStore::default()), cx);

        let unguarded = keys::unguarded_bindings(cx);
        assert!(
            unguarded.is_empty(),
            "these bindings fire while typing in a text field; make them with \
             `coqo_ui::keys::binding`:\n{}",
            unguarded.join("\n")
        );

        // The check saw the view keys, and they carry the exclusion.
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        for action in [
            "diff_review::NextHunk",
            "diff_review::ToggleMode",
            "walkthrough::NextStep",
            "walkthrough::ToggleUnderstood",
        ] {
            let contexts: Vec<String> = keymap
                .bindings()
                .filter(|b| b.action().name() == action)
                .map(|b| b.predicate().map(|p| p.to_string()).unwrap_or_default())
                .collect();
            assert!(!contexts.is_empty(), "{action} is bound");
            assert!(
                contexts.iter().all(|c| c.ends_with("!Input")),
                "{action}: {contexts:?}"
            );
        }
    });
}
