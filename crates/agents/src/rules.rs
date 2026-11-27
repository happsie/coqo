// SPDX-License-Identifier: Apache-2.0

//! Per-workspace permission rules (#65): what a permission request is about,
//! whether a rule may answer it, and which option a rule answers with.
//!
//! An allowlist model: a request is asked unless a rule matches it, and a
//! rule only ever matches a request Coqo can read exactly.
//!
//! - **Commands** (`execute`): a simple command line, split into words like
//!   a shell would, but refused outright if it has operators (`;`, `&&`,
//!   `|`), redirections, substitutions (`$`, backticks), globs, `~`, `..`, an
//!   absolute path outside the workspace, a flag that runs code (`-c`, `-e`,
//!   `-exec`, …) or a program from [`DANGEROUS`] (`rm`, `sudo`, `curl`, a
//!   shell, …). A rule is a word-wise prefix: `cargo test` matches
//!   `cargo test -p shop`, not `cargo testing`.
//! - **Files** (`edit`, `read`): every file the request touches (its diffs,
//!   locations and the raw input's path) must resolve, symlinks included,
//!   inside one of the workspace's roots. A rule is a glob over the
//!   root-relative path (`src/**`, `docs/*.md`).
//!
//! Anything else (deletes, moves, fetches, MCP tools, …) is always asked.
//! A reject rule wins over an allow rule. A rule answers with the matching
//! option by kind (`allow_once`, else `allow_always`; `reject_once`, else
//! `reject_always`), never by position, and asks if there is none.

use std::path::{Component, Path, PathBuf};

use coqo_protocol::methods::agents::{
    PermissionOption, PermissionOptionKind, PermissionRule, PermissionRuleSpec, RuleDecision,
    ToolCall, ToolCallContent, ToolCallLocation, ToolCallUpdate, ToolKind,
};
use serde_json::Value;

/// Programs a rule never runs, whatever its pattern. A backstop, not the
/// model: rules are an allowlist.
pub(crate) const DANGEROUS: &[&str] = &[
    // Deleting, overwriting, devices.
    "rm",
    "rmdir",
    "unlink",
    "shred",
    "dd",
    "mkfs",
    "fdisk",
    "mount",
    "umount",
    "truncate",
    // Privileges and ownership.
    "sudo",
    "su",
    "doas",
    "pkexec",
    "chmod",
    "chown",
    "chgrp",
    "chattr",
    // The network.
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "socat",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "ftp",
    "telnet",
    // Running other commands.
    "eval",
    "exec",
    "source",
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "env",
    "xargs",
    "nohup",
    "time",
    "timeout",
    "nice",
    "watch",
    "find",
    "osascript",
    "open",
    "xdg-open",
    // The system.
    "kill",
    "killall",
    "pkill",
    "reboot",
    "shutdown",
    "halt",
    "poweroff",
    "systemctl",
    "launchctl",
    "crontab",
    "at",
];

/// Flags that make a program run code given on the command line.
const CODE_FLAGS: &[&str] = &[
    "-c", "-e", "--eval", "--exec", "-exec", "-execdir", "-delete", "-ok", "-okdir",
];

/// Characters that make a word more than a literal outside quotes.
const SHELL_SPECIAL: &[char] = &[
    ';', '&', '|', '<', '>', '`', '$', '(', ')', '{', '}', '*', '?', '[', ']', '~', '!', '\\', '#',
    '\n', '\r',
];

/// A tool's command, from its raw input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// A shell command line (`"command": "cargo test"`).
    Line(String),
    /// An argument vector, run without a shell (`"command": ["cargo", "test"]`).
    Argv(Vec<String>),
}

/// What a tool call is, gathered from the call and its updates: what rules
/// look at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolFacts {
    pub kind: Option<ToolKind>,
    pub command: Option<Command>,
    /// The raw input's working directory.
    pub cwd: Option<String>,
    /// Paths in the raw input (`file_path`, `path`, …).
    raw_paths: Vec<String>,
    /// Paths of `diff` content.
    diff_paths: Vec<String>,
    /// Locations.
    location_paths: Vec<String>,
}

