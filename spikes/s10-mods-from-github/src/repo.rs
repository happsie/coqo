//! A read-only view of a repository at one commit: its file list and the
//! bytes of a file. Two implementations: the public GitHub API (no auth) and
//! a local folder (for the offline tests).
//!
//! Nothing here executes anything it downloads. Files are only read as text.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The User-Agent sent on every request. Generic on purpose: no user name,
/// host name or version that could identify the person.
pub const USER_AGENT: &str = "coqo-mod-installer";

/// Largest file the prototype reads (manifests, indexes, frontmatter).
const MAX_READ: u64 = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Executable,
    Symlink,
    /// A git submodule: a pinned commit of another repository.
    Submodule,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
}

pub trait Repo {
    /// Human-readable location, for the plan.
    fn location(&self) -> String;
    /// Every file at the pinned commit (folders are implied by paths).
    fn files(&self) -> &[Entry];
    /// Whether the file list is incomplete (GitHub truncates huge trees).
    fn truncated(&self) -> bool {
        false
    }
    fn read(&self, path: &str) -> Result<String, String>;
    /// Network requests made so far (0 for a local folder).
    fn requests(&self) -> (u32, u32) {
        (0, 0)
    }

    fn exists(&self, path: &str) -> bool {
        self.files().iter().any(|e| e.path == path)
    }
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

pub struct GitHub {
    pub owner: String,
    pub repo: String,
    /// The requested ref, or `None` for the default branch.
    pub reference: Option<String>,
    /// The commit everything below is read at.
    pub sha: String,
    files: Vec<Entry>,
    truncated: bool,
    /// Calls to api.github.com (counted against 60/hour per IP).
    api_calls: Cell<u32>,
    /// Fetches from raw.githubusercontent.com.
    raw_calls: Cell<u32>,
}

#[derive(Deserialize)]
struct Tree {
    tree: Vec<TreeItem>,
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeItem {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
}

fn get(url: &str, accept: &str) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) => format!("not found: {url}"),
            ureq::Error::StatusCode(403 | 429) => {
                format!("rate limited by GitHub (60 unauthenticated API calls an hour): {url}")
            }
            other => format!("{url}: {other}"),
        })?;
    response
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("{url}: {e}"))
}

impl GitHub {
    /// Resolves `reference` (or the default branch) to a commit SHA and
    /// lists the tree at that commit: two API calls.
    pub fn open(owner: &str, repo: &str, reference: Option<&str>) -> Result<Self, String> {
        let wanted = reference.unwrap_or("HEAD");
        let sha = get(
            &format!("https://api.github.com/repos/{owner}/{repo}/commits/{wanted}"),
            "application/vnd.github.sha",
        )?
        .trim()
        .to_owned();
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("GitHub did not return a commit SHA for {wanted}"));
        }
        let tree: Tree = serde_json::from_str(&get(
            &format!("https://api.github.com/repos/{owner}/{repo}/git/trees/{sha}?recursive=1"),
            "application/vnd.github+json",
        )?)
        .map_err(|e| format!("tree: {e}"))?;
        let files = tree
            .tree
            .into_iter()
            .filter_map(|t| {
                let kind = match (t.kind.as_str(), t.mode.as_str()) {
                    ("blob", "100755") => Kind::Executable,
                    ("blob", "120000") => Kind::Symlink,
                    ("blob", _) => Kind::File,
                    ("commit", _) => Kind::Submodule,
                    _ => return None, // folders
                };
                Some(Entry {
                    path: t.path,
                    kind,
                    size: t.size,
                })
            })
            .collect();
        Ok(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            reference: reference.map(str::to_owned),
            sha,
            files,
            truncated: tree.truncated,
            api_calls: Cell::new(2),
            raw_calls: Cell::new(0),
        })
    }

    /// The latest non-prerelease release, if any: one API call.
    pub fn latest_release(&self) -> Result<Option<Release>, String> {
        self.api_calls.set(self.api_calls.get() + 1);
        match get(
            &format!(
                "https://api.github.com/repos/{}/{}/releases/latest",
                self.owner, self.repo
            ),
            "application/vnd.github+json",
        ) {
            Ok(body) => serde_json::from_str(&body)
                .map(Some)
                .map_err(|e| format!("release: {e}")),
            Err(e) if e.starts_with("not found") => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Repo for GitHub {
    fn location(&self) -> String {
        format!(
            "github.com/{}/{} @ {} ({})",
            self.owner,
            self.repo,
            &self.sha[..12],
            self.reference.as_deref().unwrap_or("default branch")
        )
    }

    fn files(&self) -> &[Entry] {
        &self.files
    }

    fn truncated(&self) -> bool {
        self.truncated
    }

    fn read(&self, path: &str) -> Result<String, String> {
        let entry = self
            .files
            .iter()
            .find(|e| e.path == path)
            .ok_or_else(|| format!("no file {path}"))?;
        if entry.kind != Kind::File && entry.kind != Kind::Executable {
            return Err(format!("{path} is not a regular file"));
        }
        if entry.size > MAX_READ {
            return Err(format!(
                "{path} is too large to read ({} bytes)",
                entry.size
            ));
        }
        self.raw_calls.set(self.raw_calls.get() + 1);
        // Pinned by SHA, so what is read is what the plan was made from.
        get(
            &format!(
                "https://raw.githubusercontent.com/{}/{}/{}/{}",
                self.owner, self.repo, self.sha, path
            ),
            "text/plain",
        )
    }

    fn requests(&self) -> (u32, u32) {
        (self.api_calls.get(), self.raw_calls.get())
    }
}

// ---------------------------------------------------------------------------
// Local folder
// ---------------------------------------------------------------------------

pub struct Local {
    root: PathBuf,
    files: Vec<Entry>,
}

impl Local {
    pub fn open(root: &Path) -> Result<Self, String> {
        let mut files = Vec::new();
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if rel == ".git" || rel.starts_with(".git/") {
                    continue;
                }
                let kind = if meta.file_type().is_symlink() {
                    Kind::Symlink
                } else if meta.is_dir() {
                    dirs.push(path);
                    continue;
                } else if is_executable(&meta) {
                    Kind::Executable
                } else {
                    Kind::File
                };
                files.push(Entry {
                    path: rel,
                    kind,
                    size: meta.len(),
                });
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            root: root.to_path_buf(),
            files,
        })
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

impl Repo for Local {
    fn location(&self) -> String {
        format!("local folder {}", self.root.display())
    }

    fn files(&self) -> &[Entry] {
        &self.files
    }

    fn read(&self, path: &str) -> Result<String, String> {
        let entry = self
            .files
            .iter()
            .find(|e| e.path == path)
            .ok_or_else(|| format!("no file {path}"))?;
        if entry.kind == Kind::Symlink {
            return Err(format!("{path} is a symlink"));
        }
        if entry.size > MAX_READ {
            return Err(format!("{path} is too large to read"));
        }
        std::fs::read_to_string(self.root.join(path)).map_err(|e| format!("{path}: {e}"))
    }
}
