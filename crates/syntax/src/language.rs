// SPDX-License-Identifier: Apache-2.0

//! The bundled languages and how files map to them.

use std::fmt;
use std::sync::OnceLock;

use tree_sitter::Query;

/// A bundled language: its file associations, its tree-sitter grammar and the
/// queries Coqo runs on it.
///
/// Every language is a `&'static Language` from [`languages`]; compare them
/// by [`id`](Language::id).
pub struct Language {
    id: &'static str,
    name: &'static str,
    extensions: &'static [&'static str],
    filenames: &'static [&'static str],
    interpreters: &'static [&'static str],
    line_comment: Option<&'static str>,
    grammar: fn() -> tree_sitter::Language,
    highlights: &'static [&'static str],
    symbols_source: &'static str,
    folds_source: &'static str,
    queries: OnceLock<Queries>,
}

/// A language's compiled queries.
pub(crate) struct Queries {
    pub(crate) symbols: Query,
    pub(crate) folds: Query,
}

impl Language {
    /// The language id, such as `rust` or `tsx`. It is also the name
    /// gpui-component's highlighter knows the language by.
    pub fn id(&self) -> &'static str {
        self.id
    }

    /// The display name, such as `TypeScript`.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// File extensions, without the dot.
    pub fn extensions(&self) -> &'static [&'static str] {
        self.extensions
    }

    /// Exact file names that are in this language whatever their extension.
    pub fn filenames(&self) -> &'static [&'static str] {
        self.filenames
    }

    /// Interpreters a `#!` line names for scripts in this language.
    pub fn interpreters(&self) -> &'static [&'static str] {
        self.interpreters
    }

    /// The line comment token, such as `//`.
    pub fn line_comment(&self) -> Option<&'static str> {
        self.line_comment
    }

    /// The tree-sitter grammar.
    pub fn grammar(&self) -> tree_sitter::Language {
        (self.grammar)()
    }

    /// The grammar's own highlight query (tree-sitter capture names such as
    /// `@keyword`), for highlighters that don't bundle one. TypeScript and
    /// TSX build on JavaScript's query, as tree-sitter itself does.
    pub fn highlights_query(&self) -> String {
        self.highlights.join("\n")
    }

    /// The compiled symbol and fold queries. They are checked by the tests,
    /// so failing to compile is a bug.
    pub(crate) fn queries(&self) -> &Queries {
        self.queries.get_or_init(|| {
            let grammar = self.grammar();
            let compile = |what: &str, source: &str| {
                Query::new(&grammar, source).unwrap_or_else(|e| {
                    panic!("the bundled {what} query for {} is invalid: {e}", self.id)
                })
            };
            Queries {
                symbols: compile("symbols", self.symbols_source),
                folds: compile("folds", self.folds_source),
            }
        })
    }

    /// The language with this id.
    pub fn by_id(id: &str) -> Option<&'static Language> {
        languages().iter().find(|l| l.id == id)
    }

    /// The language of a file, by its name (`Cargo.lock`) or extension.
    /// `path` may be a full path with `/` or `\` separators.
    pub fn for_path(path: &str) -> Option<&'static Language> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if let Some(lang) = languages().iter().find(|l| l.filenames.contains(&name)) {
            return Some(lang);
        }
        let lower = name.to_ascii_lowercase();
        let (stem, ext) = lower.rsplit_once('.')?;
        if stem.is_empty() {
            // A dotfile such as `.json` has no extension.
            return None;
        }
        languages().iter().find(|l| l.extensions.contains(&ext))
    }

    /// The language a `#!` line asks for, such as `#!/usr/bin/env node`.
    pub fn for_shebang(first_line: &str) -> Option<&'static Language> {
        let line = first_line.strip_prefix("#!")?.trim();
        let mut words = line.split_whitespace();
        let mut program = basename(words.next()?);
        if program == "env" {
            // `env [-S] [VAR=value…] program`
            program = words
                .find(|w| !w.starts_with('-') && !w.contains('='))
                .map(basename)?;
        }
        // `node18`, `python3.12`: drop a trailing version.
        let program = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        languages()
            .iter()
            .find(|l| l.interpreters.contains(&program))
    }

    /// The language of a file: by name or extension, else by its `#!` line.
    pub fn detect(path: &str, text: &str) -> Option<&'static Language> {
        Self::for_path(path).or_else(|| {
            let first = text.lines().next().unwrap_or("");
            Self::for_shebang(first)
        })
    }
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

impl fmt::Debug for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Language").field(&self.id).finish()
    }
}

