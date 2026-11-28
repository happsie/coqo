// SPDX-License-Identifier: Apache-2.0

//! The command registry: every command the palette can run, its default
//! keys, the user's overrides, and the recently used list.
//!
//! The registry owns the key bindings of its commands. [`apply`] rebuilds
//! them in GPUI's keymap: every binding whose action is a registered
//! command's is dropped, and the effective keys (the user's for that id, or
//! the defaults) are bound again through [`coqo_ui::keys::load`], so the
//! #266 rule (plain keys never fire while typing) holds for them too. The
//! other bindings (the component library's, list navigation inside views)
//! keep their order.

use std::{
    any::TypeId,
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

use coqo_ui::{
    gpui::{Action, App, Global, KeyBinding, KeyBindingContextPredicate, KeyContext, SharedString},
    keys,
};

use super::keyfile::{self, UserKeys};

/// How many recently used commands are remembered.
const RECENT_MAX: usize = 30;

/// Where a command comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Coqo's own views.
    Core,
    /// A mod in the daemon (its id).
    Mod(SharedString),
}

/// A command: what the palette lists and a key runs.
pub struct Command {
    /// Unique id, such as `diff.next_hunk`: what `keybindings.toml` names.
    pub id: SharedString,
    /// What the palette shows, such as "Next hunk".
    pub title: SharedString,
    /// The palette group, such as "Diff".
    pub category: SharedString,
    /// Default keys, in GPUI's syntax (`ctrl-shift-p`, `g g`).
    pub keys: Vec<SharedString>,
    /// Where the command works: a key context predicate (`DiffReview`). Its
    /// keys are bound there, and the palette lists it only when the focus
    /// was there. `None` is everywhere.
    pub context: Option<SharedString>,
    /// What running it dispatches, to where the focus was.
    pub action: Box<dyn Action>,
    /// Whether the palette lists it (the palette's own openers don't).
    pub in_palette: bool,
    pub origin: Origin,
}

impl Clone for Command {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            title: self.title.clone(),
            category: self.category.clone(),
            keys: self.keys.clone(),
            context: self.context.clone(),
            action: self.action.boxed_clone(),
            in_palette: self.in_palette,
            origin: self.origin.clone(),
        }
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("keys", &self.keys)
            .field("context", &self.context)
            .field("action", &self.action.name())
            .finish_non_exhaustive()
    }
}

impl Command {
    /// A core command `id` titled `title` in `category`, running `action`.
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        category: impl Into<SharedString>,
        action: impl Action,
    ) -> Self {
        Self::new_boxed(id, title, category, Box::new(action))
    }

    /// [`Command::new`] with a boxed action.
    pub fn new_boxed(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        category: impl Into<SharedString>,
        action: Box<dyn Action>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            category: category.into(),
            keys: Vec::new(),
            context: None,
            action,
            in_palette: true,
            origin: Origin::Core,
        }
    }

    /// Add a default key sequence (GPUI syntax).
    pub fn key(mut self, keys: impl Into<SharedString>) -> Self {
        self.keys.push(keys.into());
        self
    }

    /// Where the command works and its keys are bound.
    pub fn context(mut self, context: impl Into<SharedString>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Keep it out of the palette (it can still be bound).
    pub fn hidden(mut self) -> Self {
        self.in_palette = false;
        self
    }

    /// Whether `action` is this command's.
    pub fn runs(&self, action: &dyn Action) -> bool {
        self.action.partial_eq(action)
    }

    /// The type of its action, to match against the actions a focus path
    /// handles.
    pub fn action_type(&self) -> TypeId {
        self.action.as_any().type_id()
    }

    /// Whether its context matches `stack` (the contexts from the window
    /// down to the focus). A context that doesn't parse never does.
    pub fn matches_context(&self, stack: &[KeyContext]) -> bool {
        match &self.context {
            None => true,
            Some(context) => KeyBindingContextPredicate::parse(context)
                .is_ok_and(|p| p.depth_of(stack).is_some()),
        }
    }
}

/// The app's commands. A GPUI global; see the module docs.
#[derive(Default)]
pub struct CommandRegistry {
    commands: Vec<Command>,
    user: UserKeys,
    /// The user's keybindings file, once set.
    user_path: Option<PathBuf>,
    recent: Vec<SharedString>,
    /// Where the recently used list is kept, once set.
    recent_path: Option<PathBuf>,
    /// Whether the daemon's commands are in (or there is no daemon), so an
    /// unknown id in the user's file is really unknown.
    pub(crate) complete: bool,
    /// Problems already logged.
    reported: HashSet<String>,
    /// What [`report`] found last.
    problems: Vec<String>,
}

impl Global for CommandRegistry {}

impl std::fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandRegistry")
            .field("commands", &self.commands.len())
            .field("user_path", &self.user_path)
            .finish_non_exhaustive()
    }
}

