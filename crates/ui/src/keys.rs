// SPDX-License-Identifier: Apache-2.0

//! Key bindings that leave typing alone (#266).
//!
//! A view's shortcuts are bound to its key context, and a text field or code
//! editor inside the view sits in that context too. GPUI matches a binding
//! before the focused field gets the character, so a plain `j` bound to the
//! diff view would move to the next hunk instead of typing `j` in a comment.
//!
//! Every Coqo binding is made with [`binding`]. When its first keystroke is
//! *plain* ([`is_plain`]: letters, digits, `shift`+letter, arrows, `enter`,
//! `escape`, `backspace`, `space`…), its context gets `&& !Input`, so it never
//! fires while a text field or editor ([`TEXT_FIELD_CONTEXT`]) anywhere on the
//! focus path has focus. Shortcuts with `ctrl`, `alt`, `cmd` or `fn`, and
//! function keys, are left as they are and keep working while typing.
//!
//! GPUI evaluates `!Input` against the whole context stack from the window
//! down to the focused element, so the field may sit at any depth under the
//! view.
//!
//! [`unguarded_bindings`] lists the bindings that break the rule; a test runs
//! it over every binding the app registers.

use gpui_kit::{Action, App, Global, KeyBinding, KeyBindingContextPredicate, Keystroke};

/// The key context of every text field and code editor: gpui-component's
/// input (the code editor adds `mode=editor` to the same context).
pub const TEXT_FIELD_CONTEXT: &str = "Input";