impl PartialEq for Language {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Language {}

macro_rules! queries {
    ($dir:literal) => {
        queries!($dir, $dir)
    };
    ($symbols:literal, $folds:literal) => {
        (
            include_str!(concat!("../queries/", $symbols, "/symbols.scm")),
            include_str!(concat!("../queries/", $folds, "/folds.scm")),
        )
    };
}

#[allow(clippy::too_many_arguments)]
const fn language(
    id: &'static str,
    name: &'static str,
    extensions: &'static [&'static str],
    filenames: &'static [&'static str],
    interpreters: &'static [&'static str],
    line_comment: Option<&'static str>,
    grammar: fn() -> tree_sitter::Language,
    highlights: &'static [&'static str],
    (symbols_source, folds_source): (&'static str, &'static str),
) -> Language {
    Language {
        id,
        name,
        extensions,
        filenames,
        interpreters,
        line_comment,
        grammar,
        highlights,
        symbols_source,
        folds_source,
        queries: OnceLock::new(),
    }
}

static LANGUAGES: [Language; 13] = [
    language(
        "rust",
        "Rust",
        &["rs"],
        &[],
        &["rust-script", "cargo"],
        Some("//"),
        || tree_sitter_rust::LANGUAGE.into(),
        &[tree_sitter_rust::HIGHLIGHTS_QUERY],
        queries!("rust"),
    ),
    language(
        "typescript",
        "TypeScript",
        &["ts", "mts", "cts"],
        &[],
        &["deno", "ts-node", "tsx", "bun"],
        Some("//"),
        || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        &[
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        ],
        queries!("typescript"),
    ),
    language(
        "tsx",
        "TSX",
        &["tsx"],
        &[],
        &[],
        Some("//"),
        || tree_sitter_typescript::LANGUAGE_TSX.into(),
        &[
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
        ],
        // TSX is TypeScript plus JSX: TypeScript's symbols, and folds that
        // add JSX elements.
        queries!("typescript", "tsx"),
    ),
    language(
        "javascript",
        "JavaScript",
        &["js", "mjs", "cjs", "jsx"],
        &[],
        &["node", "nodejs"],
        Some("//"),
        || tree_sitter_javascript::LANGUAGE.into(),
        &[
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
        ],
        queries!("javascript"),
    ),
    language(
        "java",
        "Java",
        &["java"],
        &[],
        &["java"],
        Some("//"),
        || tree_sitter_java::LANGUAGE.into(),
        &[tree_sitter_java::HIGHLIGHTS_QUERY],
        queries!("java"),
    ),
    language(
        "kotlin",
        "Kotlin",
        &["kt", "kts"],
        &[],
        &["kotlin", "kscript"],
        Some("//"),
        || tree_sitter_kotlin_sg::LANGUAGE.into(),
        &[tree_sitter_kotlin_sg::HIGHLIGHTS_QUERY],
        queries!("kotlin"),
    ),
    language(
        "json",
        "JSON",
        &["json", "jsonc"],
        &[".babelrc", ".eslintrc", ".prettierrc", "flake.lock"],
        &[],
        None,
        || tree_sitter_json::LANGUAGE.into(),
        &[tree_sitter_json::HIGHLIGHTS_QUERY],
        queries!("json"),
    ),
    language(
        "toml",
        "TOML",
        &["toml"],
        &["Cargo.lock", "Pipfile", "uv.lock", "poetry.lock"],
        &[],
        Some("#"),
        || tree_sitter_toml_ng::LANGUAGE.into(),
        &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
        queries!("toml"),
    ),
    language(
        "markdown",
        "Markdown",
        &["md", "markdown", "mdx"],
        &[],
        &[],
        None,
        || tree_sitter_md::LANGUAGE.into(),
        &[tree_sitter_md::HIGHLIGHT_QUERY_BLOCK],
        queries!("markdown"),
    ),
    language(
        "yaml",
        "YAML",
        &["yml", "yaml"],
        &[".clang-format", ".clang-tidy"],
        &[],
        Some("#"),
        || tree_sitter_yaml::LANGUAGE.into(),
        &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
        queries!("yaml"),
    ),
    language(
        "html",
        "HTML",
        &["html", "htm", "xhtml"],
        &[],
        &[],
        None,
        || tree_sitter_html::LANGUAGE.into(),
        &[tree_sitter_html::HIGHLIGHTS_QUERY],
        queries!("html"),
    ),
    language(
        "css",
        "CSS",
        &["css"],
        &[],
        &[],
        None,
        || tree_sitter_css::LANGUAGE.into(),
        &[tree_sitter_css::HIGHLIGHTS_QUERY],
        queries!("css"),
    ),
    language(
        "xml",
        "XML",
        &[
            "xml", "xsd", "xsl", "xslt", "svg", "plist", "xaml", "csproj", "fxml",
        ],
        &[],
        &[],
        None,
        || tree_sitter_xml::LANGUAGE_XML.into(),
        &[tree_sitter_xml::XML_HIGHLIGHT_QUERY],
        queries!("xml"),
    ),
];

/// Every bundled language.
pub fn languages() -> &'static [Language] {
    &LANGUAGES
}