fn locations(l: &[ToolCallLocation]) -> Vec<String> {
    l.iter().map(|l| l.path.clone()).collect()
}

fn diff_paths(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Diff { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

impl ToolFacts {
    pub fn of_call(call: &ToolCall) -> Self {
        let mut facts = Self {
            kind: Some(call.kind),
            diff_paths: diff_paths(&call.content),
            location_paths: locations(&call.locations),
            ..Self::default()
        };
        if let Some(raw) = &call.raw_input {
            facts.set_raw(raw);
        }
        facts
    }

    #[cfg(test)]
    pub fn of_update(update: &ToolCallUpdate) -> Self {
        let mut facts = Self::default();
        facts.apply(update);
        facts
    }

    /// Fields the update has replace this call's.
    pub fn apply(&mut self, update: &ToolCallUpdate) {
        if let Some(kind) = update.kind {
            self.kind = Some(kind);
        }
        if let Some(content) = &update.content {
            self.diff_paths = diff_paths(content);
        }
        if let Some(l) = &update.locations {
            self.location_paths = locations(l);
        }
        if let Some(raw) = &update.raw_input {
            self.set_raw(raw);
        }
    }

    fn set_raw(&mut self, raw: &Value) {
        let str_of = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_owned);
        self.command = match raw.get("command").or_else(|| raw.get("cmd")) {
            Some(Value::String(s)) => Some(Command::Line(s.clone())),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .map(Command::Argv),
            _ => None,
        };
        self.cwd = str_of("cwd").or_else(|| str_of("workdir"));
        self.raw_paths = ["file_path", "path", "notebook_path", "abs_path"]
            .into_iter()
            .filter_map(str_of)
            .collect();
    }

    fn paths(&self) -> Vec<&str> {
        let mut all: Vec<&str> = self
            .raw_paths
            .iter()
            .chain(&self.diff_paths)
            .chain(&self.location_paths)
            .map(String::as_str)
            .collect();
        all.sort_unstable();
        all.dedup();
        all
    }
}

/// What a request does, in the terms rules match. Only built for requests a
/// rule may answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Subject {
    /// A simple command, as words.
    Command(Vec<String>),
    /// Edits or reads of files inside the workspace, as root-relative paths.
    Files { kind: ToolKind, paths: Vec<String> },
}

/// The workspace's roots, canonical.
pub(crate) fn canonical_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots
        .into_iter()
        .map(|r| r.canonicalize().unwrap_or(r))
        .collect()
}

impl Subject {
    /// What `facts` does, if a rule may answer it: a simple command run
    /// inside the workspace, or files inside `roots`. `cwd` is the
    /// session's working directory.
    pub fn of(facts: &ToolFacts, roots: &[PathBuf], cwd: &Path) -> Option<Self> {
        if roots.is_empty() {
            return None;
        }
        match facts.kind? {
            ToolKind::Execute => {
                let run_in = match &facts.cwd {
                    Some(dir) => resolve(dir, cwd)?,
                    None => resolve(&cwd.to_string_lossy(), Path::new("/"))?,
                };
                relative_to_roots(&run_in, roots)?;
                let words = match facts.command.as_ref()? {
                    Command::Line(line) => words(line)?,
                    Command::Argv(argv) => argv.clone(),
                };
                safe_command(&words, &run_in, roots).then_some(Self::Command(words))
            }
            kind @ (ToolKind::Edit | ToolKind::Read) => {
                let paths = facts.paths();
                if paths.is_empty() {
                    return None;
                }
                let paths = paths
                    .into_iter()
                    .map(|p| relative_to_roots(&resolve(p, cwd)?, roots))
                    .collect::<Option<Vec<_>>>()?;
                Some(Self::Files { kind, paths })
            }
            _ => None,
        }
    }

