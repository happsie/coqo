# 11. Syntax layer: `coqo-syntax`, shared by the daemon and the UI

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #42. Refines PLAN §7.1.1 (the always-on *Syntax* layer).

## Context

PLAN §7.1.1 puts the always-on syntax layer (highlighting, folding, outline, symbol search) "in the UI, no server process". Two things since then need it outside the UI:

- **Agents** ask about code by name ("explain `Circle::area`") through the editor MCP tools, which `coqod mcp` serves from the daemon, often with no UI attached and with intellisense off (the ⚡ bolt is off by default).
- **Anchors** (#72) re-attach questions, walkthrough steps and trace points to code after edits. Their fuzzy step uses tree-sitter node identity, and the anchor engine runs in the daemon, next to the buffers.

At the same time, PR #183 had set up highlighting directly through gpui-component's highlighter (its own tree-sitter integration, grammars and highlight queries), with its own extension table in `coqo_ui::editor`.

## Decision

**One crate, `coqo-syntax`, with no GPUI dependency,** owns the syntax model:

- **Language registry:** the bundled languages (Rust, TypeScript, TSX, JavaScript/JSX, Java, Kotlin, JSON, TOML, Markdown, YAML, HTML, CSS, XML), detected by file name, extension or `#!` line. The UI's `language_for_path` delegates to it, so the editor and the daemon always agree on a file's language.
- **Documents:** a text plus its tree-sitter tree. `Document::edit` takes a buffer change (sorted, non-overlapping edits in pre-change coordinates, as in `buffer/*`) and re-parses incrementally.
- **Symbols:** per-language tags queries (`queries/<lang>/symbols.scm`, tree-sitter's `@definition.<kind>` / `@name` conventions). Each symbol has a name, a kind (function, method, class, module, heading, key, …), its byte range, its name's range, lines and a **container path** (the names of the enclosing symbols). A function directly inside a class, struct, trait, interface, enum or impl becomes a method. When two patterns match one definition, the earlier one wins. The outline is the same symbols nested by containment. `Document::symbols_at(offset)` gives the enclosing symbols, which is what an anchor attaches to.
- **Folding:** per-language folds queries (`queries/<lang>/folds.scm`) with `@fold`, `@fold.comment` and `@fold.imports`; consecutive comments and imports fold as one run.

The queries are Coqo's own, written against each grammar's node types, so a symbol's kind and name mean the same across languages. Golden tests per language (`crates/syntax/tests/golden`) pin them.

**The daemon serves it.** `coqod`'s `syntax` service keeps a parsed document per buffer it is asked about and brings it up to date from the buffer's change log (an incremental re-parse) or, when the log no longer has the changes, from the text. `syntax/symbols`, `syntax/outline` and `syntax/folding` take a buffer id and answer for its current revision, unsaved edits included. They are session-callable for buffers of the token's own workspace (ADR 0007's allowlist), since agents are their main users.

**The UI keeps gpui-component's highlighter for drawing.** Replacing it would mean re-implementing highlight runs, injections and the background parse for large files behind the editor's `InputHighlighter` seam: a big rewrite for no visible gain. Instead gpui-component highlights with the same grammar crates (same versions, so each grammar's C code is linked once), and `coqo_ui::editor` registers the languages it lacks (XML) or gets wrong (TSX, which gpui-component 0.11 highlights with TypeScript's query alone) from `coqo-syntax`'s grammar and highlight query. A `coqo-ui` test highlights every fixture and checks token colours against the zen palette. The UI's folding is still gpui-component's (every multi-line named node); feeding `coqo-syntax`'s folds (#195) and an outline view with breadcrumbs (#194) into the editor are follow-ups, as is word completion with the bolt off (#196).

**`coqo.syntax` is a built-in mod.** It is always on (`activation = ["startup"]`) and contributes one `languages` contribution per bundled language (id, name, extensions, file names, line comment). It lives in `coqo-syntax` (`builtin`) rather than `mods/`, because a mod under `mods/` may depend only on `coqo-mod-api`, and the languages come from the crate's registry.

## Consequences

- PLAN §7.1.1's syntax layer is provided by `coqo-syntax` in both processes. The daemon parses only buffers it is asked about, and drops a parse when its buffer is unloaded.
- Performance: a 10,000-line Rust file parses in about 14 ms and yields its symbols and folds in about 18 ms in a release build, and an incremental one-character edit re-parses in about 1 ms (`crates/syntax/tests/document.rs` asserts the parse under 100 ms in release).
- Adding a language means a grammar crate (MIT or similarly permissive, at the version gpui-component uses if it bundles one), an entry in `language.rs`, symbols and folds queries, and a fixture with its golden file. Adding a symbol kind is a protocol change: `coqod` maps kinds to `syntax/*` wire kinds exhaustively.
- `#72` builds on `Document`, `Symbol` (`container`, `name_range`, `qualified_name`) and `symbols_at`.