/// A binding [`CommandRegistry::effective`] computed: command, keys and
/// context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub id: SharedString,
    pub keys: SharedString,
    pub context: Option<SharedString>,
}

impl CommandRegistry {
    /// The registry, if any command was registered.
    #[cfg(test)]
    pub fn try_get(cx: &App) -> Option<&Self> {
        cx.try_global::<Self>()
    }

    /// Add `command`, replacing the one with its id.
    pub fn add(&mut self, command: Command) {
        match self.commands.iter_mut().find(|c| c.id == command.id) {
            Some(existing) => *existing = command,
            None => self.commands.push(command),
        }
    }

    /// Remove the commands `f` picks.
    pub fn remove_where(&mut self, f: impl Fn(&Command) -> bool) {
        self.commands.retain(|c| !f(c));
    }

    /// Every command, in registration order.
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// Command `id`.
    pub fn get(&self, id: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.id == id)
    }

    /// The command that runs `action`, if any.
    pub fn find_action(&self, action: &dyn Action) -> Option<&Command> {
        self.commands.iter().find(|c| c.runs(action))
    }

    /// Use these overrides (from the user's file).
    pub fn set_user_keys(&mut self, user: UserKeys) {
        self.user = user;
    }

    /// The keys command `id` has now: the user's, or its defaults.
    pub fn keys_of(&self, id: &str) -> Vec<SharedString> {
        match self.user.overrides.get(id) {
            Some(keys) => keys.iter().map(|k| k.clone().into()).collect(),
            None => self.get(id).map(|c| c.keys.clone()).unwrap_or_default(),
        }
    }

    /// Whether the user's file sets keys for command `id`.
    pub fn is_overridden(&self, id: &str) -> bool {
        self.user.overrides.contains_key(id)
    }

    /// Whether the user's file overrides any command.
    pub fn has_overrides(&self) -> bool {
        !self.user.overrides.is_empty()
    }

    /// The user's keybindings file: where it was loaded from, or the
    /// default place.
    pub fn keys_path(&self) -> PathBuf {
        self.user_path.clone().unwrap_or_else(keyfile::path)
    }

    /// Every binding the commands have now. The user's come after the
    /// defaults, so at the same context depth they win.
    pub fn effective(&self) -> Vec<Bound> {
        let (mut defaults, mut users) = (Vec::new(), Vec::new());
        for c in &self.commands {
            let overridden = self.user.overrides.contains_key(c.id.as_ref());
            for keys in self.keys_of(&c.id) {
                let bound = Bound {
                    id: c.id.clone(),
                    keys,
                    context: c.context.clone(),
                };
                if overridden {
                    users.push(bound);
                } else {
                    defaults.push(bound);
                }
            }
        }
        defaults.extend(users);
        defaults
    }

    /// What's wrong with the keybindings: the file's problems, ids no
    /// command has (once the daemon's commands are in), and keys bound to
    /// more than one command in the same context.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = self.user.problems.clone();
        if self.complete {
            for id in self.user.overrides.keys() {
                if self.get(id).is_none() {
                    problems.push(format!("`{id}`: no command has this id"));
                }
            }
        }
        let mut by_keys: BTreeMap<(String, String), Vec<SharedString>> = BTreeMap::new();
        for b in self.effective() {
            let context = b
                .context
                .as_ref()
                .map(|c| c.to_string())
                .unwrap_or_default();
            let ids = by_keys.entry((b.keys.to_string(), context)).or_default();
            if !ids.contains(&b.id) {
                ids.push(b.id);
            }
        }
        for ((keys, context), ids) in by_keys {
            if ids.len() > 1 {
                let ids: Vec<String> = ids.iter().map(|id| format!("`{id}`")).collect();
                let place = if context.is_empty() {
                    String::new()
                } else {
                    format!(" in {context}")
                };
                problems.push(format!(
                    "`{keys}`{place} is bound to {}; the last one wins",
                    ids.join(" and ")
                ));
            }
        }
        problems
    }

    /// What [`report`] found last.
    pub fn last_problems(&self) -> &[String] {
        &self.problems
    }

    /// The recently used commands, most recent first.
    pub fn recent(&self) -> &[SharedString] {
        &self.recent
    }

    /// Note that command `id` ran: it goes to the front of the recent list,
    /// which is saved if it has a place.
    pub fn touch(&mut self, id: &SharedString) {
        self.recent.retain(|r| r != id);
        self.recent.insert(0, id.clone());
        self.recent.truncate(RECENT_MAX);
        if let Some(path) = &self.recent_path {
            let ids: Vec<&str> = self.recent.iter().map(|s| s.as_ref()).collect();
            let saved = serde_json::to_vec(&ids)
                .map_err(std::io::Error::other)
                .and_then(|json| {
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(path, json)
                });
            if let Err(e) = saved {
                eprintln!("coqo: cannot save recent commands: {e}");
            }
        }
    }

    /// Keep the recent list at `path` (and load what is there).
    pub fn set_recent_path(&mut self, path: PathBuf) {
        let ids: Vec<String> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        self.recent = ids.into_iter().map(SharedString::from).collect();
        self.recent.truncate(RECENT_MAX);
        self.recent_path = Some(path);
    }

    /// The bindings to put in the keymap, and what didn't parse.
    fn key_bindings(&self, cx: &App) -> (Vec<KeyBinding>, Vec<String>) {
        let (mut bindings, mut problems) = (Vec::new(), Vec::new());
        for b in self.effective() {
            let Some(command) = self.get(&b.id) else {
                continue;
            };
            match keys::load(
                &b.keys,
                command.action.boxed_clone(),
                b.context.as_deref(),
                cx,
            ) {
                Ok(binding) => bindings.push(binding),
                Err(e) => problems.push(format!("`{}`: {e}", b.id)),
            }
        }
        (bindings, problems)
    }
}

