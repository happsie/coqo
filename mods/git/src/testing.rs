// SPDX-License-Identifier: Apache-2.0

//! Fixture repositories for tests (here, in the daemon and in the UI):
//! `git init` in a folder, with local config that pins what would make
//! `git`'s output depend on the machine, and commits with fixed dates.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};

/// A repository made by the `git` CLI, for tests.
#[derive(Debug)]
pub struct Fixture {
    dir: PathBuf,
    clock: AtomicI64,
}

impl Fixture {
    /// `git init -b main` in `dir` (created if needed), with an identity,
    /// no signing, no hooks and git's default diff settings.
    pub fn init(dir: &Path) -> Self {
        std::fs::create_dir_all(dir).expect("create the fixture folder");
        let dir = dir.canonicalize().expect("canonical fixture folder");
        let f = Self {
            dir,
            clock: AtomicI64::new(1_700_000_000),
        };
        f.git(&["init", "-q", "-b", "main"]);
        let hooks = f.dir.join(".git").join("no-hooks");
        std::fs::create_dir_all(&hooks).expect("hooks folder");
        let hooks = hooks.to_str().expect("UTF-8 path").to_owned();
        for (k, v) in [
            ("user.name", "Fixture Author"),
            ("user.email", "author@example.com"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.autocrlf", "false"),
            ("core.hooksPath", hooks.as_str()),
            ("diff.algorithm", "myers"),
            ("diff.indentHeuristic", "true"),
            ("diff.context", "3"),
            ("diff.interHunkContext", "0"),
            ("diff.noprefix", "false"),
            ("diff.mnemonicPrefix", "false"),
            ("diff.renames", "false"),
            ("status.renames", "false"),
            ("color.ui", "never"),
            ("advice.detachedHead", "false"),
        ] {
            f.git(&["config", "--local", k, v]);
        }
        f
    }

    /// The work tree.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Run `git args…` (dates advance a second per call); its stdout.
    /// Panics if it fails.
    pub fn git(&self, args: &[&str]) -> String {
        self.try_git(args, None)
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"))
    }

    /// Run `git args…` with `stdin`; stdout, or stderr as the error.
    pub fn try_git(&self, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
        let t = self.clock.fetch_add(1, Ordering::Relaxed);
        let date = format!("@{t} +0000");
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(&self.dir)
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_EDITOR", ":")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            use std::io::Write as _;
            pipe.write_all(input.as_bytes())
                .map_err(|e| e.to_string())?;
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    /// Write `contents` to `rel` (creating folders).
    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create folders");
        }
        std::fs::write(path, contents).expect("write fixture file");
    }

    /// Read `rel`.
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.join(rel)).expect("read fixture file")
    }

    /// Stage everything and commit with `message`; the new commit's id.
    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }
}
