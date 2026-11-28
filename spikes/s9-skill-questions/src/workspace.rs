// SPDX-License-Identifier: Apache-2.0

//! The files a reference can point at: a directory on disk (the samples), or
//! a file list with some line counts (a GitHub repository at a PR's head).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Debug, Default)]
pub struct Workspace {
    /// `/`-separated paths relative to the root, sorted.
    pub paths: Vec<String>,
    /// File contents, when known.
    pub text: BTreeMap<String, Vec<String>>,
    /// Line counts, when known without the text.
    pub counts: HashMap<String, usize>,
    /// The absolute root, to strip from absolute paths.
    pub root: Option<String>,
}

/// What a raw path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    File(String),
    /// Several files end with it, and the hints did not pick one.
    Ambiguous(Vec<String>),
    Unknown,
}

impl Workspace {
    pub fn from_dir(dir: &Path) -> Self {
        let mut ws = Workspace {
            root: Some(dir.display().to_string()),
            ..Workspace::default()
        };
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p
                        .strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    let text = std::fs::read_to_string(&p).unwrap_or_default();
                    ws.text
                        .insert(rel.clone(), text.lines().map(str::to_owned).collect());
                    ws.paths.push(rel);
                }
            }
        }
        ws.paths.sort();
        ws
    }

    pub fn from_list(paths: Vec<String>, counts: HashMap<String, usize>) -> Self {
        let mut paths = paths;
        paths.sort();
        Workspace {
            paths,
            counts,
            ..Workspace::default()
        }
    }

    /// Lines in `path`, if known.
    pub fn line_count(&self, path: &str) -> Option<usize> {
        self.text
            .get(path)
            .map(Vec::len)
            .or_else(|| self.counts.get(path).copied())
    }

    pub fn lines(&self, path: &str) -> Option<&[String]> {
        self.text.get(path).map(Vec::as_slice)
    }

    /// Resolves a path as written in a message: exact, else the files it is
    /// a suffix of (`cart.ts` -> `src/cart/cart.ts`), narrowed by `hints`
    /// (files the turn touched) when several match.
    pub fn resolve(&self, raw: &str, hints: &[String]) -> Resolved {
        let mut p =
            raw.trim_matches(|c| matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '*'));
        if let Some(root) = &self.root {
            if let Some(rest) = p.strip_prefix(root.as_str()) {
                p = rest;
            }
        }
        for prefix in ["./", "a/", "b/"] {
            if let Some(rest) = p.strip_prefix(prefix) {
                if !self.paths.iter().any(|f| f == p) {
                    p = rest;
                }
            }
        }
        let p = p.trim_start_matches('/');
        if p.is_empty() {
            return Resolved::Unknown;
        }
        if self.paths.iter().any(|f| f == p) {
            return Resolved::File(p.to_owned());
        }
        let suffix = format!("/{p}");
        let matches: Vec<String> = self
            .paths
            .iter()
            .filter(|f| f.ends_with(&suffix))
            .cloned()
            .collect();
        // An absolute path from another machine: the longest workspace path
        // it ends with.
        let matches = if matches.is_empty() && raw.starts_with('/') {
            let mut best: Vec<String> = self
                .paths
                .iter()
                .filter(|f| p.ends_with(&format!("/{f}")))
                .cloned()
                .collect();
            best.sort_by_key(|f| std::cmp::Reverse(f.len()));
            best.truncate(1);
            best
        } else {
            matches
        };
        match matches.len() {
            0 => Resolved::Unknown,
            1 => Resolved::File(matches.into_iter().next().unwrap()),
            _ => {
                let hinted: Vec<String> = matches
                    .iter()
                    .filter(|m| hints.contains(m))
                    .cloned()
                    .collect();
                if hinted.len() == 1 {
                    Resolved::File(hinted.into_iter().next().unwrap())
                } else {
                    Resolved::Ambiguous(matches)
                }
            }
        }
    }
}
