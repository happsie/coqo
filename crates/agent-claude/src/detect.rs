// SPDX-License-Identifier: Apache-2.0

//! What is installed: the user's `claude`, Node.js and npm on `PATH`, and
//! the adapter in Coqo's data directory. Cheap enough to run for every
//! `agents/providers` (file checks; `node --version` is cached per binary).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::{ADAPTER_BIN, ADAPTER_PACKAGE, ADAPTER_VERSION, MIN_NODE_MAJOR};

/// The user's Claude Code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claude {
    /// Where `PATH` finds `claude` (for the terminal the user signs in in).
    pub on_path: PathBuf,
    /// The binary itself, symlinks resolved: `CLAUDE_CODE_EXECUTABLE`.
    pub real: PathBuf,
}

/// Node.js on `PATH`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Where `PATH` finds `node`.
    pub path: PathBuf,
    /// `node --version`, if it ran.
    pub version: Option<(u32, u32, u32)>,
}

impl Node {
    /// Whether it can run the adapter.
    pub fn new_enough(&self) -> bool {
        self.version
            .is_some_and(|(major, _, _)| major >= MIN_NODE_MAJOR)
    }
}

/// Everything the provider needs, found at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detection {
    /// `claude`, if found.
    pub claude: Option<Claude>,
    /// `node`, if found.
    pub node: Option<Node>,
    /// `npm`, if found.
    pub npm: Option<PathBuf>,
    /// The adapter's install directory (`<data dir>/claude-acp`).
    pub adapter_dir: PathBuf,
    /// The installed adapter's version, if any.
    pub adapter_version: Option<String>,
}

impl Detection {
    /// Whether the pinned adapter version is installed.
    pub fn adapter_ok(&self) -> bool {
        self.adapter_version.as_deref() == Some(ADAPTER_VERSION)
            && adapter_bin(&self.adapter_dir).is_file()
    }

    /// Whether a session can start.
    pub fn ready(&self) -> bool {
        self.claude.is_some()
            && self.node.as_ref().is_some_and(Node::new_enough)
            && self.adapter_ok()
    }
}

/// `<dir>/node_modules/.bin/claude-agent-acp`.
pub fn adapter_bin(dir: &Path) -> PathBuf {
    dir.join("node_modules").join(".bin").join(ADAPTER_BIN)
}

/// The installed adapter's `package.json` version.
pub fn installed_version(dir: &Path) -> Option<String> {
    let mut manifest = dir.join("node_modules");
    manifest.extend(ADAPTER_PACKAGE.split('/'));
    manifest.push("package.json");
    let text = std::fs::read_to_string(manifest).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("version")?.as_str().map(str::to_owned)
}