/// Whether `keystroke` can type or edit text in a field: no `ctrl`, `alt`,
/// `cmd` or `fn` modifier (`shift` is allowed), and not a function key.
pub fn is_plain(keystroke: &Keystroke) -> bool {
    let m = &keystroke.modifiers;
    !(m.control || m.alt || m.platform || m.function) && !is_function_key(&keystroke.key)
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The context a binding of `keystrokes` in `context` should have: the same,
/// with `&& !Input` added when the first keystroke is plain.
pub fn guarded_context(keystrokes: &'static str, context: Option<&str>) -> Option<String> {
    let first = keystrokes.split(' ').next().unwrap_or_default();
    let plain = Keystroke::parse(first).is_ok_and(|k| is_plain(&k));
    match (plain, context) {
        (false, context) => context.map(str::to_owned),
        (true, None) => Some(format!("!{TEXT_FIELD_CONTEXT}")),
        (true, Some(context)) => Some(format!("({context}) && !{TEXT_FIELD_CONTEXT}")),
    }
}

/// A key binding of `action` to `keystrokes` in `context` that doesn't fire
/// while a text field inside `context` has focus, if its first keystroke is
/// plain (see the module docs). Use it for every binding instead of
/// `KeyBinding::new`.
pub fn binding<A: Action>(
    keystrokes: &'static str,
    action: A,
    context: Option<&str>,
) -> KeyBinding {
    KeyBinding::new(
        keystrokes,
        action,
        guarded_context(keystrokes, context).as_deref(),
    )
}

/// How many bindings the component library registered before Coqo's own:
/// those are the fields' own keys and are not checked.
struct LibraryBindings(usize);

impl Global for LibraryBindings {}

/// Note where Coqo's bindings start. Called by [`crate::init`] right after
/// the component library is initialised.
pub(crate) fn mark_library_bindings(cx: &mut App) {
    let count = cx.key_bindings().borrow().bindings().len();
    cx.set_global(LibraryBindings(count));
}

/// Coqo's bindings (registered after [`crate::init`] set up the component
/// library) whose first keystroke is plain and that would still fire while a
/// text field has focus, as `"keys" in context → action`. Bindings to
/// `NoAction` are checked too; bindings that only apply *inside* a text field
/// (their context requires `Input`) are allowed.
pub fn unguarded_bindings(cx: &App) -> Vec<String> {
    let start = cx
        .try_global::<LibraryBindings>()
        .map_or(0, |library| library.0);
    let os = cx.platform_info().os;
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap
        .bindings()
        .skip(start)
        .filter(|binding| {
            binding
                .keystrokes()
                .first()
                .is_some_and(|k| is_plain(k.inner()))
        })
        .filter(|binding| {
            !binding
                .predicate()
                .is_some_and(|p| excludes_text_field(&p) || requires_text_field(&p))
        })
        .map(|binding| {
            let keys: Vec<String> = binding
                .keystrokes()
                .iter()
                .map(|k| k.inner().unparse(os))
                .collect();
            let context = binding
                .predicate()
                .map_or_else(|| "any context".to_owned(), |p| p.to_string());
            format!(
                "\"{}\" in {context} → {}",
                keys.join(" "),
                binding.action().name()
            )
        })
        .collect()
}

fn is_text_field(p: &KeyBindingContextPredicate) -> bool {
    matches!(p, KeyBindingContextPredicate::Identifier(name) if name == TEXT_FIELD_CONTEXT)
}

/// Whether `p` can only match outside a text field.
fn excludes_text_field(p: &KeyBindingContextPredicate) -> bool {
    use KeyBindingContextPredicate::*;
    match p {
        Not(inner) => is_text_field(inner),
        And(a, b) => excludes_text_field(a) || excludes_text_field(b),
        Or(a, b) => excludes_text_field(a) && excludes_text_field(b),
        _ => false,
    }
}

/// Whether `p` can only match inside a text field (the field's own keys).
fn requires_text_field(p: &KeyBindingContextPredicate) -> bool {
    use KeyBindingContextPredicate::*;
    match p {
        Identifier(_) => is_text_field(p),
        And(a, b) => requires_text_field(a) || requires_text_field(b),
        Or(a, b) => requires_text_field(a) && requires_text_field(b),
        Descendant(_, child) => requires_text_field(child),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::gpui; // The action derive spells its paths `gpui::…`.

    fn plain(source: &'static str) -> bool {
        is_plain(&Keystroke::parse(source).unwrap())
    }

    #[test]
    fn typing_keys_are_plain_and_shortcuts_are_not() {
        for key in [
            "j",
            "shift-j",
            "J",
            "7",
            "space",
            "enter",
            "escape",
            "backspace",
            "delete",
            "tab",
            "up",
            "left",
            "home",
            "/",
            "shift-/",
        ] {
            assert!(plain(key), "{key} is plain");
        }
        for key in [
            "ctrl-j",
            "cmd-j",
            "alt-j",
            "ctrl-shift-a",
            "cmd-alt-n",
            "ctrl-`",
            "fn-left",
            "f8",
            "shift-f8",
            "f12",
        ] {
            assert!(!plain(key), "{key} is a shortcut");
        }
        // Keys named like function keys but not them.
        assert!(!is_function_key("f"));
        assert!(!is_function_key("fx"));
    }

    #[test]
    fn plain_keys_get_the_exclusion_and_shortcuts_keep_their_context() {
        assert_eq!(
            guarded_context("j", Some("DiffReview")).as_deref(),
            Some("(DiffReview) && !Input")
        );
        assert_eq!(guarded_context("enter", None).as_deref(), Some("!Input"));
        assert_eq!(
            guarded_context("ctrl-s", Some("FileEditor")).as_deref(),
            Some("FileEditor")
        );
        assert_eq!(guarded_context("f8", None), None);
        assert_eq!(
            guarded_context("g g", Some("A || B")).as_deref(),
            Some("(A || B) && !Input")
        );
    }

    fn predicate(source: &str) -> KeyBindingContextPredicate {
        KeyBindingContextPredicate::parse(source).unwrap()
    }

    #[test]
    fn the_guard_reads_the_predicate() {
        assert!(excludes_text_field(&predicate("(DiffReview) && !Input")));
        assert!(excludes_text_field(&predicate("!Input")));
        assert!(excludes_text_field(&predicate("!Input && (A || B)")));
        assert!(!excludes_text_field(&predicate("DiffReview")));
        assert!(!excludes_text_field(&predicate("A || !Input")));
        assert!(!excludes_text_field(&predicate("!(Input && A)")));
        assert!(requires_text_field(&predicate("Input")));
        assert!(requires_text_field(&predicate("Input && mode == editor")));
        assert!(requires_text_field(&predicate("Popover > Input")));
        assert!(!requires_text_field(&predicate("Input > Popover")));
        assert!(!requires_text_field(&predicate("Popover")));
    }

    /// GPUI's own matching, against the stacks the app builds: the exclusion
    /// holds with the field at any depth under the view, and a modified key
    /// still matches inside the field.
    #[test]
    fn gpui_skips_a_guarded_binding_inside_a_field() {
        use gpui_kit::KeyContext;
        let stack = |names: &[&str]| -> Vec<KeyContext> {
            names
                .iter()
                .map(|n| KeyContext::parse(n).unwrap())
                .collect()
        };
        let guarded = predicate("(DiffReview) && !Input");
        assert!(
            guarded
                .depth_of(&stack(&["Root", "Dock", "DiffReview"]))
                .is_some()
        );
        assert!(
            guarded
                .depth_of(&stack(&["Root", "DiffReview", "Diff"]))
                .is_some()
        );
        assert!(
            guarded
                .depth_of(&stack(&["Root", "DiffReview", "Input"]))
                .is_none()
        );
        assert!(
            guarded
                .depth_of(&stack(&[
                    "Root",
                    "WalkthroughPlayer",
                    "DiffReview",
                    "Input mode=editor"
                ]))
                .is_none()
        );
        let modified = predicate("FileEditor");
        assert!(
            modified
                .depth_of(&stack(&["Root", "FileEditor", "Input mode=editor"]))
                .is_some()
        );
    }

    gpui_kit::actions!(keys_test, [Probe]);

    #[test]
    fn bindings_made_with_the_helper_pass_the_guard() {
        let cx = gpui_kit::TestAppContext::single();
        cx.update(|cx| {
            mark_library_bindings(cx);
            cx.bind_keys([
                binding("j", Probe, Some("View")),
                binding("enter", Probe, None),
                binding("ctrl-j", Probe, Some("View")),
                binding("f8", Probe, None),
                KeyBinding::new("enter", Probe, Some("Input")),
            ]);
            assert_eq!(unguarded_bindings(cx), Vec::<String>::new());
            cx.bind_keys([
                KeyBinding::new("v", Probe, Some("View")),
                KeyBinding::new("escape", gpui_kit::NoAction, None),
            ]);
            let unguarded = unguarded_bindings(cx);
            assert_eq!(unguarded.len(), 2, "{unguarded:?}");
            assert_eq!(unguarded[0], "\"v\" in View → keys_test::Probe");
            assert!(unguarded[1].starts_with("\"escape\" in any context"));
        });
    }
}
