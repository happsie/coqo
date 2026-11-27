// SPDX-License-Identifier: Apache-2.0

//! Shell integration basics: picks the OSC sequences the emulator ignores out
//! of the PTY byte stream. `alacritty_terminal` handles titles (OSC 0/2) and
//! colours itself; this sees the working directory (OSC 7) and finished
//! commands (OSC 133;D, FinalTerm) when the shell reports them.

/// A shell report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellReport {
    /// OSC 7 `file://host/path`: the working directory.
    Cwd(String),
    /// OSC 133;D[;code]: a command finished.
    CommandFinished(Option<i32>),
}

/// Longest OSC payload kept; longer ones are skipped.
const MAX_PAYLOAD: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc,
    /// Saw `ESC` inside an OSC: `\` ends it (ST).
    OscEscape,
}

/// Incremental scanner; sequences may be split across reads.
#[derive(Debug, Default)]
pub struct OscScanner {
    state: State,
    payload: Vec<u8>,
    overflow: bool,
}

impl OscScanner {
    /// Scan `bytes`, returning the reports completed in them.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<ShellReport> {
        let mut reports = Vec::new();
        for &b in bytes {
            self.state = match (self.state, b) {
                (State::Ground, 0x1b) => State::Escape,
                (State::Ground, _) => State::Ground,
                (State::Escape, b']') => {
                    self.payload.clear();
                    self.overflow = false;
                    State::Osc
                }
                (State::Escape, 0x1b) => State::Escape,
                (State::Escape, _) => State::Ground,
                (State::Osc, 0x07) => {
                    self.finish(&mut reports);
                    State::Ground
                }
                (State::Osc, 0x1b) => State::OscEscape,
                (State::Osc, 0x18 | 0x1a) => State::Ground, // CAN / SUB abort
                (State::Osc, b) => {
                    self.push(b);
                    State::Osc
                }
                (State::OscEscape, b'\\') => {
                    self.finish(&mut reports);
                    State::Ground
                }
                (State::OscEscape, b']') => {
                    // An unterminated OSC followed by a new one.
                    self.payload.clear();
                    self.overflow = false;
                    State::Osc
                }
                (State::OscEscape, _) => State::Ground,
            };
        }
        reports
    }

    fn push(&mut self, b: u8) {
        if self.payload.len() < MAX_PAYLOAD {
            self.payload.push(b);
        } else {
            self.overflow = true;
        }
    }

    fn finish(&mut self, reports: &mut Vec<ShellReport>) {
        if !self.overflow
            && let Some(report) = parse(&self.payload)
        {
            reports.push(report);
        }
        self.payload.clear();
    }
}

fn parse(payload: &[u8]) -> Option<ShellReport> {
    let text = std::str::from_utf8(payload).ok()?;
    if let Some(url) = text.strip_prefix("7;") {
        return cwd_from_url(url).map(ShellReport::Cwd);
    }
    let rest = text.strip_prefix("133;D")?;
    let code = rest
        .strip_prefix(';')
        .and_then(|r| r.split(';').next())
        .and_then(|c| c.trim().parse().ok());
    Some(ShellReport::CommandFinished(code))
}

/// `file://host/some%20path` → `/some path`. The host is ignored: the PTY is
/// local to the daemon.
fn cwd_from_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let decoded = percent_decode(path)?;
    (!decoded.is_empty()).then_some(decoded)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cwd_and_command_exit_codes() {
        let mut s = OscScanner::default();
        let reports = s.scan(b"hi\x1b]7;file://box/home/me/My%20Code\x07\x1b]133;D;2\x1b\\");
        assert_eq!(
            reports,
            vec![
                ShellReport::Cwd("/home/me/My Code".into()),
                ShellReport::CommandFinished(Some(2)),
            ]
        );
        assert_eq!(
            s.scan(b"\x1b]133;D\x07"),
            vec![ShellReport::CommandFinished(None)]
        );
    }

    #[test]
    fn sequences_may_be_split_across_reads() {
        let mut s = OscScanner::default();
        assert!(s.scan(b"\x1b]7;file:").is_empty());
        assert!(s.scan(b"///tmp").is_empty());
        assert_eq!(s.scan(b"\x07"), vec![ShellReport::Cwd("/tmp".into())]);
    }

    #[test]
    fn ignores_other_sequences() {
        let mut s = OscScanner::default();
        assert!(
            s.scan(b"\x1b]0;title\x07\x1b[31mred\x1b]133;A\x07\x1b]7;not-a-url\x07")
                .is_empty()
        );
    }
}