/// The registry, created on first use.
pub fn registry(cx: &mut App) -> &mut CommandRegistry {
    cx.default_global::<CommandRegistry>()
}

/// Add `commands` (replacing any with the same ids) and bind their keys.
pub fn register(cx: &mut App, commands: impl IntoIterator<Item = Command>) {
    let registry = registry(cx);
    for command in commands {
        registry.add(command);
    }
    apply(cx);
}

/// Rebuild the commands' bindings in the keymap (see the module docs).
pub fn apply(cx: &mut App) {
    let Some(registry) = cx.try_global::<CommandRegistry>() else {
        return;
    };
    let (bindings, problems) = registry.key_bindings(cx);
    let keymap = cx.key_bindings();
    {
        let mut keymap = keymap.borrow_mut();
        let kept: Vec<KeyBinding> = keymap
            .bindings()
            .filter(|b| registry.find_action(b.action()).is_none())
            .cloned()
            .collect();
        keymap.clear();
        keymap.add_bindings(kept);
        keymap.add_bindings(bindings);
    }
    if !problems.is_empty() {
        let registry = cx.global_mut::<CommandRegistry>();
        for p in problems {
            if !registry.user.problems.contains(&p) {
                registry.user.problems.push(p);
            }
        }
    }
    cx.refresh_windows();
}

/// Read the user's keybindings from `path` (and keep it for [`reload`]),
/// then rebind.
pub fn load_user_keys(path: PathBuf, cx: &mut App) {
    let user = keyfile::read(&path, cfg!(target_os = "macos"));
    let registry = registry(cx);
    registry.set_user_keys(user);
    registry.user_path = Some(path);
    apply(cx);
}

/// Read the user's keybindings file again, rebind, and report.
pub fn reload(cx: &mut App) {
    let Some(path) = cx
        .try_global::<CommandRegistry>()
        .and_then(|r| r.user_path.clone())
    else {
        return;
    };
    load_user_keys(path, cx);
    let registry = registry(cx);
    registry.reported.clear();
    report(cx);
}

/// Log the keybindings' problems not logged yet. Never fails: a bad entry
/// is skipped and the rest apply.
pub fn report(cx: &mut App) {
    let registry = registry(cx);
    let problems = registry.problems();
    for p in &problems {
        if registry.reported.insert(p.clone()) {
            eprintln!("coqo: keybindings: {p}");
        }
    }
    registry.problems = problems;
}

#[cfg(test)]
mod tests {
    use coqo_ui::gpui::{self as gpui, TestAppContext};

    use super::*;

    gpui::actions!(registry_test, [Alpha, Beta, Gamma]);

    fn sample() -> CommandRegistry {
        let mut r = CommandRegistry::default();
        r.add(Command::new("test.alpha", "Alpha", "Test", Alpha).key("ctrl-a"));
        r.add(
            Command::new("test.beta", "Beta", "Test", Beta)
                .key("j")
                .key("down")
                .context("View"),
        );
        r.add(Command::new("test.gamma", "Gamma", "Test", Gamma));
        r
    }

    #[test]
    fn a_command_with_the_same_id_replaces_the_old_one() {
        let mut r = sample();
        r.add(Command::new("test.alpha", "Alpha again", "Test", Alpha));
        assert_eq!(r.commands().len(), 3);
        assert_eq!(r.get("test.alpha").unwrap().title, "Alpha again");
        assert!(r.get("test.alpha").unwrap().keys.is_empty());
        assert_eq!(r.find_action(&Beta).unwrap().id, "test.beta");
        r.remove_where(|c| c.id == "test.beta");
        assert!(r.find_action(&Beta).is_none());
    }