    /// Whether `rule` matches it.
    pub fn matches(&self, rule: &PermissionRuleSpec) -> bool {
        match self {
            Self::Command(words) => {
                let prefix: Vec<&str> = rule.pattern.split_whitespace().collect();
                rule.tool == ToolKind::Execute
                    && !prefix.is_empty()
                    && prefix.len() <= words.len()
                    && prefix.iter().zip(words).all(|(p, w)| p == w)
            }
            Self::Files { kind, paths } => {
                rule.tool == *kind
                    && !rule.pattern.is_empty()
                    && paths.iter().all(|p| glob(&rule.pattern, p))
            }
        }
    }

    /// The rule "remember for this workspace" creates for it.
    pub fn suggest(&self) -> Option<PermissionRuleSpec> {
        match self {
            Self::Command(words) => {
                let word_like = |w: &str| {
                    let mut chars = w.chars();
                    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                        && chars.all(|c| c.is_ascii_alphanumeric() || "_:-".contains(c))
                };
                let mut prefix = vec![words.first()?.clone()];
                if let Some(sub) = words.get(1).filter(|w| word_like(w)) {
                    prefix.push(sub.clone());
                    if ["run", "exec", "x", "dlx"].contains(&sub.as_str())
                        && let Some(script) = words.get(2).filter(|w| word_like(w))
                    {
                        prefix.push(script.clone());
                    }
                }
                Some(PermissionRuleSpec {
                    tool: ToolKind::Execute,
                    pattern: prefix.join(" "),
                })
            }
            Self::Files { kind, paths } => {
                let dir = common_dir(paths);
                let pattern = if !dir.is_empty() {
                    format!("{dir}/**")
                } else if let [only] = paths.as_slice() {
                    only.clone()
                } else {
                    return None;
                };
                Some(PermissionRuleSpec {
                    tool: *kind,
                    pattern,
                })
            }
        }
    }
}

/// The rule that answers a request about `subject`, and the option it
/// answers with. Reject rules first.
pub(crate) fn decide<'a>(
    rules: &'a [PermissionRule],
    subject: &Subject,
    options: &[PermissionOption],
) -> Option<(&'a PermissionRule, String)> {
    [RuleDecision::Reject, RuleDecision::Allow]
        .into_iter()
        .find_map(|decision| {
            let rule = rules
                .iter()
                .find(|r| r.decision == decision && subject.matches(&r.spec()))?;
            Some((rule, option_for(decision, options)?.id.clone()))
        })
}

/// The option a decision answers with: the "once" kind, else the "always"
/// kind.
pub(crate) fn option_for(
    decision: RuleDecision,
    options: &[PermissionOption],
) -> Option<&PermissionOption> {
    let kinds = match decision {
        RuleDecision::Allow => [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ],
        RuleDecision::Reject => [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ],
    };
    kinds
        .into_iter()
        .find_map(|k| options.iter().find(|o| o.kind == k))
}

/// The decision choosing an option of `kind` means.
pub(crate) fn decision_of(kind: PermissionOptionKind) -> RuleDecision {
    match kind {
        PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => RuleDecision::Allow,
        PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => {
            RuleDecision::Reject
        }
    }
}

// ---- commands ------------------------------------------------------------------

/// Splits a command line into words, with `'…'` and `"…"` quoting. `None`
/// unless it is one simple command made of literal words.
pub(crate) fn words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if let Some(w) = word.take() {
                    words.push(w);
                }
            }
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        '\n' | '\r' => return None,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' | '!' | '\n' | '\r' => return None,
                        c => w.push(c),
                    }
                }
            }
            c if SHELL_SPECIAL.contains(&c) => return None,
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    (!words.is_empty()).then_some(words)
}

