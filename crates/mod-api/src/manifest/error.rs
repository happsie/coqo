// SPDX-License-Identifier: Apache-2.0

//! Manifest errors with field paths and source positions.

use std::fmt;
use std::ops::Range;

use toml::Spanned;
use toml::de::{DeTable, DeValue};

/// One segment of a field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seg {
    Key(String),
    Index(usize),
}

/// A field path such as `contributes.commands[1].id`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FieldPath(Vec<Seg>);

impl FieldPath {
    pub(crate) fn root() -> Self {
        Self::default()
    }

    pub(crate) fn key(&self, key: &str) -> Self {
        let mut segs = self.0.clone();
        segs.push(Seg::Key(key.to_owned()));
        Self(segs)
    }

    pub(crate) fn index(&self, index: usize) -> Self {
        let mut segs = self.0.clone();
        segs.push(Seg::Index(index));
        Self(segs)
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.0.iter().enumerate() {
            match seg {
                Seg::Key(k) if i == 0 => f.write_str(k)?,
                Seg::Key(k) => write!(f, ".{k}")?,
                Seg::Index(n) => write!(f, "[{n}]")?,
            }
        }
        Ok(())
    }
}

/// A line and column in the manifest source, both starting at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    /// Line number.
    pub line: usize,
    /// Column, counted in characters.
    pub column: usize,
}

impl Position {
    fn of(source: &str, offset: usize) -> Self {
        let before = &source[..offset.min(source.len())];
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        Self {
            line: before.matches('\n').count() + 1,
            column: before[line_start..].chars().count() + 1,
        }
    }
}

/// One thing wrong with a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestProblem {
    /// Field path, such as `contributes.commands[1].id`. `None` for syntax
    /// errors and for problems the TOML parser reports, whose message names
    /// the field.
    pub path: Option<String>,
    /// What is wrong and, where it helps, what to write instead.
    pub message: String,
    /// Byte range in the source, when the manifest was parsed from text.
    pub span: Option<Range<usize>>,
    /// Where `span` starts.
    pub position: Option<Position>,
}

impl fmt::Display for ManifestProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(pos) = self.position {
            write!(f, "{}:{}: ", pos.line, pos.column)?;
        }
        if let Some(path) = &self.path {
            write!(f, "{path}: ")?;
        }
        f.write_str(&self.message)
    }
}

/// Why a manifest was rejected: every problem found, in source order.
///
/// Its `Display` prints one problem per line as
/// `line:column: field.path: message`; prefix it with the file name when
/// reporting it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ManifestError {
    problems: Vec<ManifestProblem>,
}

impl ManifestError {
    pub(crate) fn new(mut problems: Vec<ManifestProblem>) -> Self {
        problems.sort_by_key(|p| p.span.as_ref().map_or(usize::MAX, |s| s.start));
        Self { problems }
    }

    /// The problems, at least one.
    pub fn problems(&self) -> &[ManifestProblem] {
        &self.problems
    }

    /// A TOML syntax or type error.
    pub(crate) fn from_toml(source: &str, err: &toml::de::Error) -> Self {
        let span = err.span();
        Self::new(vec![ManifestProblem {
            path: None,
            message: err.message().trim_end().to_owned(),
            position: span.as_ref().map(|s| Position::of(source, s.start)),
            span,
        }])
    }

    /// Problems found by validation, located in `source` when given.
    pub(crate) fn from_problems(source: Option<&str>, problems: Vec<(FieldPath, String)>) -> Self {
        let root = source.and_then(|s| DeTable::parse(s).ok());
        let problems = problems
            .into_iter()
            .map(|(path, message)| {
                let span = root.as_ref().and_then(|r| locate(r, &path));
                ManifestProblem {
                    path: Some(path.to_string()),
                    message,
                    position: span
                        .as_ref()
                        .zip(source)
                        .map(|(s, src)| Position::of(src, s.start)),
                    span,
                }
            })
            .collect();
        Self::new(problems)
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, problem) in self.problems.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{problem}")?;
        }
        Ok(())
    }
}

/// The span of the deepest part of `path` present in the document.
fn locate(root: &Spanned<DeTable<'_>>, path: &FieldPath) -> Option<Range<usize>> {
    let mut segs = path.0.iter();
    let Some(Seg::Key(first)) = segs.next() else {
        return None;
    };
    let mut current: &Spanned<DeValue<'_>> = root.get_ref().get(first.as_str())?;
    for seg in segs {
        let next = match seg {
            Seg::Key(k) => current.get_ref().get(k.as_str()),
            Seg::Index(n) => current.get_ref().get(*n),
        };
        match next {
            Some(value) => current = value,
            None => break,
        }
    }
    Some(current.span())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_display_like_toml_keys() {
        let p = FieldPath::root()
            .key("contributes")
            .key("commands")
            .index(1)
            .key("id");
        assert_eq!(p.to_string(), "contributes.commands[1].id");
    }

    #[test]
    fn positions_are_one_based() {
        let src = "a = 1\nbé = 2\n";
        assert_eq!(Position::of(src, 0), Position { line: 1, column: 1 });
        let offset = src.find('2').unwrap();
        assert_eq!(Position::of(src, offset), Position { line: 2, column: 6 });
    }

    #[test]
    fn locates_nested_values_and_falls_back_to_parents() {
        let src = "[[contributes.commands]]\nid = \"a\"\ntitle = \"\"\n";
        let root = DeTable::parse(src).unwrap();
        let path = FieldPath::root()
            .key("contributes")
            .key("commands")
            .index(0);
        let span = locate(&root, &path.key("title")).unwrap();
        assert_eq!(&src[span], "\"\"");
        // A missing key points at its parent: for an array-of-tables entry,
        // that is its header.
        let span = locate(&root, &path.key("missing")).unwrap();
        assert_eq!(&src[span], "[[contributes.commands]]");
    }
}