    #[test]
    fn user_keys_override_defaults_by_id() {
        let mut r = sample();
        let mut user = UserKeys::default();
        user.overrides
            .insert("test.alpha".into(), vec!["ctrl-shift-a".into()]);
        user.overrides.insert("test.beta".into(), vec![]);
        user.overrides
            .insert("test.gamma".into(), vec!["g g".into()]);
        r.set_user_keys(user);
        assert_eq!(
            r.keys_of("test.alpha"),
            vec![SharedString::from("ctrl-shift-a")]
        );
        assert!(r.keys_of("test.beta").is_empty());
        let effective = r.effective();
        assert_eq!(
            effective,
            vec![
                Bound {
                    id: "test.alpha".into(),
                    keys: "ctrl-shift-a".into(),
                    context: None
                },
                Bound {
                    id: "test.gamma".into(),
                    keys: "g g".into(),
                    context: None
                },
            ]
        );
    }

    #[test]
    fn conflicts_and_unknown_ids_are_problems() {
        let mut r = sample();
        let mut user = UserKeys::default();
        user.overrides
            .insert("test.gamma".into(), vec!["ctrl-a".into()]);
        user.overrides
            .insert("test.nothing".into(), vec!["ctrl-n".into()]);
        user.problems.push("`x`: bad".into());
        r.set_user_keys(user);
        let problems = r.problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[1].contains("`test.alpha` and `test.gamma`"),
            "{problems:?}"
        );
        // Unknown ids wait until the daemon's commands are in.
        r.complete = true;
        let problems = r.problems();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`test.nothing`: no command")),
            "{problems:?}"
        );
        // The same keys in different contexts don't conflict.
        let mut r = sample();
        r.add(Command::new("test.delta", "Delta", "Test", Gamma).key("j"));
        assert!(r.problems().is_empty(), "{:?}", r.problems());
    }

    #[test]
    fn recently_used_commands_come_first_and_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recent.json");
        let mut r = sample();
        r.set_recent_path(path.clone());
        r.touch(&"test.beta".into());
        r.touch(&"test.alpha".into());
        r.touch(&"test.beta".into());
        assert_eq!(r.recent(), &["test.beta", "test.alpha"]);
        let mut again = CommandRegistry::default();
        again.set_recent_path(path);
        assert_eq!(again.recent(), &["test.beta", "test.alpha"]);
    }

    #[test]
    fn contexts_decide_where_a_command_works() {
        let r = sample();
        let stack = |names: &[&str]| -> Vec<KeyContext> {
            names
                .iter()
                .map(|n| KeyContext::parse(n).unwrap())
                .collect()
        };
        let beta = r.get("test.beta").unwrap();
        assert!(beta.matches_context(&stack(&["Root", "View"])));
        assert!(!beta.matches_context(&stack(&["Root", "Other"])));
        assert!(
            r.get("test.alpha")
                .unwrap()
                .matches_context(&stack(&["Root"]))
        );
    }

    #[test]
    fn apply_owns_the_commands_bindings() {
        let cx = TestAppContext::single();
        cx.update(|cx| {
            // Someone else bound Alpha; the registry takes it over.
            cx.bind_keys([
                coqo_ui::keys::binding("ctrl-q", Alpha, None),
                coqo_ui::keys::binding("ctrl-z", Gamma, Some("Other")),
            ]);
            let commands = sample().commands().to_vec();
            register(cx, commands);
            let names = |cx: &App| -> Vec<(String, String)> {
                let keymap = cx.key_bindings();
                let keymap = keymap.borrow();
                keymap
                    .bindings()
                    .map(|b| {
                        let keys: Vec<String> = b
                            .keystrokes()
                            .iter()
                            .map(|k| k.inner().unparse(cx.platform_info().os))
                            .collect();
                        (keys.join(" "), b.action().name().to_owned())
                    })
                    .collect()
            };
            assert_eq!(
                names(cx),
                vec![
                    ("ctrl-a".into(), "registry_test::Alpha".into()),
                    ("j".into(), "registry_test::Beta".into()),
                    ("down".into(), "registry_test::Beta".into()),
                ]
            );
            assert!(coqo_ui::keys::unguarded_bindings(cx).is_empty());

            let mut user = UserKeys::default();
            user.overrides
                .insert("test.alpha".into(), vec!["ctrl-b".into()]);
            user.overrides
                .insert("test.gamma".into(), vec!["ctrl-".into()]);
            registry(cx).set_user_keys(user);
            apply(cx);
            assert_eq!(
                names(cx),
                vec![
                    ("j".into(), "registry_test::Beta".into()),
                    ("down".into(), "registry_test::Beta".into()),
                    ("ctrl-b".into(), "registry_test::Alpha".into()),
                ]
            );
            report(cx);
            let problems = registry(cx).last_problems().to_vec();
            assert!(
                problems.iter().any(|p| p.contains("test.gamma")),
                "{problems:?}"
            );
        });
    }
}
