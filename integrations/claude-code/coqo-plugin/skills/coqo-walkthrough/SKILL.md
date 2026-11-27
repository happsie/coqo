---
name: coqo-walkthrough
description: Write a code walkthrough of a change and publish it to Coqo with the coqo-editor `publish_walkthrough` tool. Use when the user asks to explain, walk through or present a change (uncommitted edits, commits, a branch or a PR), and when you finish a change of more than a line or two in Coqo, before you say it is done. Not for explaining one function or for a general question.
---

# Coqo walkthrough

A walkthrough is the change in the order a reader should follow it: steps anchored to the code, each saying what the code does, why, and what could go wrong. Coqo replays it in the editor step by step and checks that every changed hunk is covered. Publish it with the `coqo-editor` MCP tool `publish_walkthrough`; its schema is also the MCP resource `coqo://schemas/walkthrough.json`.

## 1. Find the change and every hunk

- Scope: uncommitted edits are `worktree` (against `HEAD` unless you name `base`); commits or a branch are `commits` with `base` and `head`; a pull request is `pr`.
- List the hunks with `git diff -U0` for the scope (`git diff -U0 HEAD`, or `git diff -U0 <base>...<head>`). In `@@ -a,b +c,d @@`, `c` to `c+d-1` are the hunk's lines in the new file. Include new, untracked files (`git status --short`).
- Read the changed files as they are now: anchors use today's line numbers.

## 2. Group hunks into steps, in reading order

Order the steps the way a reader follows the code, not the order of the diff or of the files:

1. `entry_point`: where the behaviour starts (the public function, command, route or UI action).
2. `flow`: how a call moves through the code from there.
3. `data_model`: types, schemas and state the flow reads or writes.
4. `logic`: the rules and calculations.
5. `edge_case`: limits, errors, empty and concurrent cases.
6. `test`: the tests, and what they prove.

Put `config` and `migration` steps right before the step that needs them. One idea per step; a step may anchor several hunks in several files. Most changes need 3 to 8 steps. Use `depends_on` when a step only makes sense after another.

## 3. Write each step

- `id`: short and unique (`s1`, `discount-rule`); `kind`: from the list above.
- `title`: a few words.
- `what`: what this code does now, for someone who has not seen it. Explain behaviour, not the diff ("adds a line").
- `why`: the reason for this design, the constraint it meets, the alternative you turned down.
- `risks`: concrete things that could break and how they would show (`{ "severity": "low" | "med" | "high", "text": ... }`). Leave it empty only if there really are none.
- `anchors`: `{ path, start_line, end_line, text }` with 1-based lines of the file as it is now, as tight as the code the step explains. Set `text` to one distinctive line from that range, so a stale line number is caught.

## 4. Cover every hunk

Every hunk must overlap a step's anchor or be listed in `trivial_hunks` as `{ path, start_line, end_line, reason }` (formatting, imports, renames, generated files). A deleted file goes in `trivial_hunks` with just its `path` and a reason, or is explained in the step that replaces it. For lines removed inside a file, anchor the code around the gap. Before publishing, check your hunk list against the anchors.

## 5. Refer to acceptance criteria

If the session has acceptance criteria (`set_acceptance_criteria`), leave `acceptance_criteria` out (it defaults to the session's) and put their ids in each step's `criteria_refs`. Criteria from the user or an issue that are not registered go in `acceptance_criteria` with `id` and `text`. Each criterion should be referenced by at least one step; if one is not met, say so in `summary`.

## 6. Publish, then fix

Call `publish_walkthrough` with `title`, `summary` (two to five sentences: what changed and why), `scope`, `steps` and `trivial_hunks`. If it refuses, it lists every problem with its JSON path: fix them all (re-read the file for a wrong line or `text`) and call it again. Never drop a step or a hunk just to get it accepted. If the result starts with `COVERAGE INCOMPLETE`, it lists the changed hunks no step or trivial entry covers: add each to a step's anchors (or to `trivial_hunks` with a reason) and publish again. Set `require_coverage: true` to have an incomplete walkthrough refused instead.

When it is published, tell the user in one or two sentences; Coqo shows the walkthrough, so do not repeat it in the chat. If `publish_walkthrough` is not available, you are not in Coqo: give the same steps in the chat instead.