/// The program's name without its directory.
fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether a rule may run `words` in `cwd`: no dangerous program, no
/// environment assignment, no code flags, and every path inside `roots`.
fn safe_command(words: &[String], cwd: &Path, roots: &[PathBuf]) -> bool {
    let Some(program) = words.first() else {
        return false;
    };
    let name = program_name(program);
    if name.is_empty()
        || program.contains('=')
        || DANGEROUS.contains(&name)
        || name.starts_with("mkfs.")
    {
        return false;
    }
    words.iter().all(|w| {
        if CODE_FLAGS.contains(&w.as_str()) || w.contains('\0') {
            return false;
        }
        // `--manifest-path=/x` and `/x` alike.
        let value = w.split_once('=').map_or(w.as_str(), |(_, v)| v);
        if has_parent_dir(w) || has_parent_dir(value) {
            return false;
        }
        if value.starts_with('/') {
            return resolve(value, cwd).is_some_and(|p| relative_to_roots(&p, roots).is_some());
        }
        true
    })
}

fn has_parent_dir(s: &str) -> bool {
    s.split('/').any(|seg| seg == "..")
}

// ---- paths ---------------------------------------------------------------------

/// `path` (absolute, or relative to `cwd`) without `.`, with symlinks
/// resolved as far as it exists. `None` with a `..`.
fn resolve(path: &str, cwd: &Path) -> Option<PathBuf> {
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut clean = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => clean.push(c.as_os_str()),
            Component::CurDir => {}
            Component::Normal(n) => clean.push(n),
            Component::ParentDir => return None,
        }
    }
    if !clean.is_absolute() {
        return None;
    }
    // Canonicalize the longest prefix that exists; what is left does not
    // exist yet, so it holds no symlink.
    let mut rest = Vec::new();
    let mut base = clean.as_path();
    loop {
        if let Ok(real) = base.canonicalize() {
            let mut real = real;
            for n in rest.iter().rev() {
                real.push(n);
            }
            return Some(real);
        }
        rest.push(base.file_name()?.to_owned());
        base = base.parent()?;
    }
}

/// `path` relative to the root that holds it (`/`-separated, `""` for a
/// root itself).
fn relative_to_roots(path: &Path, roots: &[PathBuf]) -> Option<String> {
    roots
        .iter()
        .filter_map(|r| path.strip_prefix(r).ok())
        .min_by_key(|rel| rel.components().count())
        .map(|rel| {
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
}

/// The directory every path is in (`""` if none).
fn common_dir(paths: &[String]) -> String {
    let mut common: Option<Vec<&str>> = None;
    for p in paths {
        let mut segs: Vec<&str> = p.split('/').collect();
        segs.pop();
        common = Some(match common {
            None => segs,
            Some(c) => c
                .into_iter()
                .zip(segs)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect(),
        });
    }
    common.unwrap_or_default().join("/")
}

/// Whether `/`-separated `path` matches `pattern`: `**` is any number of
/// segments, `*` any run of characters within one, `?` one character.
pub(crate) fn glob(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    };
    glob_segments(&pat, &segs)
}

fn glob_segments(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|i| glob_segments(rest, &segs[i..])),
        Some((p, rest)) => segs.split_first().is_some_and(|(s, more)| {
            segment(p.as_bytes(), s.as_bytes()) && glob_segments(rest, more)
        }),
    }
}

