// SPDX-License-Identifier: Apache-2.0

//! Keystrokes to the bytes an xterm-compatible terminal sends.

use coqo_protocol::methods::terminal::Modes;
use coqo_ui::gpui::Keystroke;

/// The xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl, or `None`
/// without modifiers.
fn modifier_param(k: &Keystroke) -> Option<u8> {
    let m = &k.modifiers;
    let n = u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    (n > 0).then_some(n + 1)
}

/// `CSI 1;m X` with modifiers; `SS3 X` / `CSI X` without.
fn cursor_key(k: &Keystroke, modes: &Modes, letter: char) -> String {
    match modifier_param(k) {
        Some(m) => format!("\x1b[1;{m}{letter}"),
        None if modes.app_cursor => format!("\x1bO{letter}"),
        None => format!("\x1b[{letter}"),
    }
}

/// `CSI n ~` with an optional modifier.
fn tilde_key(k: &Keystroke, n: u8) -> String {
    match modifier_param(k) {
        Some(m) => format!("\x1b[{n};{m}~"),
        None => format!("\x1b[{n}~"),
    }
}

/// The bytes for `k`, or `None` if the terminal doesn't take it (keys with
/// the platform modifier are the app's: copy, paste, close).
pub fn encode(k: &Keystroke, modes: &Modes) -> Option<String> {
    let m = &k.modifiers;
    if m.platform {
        return None;
    }
    let special = match k.key.as_str() {
        "enter" => Some(if m.alt { "\x1b\r".into() } else { "\r".into() }),
        "tab" if m.shift => Some("\x1b[Z".into()),
        "tab" => Some("\t".into()),
        "escape" => Some("\x1b".into()),
        "backspace" if m.control => Some("\x08".into()),
        "backspace" if m.alt => Some("\x1b\x7f".into()),
        "backspace" => Some("\x7f".into()),
        "space" if m.control => Some("\0".into()),
        "up" => Some(cursor_key(k, modes, 'A')),
        "down" => Some(cursor_key(k, modes, 'B')),
        "right" => Some(cursor_key(k, modes, 'C')),
        "left" => Some(cursor_key(k, modes, 'D')),
        "home" => Some(cursor_key(k, modes, 'H')),
        "end" => Some(cursor_key(k, modes, 'F')),
        "insert" => Some(tilde_key(k, 2)),
        "delete" => Some(tilde_key(k, 3)),
        "pageup" => Some(tilde_key(k, 5)),
        "pagedown" => Some(tilde_key(k, 6)),
        "f1" => Some(cursor_key_ss3(k, 'P')),
        "f2" => Some(cursor_key_ss3(k, 'Q')),
        "f3" => Some(cursor_key_ss3(k, 'R')),
        "f4" => Some(cursor_key_ss3(k, 'S')),
        "f5" => Some(tilde_key(k, 15)),
        "f6" => Some(tilde_key(k, 17)),
        "f7" => Some(tilde_key(k, 18)),
        "f8" => Some(tilde_key(k, 19)),
        "f9" => Some(tilde_key(k, 20)),
        "f10" => Some(tilde_key(k, 21)),
        "f11" => Some(tilde_key(k, 23)),
        "f12" => Some(tilde_key(k, 24)),
        _ => None,
    };
    if special.is_some() {
        return special;
    }

    let mut chars = k.key.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    if m.control
        && let Some(byte) = single.and_then(control_byte)
    {
        let byte = char::from(byte).to_string();
        return Some(if m.alt { format!("\x1b{byte}") } else { byte });
    }
    // Typed text: what the keyboard layout produced, else the key itself.
    let text = k
        .key_char
        .clone()
        .or_else(|| (k.key == "space").then(|| " ".to_owned()))
        .or_else(|| {
            single.map(|c| {
                if m.shift {
                    c.to_uppercase().collect()
                } else {
                    c.into()
                }
            })
        })?;
    Some(if m.alt { format!("\x1b{text}") } else { text })
}

/// F1–F4: `SS3 X`, or `CSI 1;m X` with modifiers.
fn cursor_key_ss3(k: &Keystroke, letter: char) -> String {
    match modifier_param(k) {
        Some(m) => format!("\x1b[1;{m}{letter}"),
        None => format!("\x1bO{letter}"),
    }
}

/// Ctrl+key as a C0 control byte.
fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | '2' | ' ' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// Pasted text as the program expects it: line breaks as CR, wrapped for
/// bracketed paste when the program asked for it.
pub fn paste(text: &str, modes: &Modes) -> String {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if modes.bracketed_paste {
        // A paste must not end the bracket early.
        let text = text.replace("\x1b[201~", "");
        format!("\x1b[200~{text}\x1b[201~")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &'static str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    fn enc(s: &'static str) -> Option<String> {
        encode(&key(s), &Modes::default())
    }

    #[test]
    fn control_keys_become_c0_bytes() {
        assert_eq!(enc("ctrl-c").as_deref(), Some("\x03"));
        assert_eq!(enc("ctrl-d").as_deref(), Some("\x04"));
        assert_eq!(enc("ctrl-w").as_deref(), Some("\x17"));
        assert_eq!(enc("ctrl-\\").as_deref(), Some("\x1c"));
        assert_eq!(enc("ctrl-alt-b").as_deref(), Some("\x1b\x02"));
        assert_eq!(enc("enter").as_deref(), Some("\r"));
        assert_eq!(enc("backspace").as_deref(), Some("\x7f"));
        assert_eq!(enc("escape").as_deref(), Some("\x1b"));
        assert_eq!(enc("shift-tab").as_deref(), Some("\x1b[Z"));
    }

    #[test]
    fn arrows_follow_the_cursor_key_mode() {
        assert_eq!(enc("up").as_deref(), Some("\x1b[A"));
        assert_eq!(enc("ctrl-left").as_deref(), Some("\x1b[1;5D"));
        assert_eq!(enc("shift-end").as_deref(), Some("\x1b[1;2F"));
        let app = Modes {
            app_cursor: true,
            ..Modes::default()
        };
        assert_eq!(encode(&key("down"), &app).as_deref(), Some("\x1bOB"));
        assert_eq!(enc("pageup").as_deref(), Some("\x1b[5~"));
        assert_eq!(enc("f1").as_deref(), Some("\x1bOP"));
        assert_eq!(enc("f12").as_deref(), Some("\x1b[24~"));
    }

    #[test]
    fn text_keys_send_their_characters() {
        assert_eq!(enc("a").as_deref(), Some("a"));
        assert_eq!(enc("shift-a").as_deref(), Some("A"));
        assert_eq!(enc("alt-f").as_deref(), Some("\x1bf"));
        assert_eq!(enc("space").as_deref(), Some(" "));
        let mut typed = key("2");
        typed.key_char = Some("é".into());
        assert_eq!(encode(&typed, &Modes::default()).as_deref(), Some("é"));
        assert_eq!(enc("cmd-v"), None);
    }

    #[test]
    fn pastes_use_cr_and_brackets() {
        assert_eq!(paste("a\nb\r\nc", &Modes::default()), "a\rb\rc");
        let modes = Modes {
            bracketed_paste: true,
            ..Modes::default()
        };
        assert_eq!(paste("x\x1b[201~y", &modes), "\x1b[200~xy\x1b[201~");
    }
}