/// The first executable `name` in the absolute directories of `path`.
pub fn find_on_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `v24.18.0` → `(24, 18, 0)`.
pub fn parse_node_version(output: &str) -> Option<(u32, u32, u32)> {
    let v = output.trim().strip_prefix('v')?;
    let mut parts = v.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

type NodeKey = (PathBuf, Option<SystemTime>);
type Version = (u32, u32, u32);

/// Finds things, caching `node --version` per binary.
#[derive(Debug)]
pub struct Detector {
    path: Option<OsString>,
    adapter_dir: PathBuf,
    node_versions: Mutex<Option<(NodeKey, Option<Version>)>>,
}

impl Detector {
    /// Searches `path` (the daemon's `PATH` when `None`), with the adapter
    /// in `adapter_dir`.
    pub fn new(path: Option<OsString>, adapter_dir: PathBuf) -> Self {
        Self {
            path,
            adapter_dir,
            node_versions: Mutex::new(None),
        }
    }

    /// The adapter's install directory.
    pub fn adapter_dir(&self) -> &Path {
        &self.adapter_dir
    }

    /// The `PATH` searched.
    pub fn path(&self) -> OsString {
        self.path
            .clone()
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default()
    }

    /// What is installed now.
    pub fn detect(&self) -> Detection {
        let path = self.path();
        let claude = find_on_path("claude", &path).and_then(|on_path| {
            let real = std::fs::canonicalize(&on_path).ok()?;
            Some(Claude { on_path, real })
        });
        let node = find_on_path("node", &path).map(|path| {
            let version = self.node_version(&path);
            Node { path, version }
        });
        Detection {
            claude,
            node,
            npm: find_on_path("npm", &path),
            adapter_dir: self.adapter_dir.clone(),
            adapter_version: installed_version(&self.adapter_dir),
        }
    }

    fn node_version(&self, node: &Path) -> Option<(u32, u32, u32)> {
        let real = std::fs::canonicalize(node).unwrap_or_else(|_| node.to_path_buf());
        let modified = std::fs::metadata(&real).and_then(|m| m.modified()).ok();
        let key = (real, modified);
        let mut cache = self
            .node_versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached, version)) = cache.as_ref()
            && *cached == key
        {
            return *version;
        }
        let version = Command::new(node)
            .arg("--version")
            .env("PATH", self.path())
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| parse_node_version(&String::from_utf8_lossy(&o.stdout)));
        *cache = Some((key, version));
        version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_versions_parse() {
        assert_eq!(parse_node_version("v24.18.0\n"), Some((24, 18, 0)));
        assert_eq!(parse_node_version("v22.0.1-nightly"), Some((22, 0, 1)));
        assert_eq!(parse_node_version("24.1.0"), None);
        assert_eq!(parse_node_version("vx"), None);
    }

    #[cfg(unix)]
    #[test]
    fn finds_executables_on_path_and_resolves_claude() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (a, b, versions) = (root.join("a"), root.join("b"), root.join("versions"));
        for d in [&a, &b, &versions] {
            std::fs::create_dir(d).unwrap();
        }
        let script = |p: &Path, body: &str, mode: u32| {
            std::fs::write(p, body).unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        // Not executable: skipped.
        script(&a.join("claude"), "", 0o644);
        let real = versions.join("2.1.0");
        script(&real, "#!/bin/sh\n", 0o755);
        std::os::unix::fs::symlink(&real, b.join("claude")).unwrap();
        script(&b.join("node"), "#!/bin/sh\necho v20.11.1\n", 0o755);

        let path = std::env::join_paths(["relative".into(), a.clone(), b.clone()]).unwrap();
        let adapter = root.join("claude-acp");
        let found = Detector::new(Some(path), adapter.clone()).detect();
        assert_eq!(
            found.claude,
            Some(Claude {
                on_path: b.join("claude"),
                real
            })
        );
        let node = found.node.clone().unwrap();
        assert_eq!(node.version, Some((20, 11, 1)));
        assert!(!node.new_enough());
        assert_eq!(found.npm, None);
        assert_eq!(found.adapter_version, None);
        assert!(!found.ready());

        // The adapter counts only at the pinned version, with its bin.
        let package = adapter.join("node_modules/@agentclientprotocol/claude-agent-acp");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), r#"{"version":"0.1.0"}"#).unwrap();
        assert_eq!(installed_version(&adapter).as_deref(), Some("0.1.0"));
        let found = Detector::new(Some(OsString::new()), adapter.clone()).detect();
        assert!(!found.adapter_ok());
        std::fs::write(
            package.join("package.json"),
            format!(r#"{{"version":"{ADAPTER_VERSION}"}}"#),
        )
        .unwrap();
        let found = Detector::new(Some(OsString::new()), adapter.clone()).detect();
        assert!(!found.adapter_ok(), "no bin yet");
        std::fs::create_dir_all(adapter.join("node_modules/.bin")).unwrap();
        std::fs::write(adapter_bin(&adapter), "").unwrap();
        let found = Detector::new(Some(OsString::new()), adapter).detect();
        assert!(found.adapter_ok());
        assert!(found.claude.is_none() && found.node.is_none());
    }
}