fn segment(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| segment(rest, &s[i..])),
        Some((b'?', rest)) => !s.is_empty() && segment(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && segment(rest, &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_protocol::methods::agents::PermissionRuleId;
    use serde_json::json;

    fn w(line: &str) -> Option<Vec<String>> {
        words(line)
    }

    #[test]
    fn command_lines_split_into_literal_words_or_not_at_all() {
        assert_eq!(
            w("cargo test -p shop").unwrap(),
            ["cargo", "test", "-p", "shop"]
        );
        assert_eq!(
            w(r#"git commit -m "fix: the cart" --author='A B'"#).unwrap(),
            ["git", "commit", "-m", "fix: the cart", "--author=A B"]
        );
        for bad in [
            "cargo test; rm -rf /",
            "cargo test && echo",
            "cargo test || true",
            "cargo test | tee log",
            "cargo test > out",
            "cat < in",
            "echo $HOME",
            "echo `id`",
            "echo $(id)",
            "ls *.rs",
            "ls ~/x",
            "cargo test &",
            "echo \"$HOME\"",
            "echo 'unterminated",
            "a\nb",
            "echo \\x",
            "cargo test # comment",
            "",
            "   ",
        ] {
            assert_eq!(w(bad), None, "{bad:?}");
        }
    }

    fn roots(dir: &Path) -> Vec<PathBuf> {
        canonical_roots(vec![dir.to_path_buf()])
    }

    fn exec(command: &str) -> ToolFacts {
        ToolFacts::of_update(&ToolCallUpdate {
            kind: Some(ToolKind::Execute),
            raw_input: Some(json!({ "command": command, "description": "x" })),
            ..ToolCallUpdate::default()
        })
    }

    fn edit(paths: &[&str]) -> ToolFacts {
        ToolFacts::of_update(&ToolCallUpdate {
            kind: Some(ToolKind::Edit),
            content: Some(
                paths
                    .iter()
                    .map(|p| ToolCallContent::Diff {
                        path: (*p).into(),
                        old_text: None,
                        new_text: "x".into(),
                    })
                    .collect(),
            ),
            ..ToolCallUpdate::default()
        })
    }

    #[test]
    fn only_simple_commands_inside_the_workspace_are_subjects() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = roots(tmp.path());
        let cwd = ws[0].clone();
        let inside = cwd.join("Cargo.toml").display().to_string();
        let of = |c: &str| Subject::of(&exec(c), &ws, &cwd);
        assert_eq!(
            of("cargo test -p shop"),
            Some(Subject::Command(
                ["cargo", "test", "-p", "shop"].map(String::from).to_vec()
            ))
        );
        assert!(of(&format!("cargo test --manifest-path={inside}")).is_some());
        assert!(of("./gradlew test").is_some());
        for bad in [
            "rm -rf build",
            "/bin/rm -rf build",
            "sudo cargo test",
            "curl https://example.com",
            "bash -c 'cargo test'",
            "python -c 'print(1)'",
            "git -c core.pager=x log",
            "FOO=1 cargo test",
            "cargo test --manifest-path=/etc/Cargo.toml",
            "cat /etc/passwd",
            "cat ../secret",
            "cargo test; rm -rf /",
            "mkfs.ext4 /dev/sda",
        ] {
            assert_eq!(of(bad), None, "{bad:?}");
        }
        // Run somewhere else.
        let elsewhere = ToolFacts {
            cwd: Some("/".into()),
            ..exec("cargo test")
        };
        assert_eq!(Subject::of(&elsewhere, &ws, &cwd), None);
        // No workspace, no rules.
        assert_eq!(Subject::of(&exec("cargo test"), &[], &cwd), None);
        // An argument vector skips the shell, so quoting is not an issue.
        let argv = ToolFacts::of_update(&ToolCallUpdate {
            kind: Some(ToolKind::Execute),
            raw_input: Some(json!({ "command": ["cargo", "test", "a b"] })),
            ..ToolCallUpdate::default()
        });
        assert!(Subject::of(&argv, &ws, &cwd).is_some());
    }

    #[test]
    fn files_must_resolve_inside_the_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = roots(tmp.path());
        let root = ws[0].clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let p = |rel: &str| root.join(rel).display().to_string();
        assert_eq!(
            Subject::of(&edit(&[&p("src/a.rs"), &p("src/new/b.rs")]), &ws, &root),
            Some(Subject::Files {
                kind: ToolKind::Edit,
                paths: vec!["src/a.rs".into(), "src/new/b.rs".into()],
            })
        );
        // Relative to the session's directory.
        assert!(Subject::of(&edit(&["src/a.rs"]), &ws, &root).is_some());
        assert_eq!(Subject::of(&edit(&["/etc/passwd"]), &ws, &root), None);
        assert_eq!(
            Subject::of(&edit(&[&p("src/../../x")]), &ws, &root),
            None,
            "no .."
        );
        assert_eq!(
            Subject::of(&edit(&[&p("src/a.rs"), "/tmp/x"]), &ws, &root),
            None,
            "every path"
        );
        assert_eq!(Subject::of(&edit(&[]), &ws, &root), None);
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
            assert_eq!(
                Subject::of(&edit(&[&p("link/x.rs")]), &ws, &root),
                None,
                "a symlink out of the workspace"
            );
        }
        let delete = ToolFacts {
            kind: Some(ToolKind::Delete),
            ..edit(&[&p("src/a.rs")])
        };
        assert_eq!(Subject::of(&delete, &ws, &root), None, "never deletes");
    }

    fn rule(id: u64, tool: ToolKind, pattern: &str, decision: RuleDecision) -> PermissionRule {
        PermissionRule {
            id: PermissionRuleId(id),
            workspace: "ws".into(),
            tool,
            pattern: pattern.into(),
            decision,
            created_at: 0,
        }
    }

    fn cmd(words: &[&str]) -> Subject {
        Subject::Command(words.iter().map(|s| (*s).to_owned()).collect())
    }

    fn spec(tool: ToolKind, pattern: &str) -> PermissionRuleSpec {
        PermissionRuleSpec {
            tool,
            pattern: pattern.into(),
        }
    }

    #[test]
    fn rules_match_word_prefixes_and_globs() {
        let test = spec(ToolKind::Execute, "cargo test");
        assert!(cmd(&["cargo", "test"]).matches(&test));
        assert!(cmd(&["cargo", "test", "-p", "shop"]).matches(&test));
        assert!(!cmd(&["cargo", "testing"]).matches(&test));
        assert!(!cmd(&["cargo"]).matches(&test));
        assert!(!cmd(&["cargo", "test"]).matches(&spec(ToolKind::Edit, "cargo test")));
        assert!(!cmd(&["cargo"]).matches(&spec(ToolKind::Execute, " ")));

        let files = |kind, paths: &[&str]| Subject::Files {
            kind,
            paths: paths.iter().map(|s| (*s).to_owned()).collect(),
        };
        let src = spec(ToolKind::Edit, "src/**");
        assert!(files(ToolKind::Edit, &["src/a.rs", "src/x/y/z.rs"]).matches(&src));
        assert!(!files(ToolKind::Edit, &["src/a.rs", "tests/a.rs"]).matches(&src));
        assert!(!files(ToolKind::Read, &["src/a.rs"]).matches(&src));
        assert!(files(ToolKind::Read, &["docs/a.md"]).matches(&spec(ToolKind::Read, "docs/*.md")));
        assert!(
            !files(ToolKind::Read, &["docs/x/a.md"]).matches(&spec(ToolKind::Read, "docs/*.md"))
        );
    }

    #[test]
    fn globs() {
        assert!(glob("**", "a/b/c"));
        assert!(glob("src/**", "src/a.rs"));
        assert!(glob("src/**", "src/a/b.rs"));
        assert!(!glob("src/**", "srcx/a.rs"));
        assert!(glob("**/*.rs", "a.rs"));
        assert!(glob("**/*.rs", "x/y/a.rs"));
        assert!(!glob("**/*.rs", "x/y/a.ts"));
        assert!(glob("a/**/c", "a/c"));
        assert!(glob("a/**/c", "a/b/b/c"));
        assert!(glob("?.md", "a.md"));
        assert!(!glob("?.md", "ab.md"));
        assert!(glob("Cargo.toml", "Cargo.toml"));
        assert!(!glob("Cargo.toml", "x/Cargo.toml"));
    }

    #[test]
    fn suggestions_are_narrow() {
        let s = |words: &[&str]| cmd(words).suggest().unwrap().pattern;
        assert_eq!(s(&["cargo", "test", "-p", "shop"]), "cargo test");
        assert_eq!(s(&["cargo", "--version"]), "cargo");
        assert_eq!(s(&["npm", "run", "build", "--", "x"]), "npm run build");
        assert_eq!(s(&["uv", "run", "pytest"]), "uv run pytest");
        assert_eq!(s(&["./gradlew", "test"]), "./gradlew test");
        assert_eq!(s(&["python", "script.py"]), "python");

        let f = |paths: &[&str]| {
            Subject::Files {
                kind: ToolKind::Edit,
                paths: paths.iter().map(|s| (*s).to_owned()).collect(),
            }
            .suggest()
            .map(|s| s.pattern)
        };
        assert_eq!(f(&["src/cart.rs"]).as_deref(), Some("src/**"));
        assert_eq!(f(&["src/a/x.rs", "src/b.rs"]).as_deref(), Some("src/**"));
        assert_eq!(f(&["Cargo.toml"]).as_deref(), Some("Cargo.toml"));
        assert_eq!(f(&["Cargo.toml", "src/a.rs"]), None);
    }

    fn opt(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption {
            id: id.into(),
            name: id.into(),
            kind,
        }
    }

    #[test]
    fn decisions_choose_options_by_kind_and_rejects_win() {
        let options = [
            opt("no", PermissionOptionKind::RejectOnce),
            opt("always", PermissionOptionKind::AllowAlways),
            opt("yes", PermissionOptionKind::AllowOnce),
        ];
        let allow = rule(1, ToolKind::Execute, "cargo", RuleDecision::Allow);
        let reject = rule(2, ToolKind::Execute, "cargo publish", RuleDecision::Reject);
        let rules = [allow.clone(), reject.clone()];

        let (r, option) = decide(&rules, &cmd(&["cargo", "test"]), &options).unwrap();
        assert_eq!((r.id, option.as_str()), (allow.id, "yes"));
        let (r, option) = decide(&rules, &cmd(&["cargo", "publish"]), &options).unwrap();
        assert_eq!((r.id, option.as_str()), (reject.id, "no"));
        assert!(decide(&rules, &cmd(&["make"]), &options).is_none());

        // Only "allow always" on offer: use it rather than ask.
        let (_, option) = decide(&rules, &cmd(&["cargo", "b"]), &options[..2]).unwrap();
        assert_eq!(option, "always");
        // No allow option at all: ask.
        assert!(decide(&rules, &cmd(&["cargo", "b"]), &options[..1]).is_none());

        assert_eq!(
            decision_of(PermissionOptionKind::RejectAlways),
            RuleDecision::Reject
        );
        assert_eq!(
            decision_of(PermissionOptionKind::AllowOnce),
            RuleDecision::Allow
        );
    }

    #[test]
    fn facts_merge_the_call_and_its_updates() {
        let mut facts = ToolFacts::of_call(&ToolCall {
            id: Default::default(),
            title: "Bash".into(),
            kind: ToolKind::Execute,
            status: Default::default(),
            content: vec![],
            locations: vec![],
            raw_input: Some(json!({})),
            raw_output: None,
        });
        assert_eq!(facts.command, None);
        facts.apply(&ToolCallUpdate {
            raw_input: Some(json!({"command": "cargo test", "cwd": "/w"})),
            ..ToolCallUpdate::default()
        });
        assert_eq!(facts.kind, Some(ToolKind::Execute));
        assert_eq!(facts.command, Some(Command::Line("cargo test".into())));
        assert_eq!(facts.cwd.as_deref(), Some("/w"));
        let mut e = edit(&["/w/a.rs"]);
        e.apply(&ToolCallUpdate {
            raw_input: Some(json!({"file_path": "/w/a.rs"})),
            locations: Some(vec![ToolCallLocation {
                path: "/w/b.rs".into(),
                line: None,
            }]),
            ..ToolCallUpdate::default()
        });
        assert_eq!(e.paths(), ["/w/a.rs", "/w/b.rs"]);
    }
}
