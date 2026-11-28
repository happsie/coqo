# 30. Third-party skills' questions and findings, shown in the code

- **Status:** Accepted
- **Date:** 2026-11-28
- **Spike:** S9, issue #340. Prototype in [`spikes/s9-skill-questions/`](../../spikes/s9-skill-questions/). Builds on [ADR 0016](0016-anchored-questions.md) (questions), [ADR 0013](0013-anchors.md) (anchors) and the chat question card of #339.

## Context

Users bring their own Claude Code skills and plugins (PLAN §7.3). These ask questions their own way:

- through `AskUserQuestion`;
- through plan approval;
- through an MCP server's elicitation;
- in plain text.

They also report findings in plain text, as `file:line — issue`. The maintainer wants them to keep working that way while Coqo shows them its own way (2026-11-28): "I want user skills to work the way they like, but still keep coqo transforming those requests to anchored questions."

The constraints:

- Coqo translates the exchange where it is shown.
- It never edits a skill.
- It never blocks a tool a skill calls.
- The answer reaches the skill in the shape the skill expects.
- The chat always keeps a compact card.

#339 renders the agents' own question tools as a chat card. This spike asks five things:

1. Can such a question be pinned to the code, and from which signals?
2. Can findings written as text become gutter notes?
3. Is there a useful heuristic for a turn that ends with a question in plain text?
4. Is a hook in Coqo's session-only plugin a policy-safe fallback?
5. How do real third-party skills behave?

## What we tested

- **The adapter, read statically.** We read the code of claude-agent-acp 0.113.0, the pinned version, and of the Claude Agent SDK 0.3.331 under it. It is installed in Coqo's data directory, and we read it there. We traced `AskUserQuestion`, `ExitPlanMode` and elicitations. This needed no prompts.
- **The prototype.** `spikes/s9-skill-questions` is a standalone crate, outside the workspace and CI. Its parts:
  - `refs.rs`: a `file:line` detector. It knows these forms: `path:12`, `path:12-14`, `path:12:5`, `path#L12`, GitHub blob links, `path (line 12)` and `line 12 of path`. It checks each reference against the workspace: an exact path, or the unique file the name ends with, narrowed by the turn's files, and the line must exist.
  - `anchor.rs`: an anchor resolver for a question in #339's provider-neutral shape (text, header, options with description and preview). It also takes the turn's tool calls (path, plus lines for edits) and the selection.
  - `findings.rs`: turns a message into gutter notes.
  - `open_question.rs`: classifies how a turn ends.
  - `cargo run --release` prints every table below (`results.md`).
- **The samples.** The samples are constructed. Treat the numbers as indicative.
  - 38 structured questions over a small TypeScript fixture. Each is modelled on one of the tested skills, with the turn's tool calls and a ground-truth anchor, or "chat only", or either.
  - 5 review outputs, written in each review skill's published output format.
  - 18 turn endings.
  - The heuristics were tuned once on these samples after a first pass, so the samples are training data. The first-pass numbers are given too.
- **Real runs: 2 prompts.** They ran on the user's own Claude Code, unmodified, in a temporary Coqo session (`coqo-evals` with a private `coqod` and data directory, patch in `spikes/s9-skill-questions/evals-skill-runs.patch`). Each skill was copied into the temporary workspace's `.claude/skills/`, which was then deleted. Nothing was installed into the user's Claude Code configuration, and nothing under `~/.claude` was read.
  - superpowers `brainstorming` on the eval's shop fixture.
  - superpowers `requesting-code-review` on the eval's tax-rate change.
- **A real corpus.** We took 156 review comments that Claude wrote on 30 public GitHub pull requests: claude-code-action reviews and the official code-review plugin. Each came with its repository's file list at the PR head and the line counts of the files it names. `fetch-corpus.py` collects them. The comments belong to their authors and are not committed. Only the counts below are.
- **The skills' source, read.** Each file was read from its public repository. None was installed. Short quotes only, with links:
  - [obra/superpowers](https://github.com/obra/superpowers) (MIT): `brainstorming`, `requesting-code-review`, `executing-plans`.
  - [garrytan/gstack](https://github.com/garrytan/gstack) (MIT): `/review`, `/office-hours`, `/plan-eng-review` and their shared preamble.
  - [anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official) (Apache-2.0): `code-review`, `feature-dev`, `pr-review-toolkit`.

## Findings

### How the adapter carries a question (question 1)

- **`AskUserQuestion` is a form elicitation.**
  - The SDK routes the tool through `canUseTool`. The adapter turns it into ACP `elicitation/create` with `mode: "form"` and the tool call's `toolCallId` (`dist/elicitation.js`, `askUserQuestionsToCreateRequest`).
  - Each of the 1–4 questions becomes a field `question_<n>`. A single-select question is a `oneOf` of the option labels, and a multi-select one is an `anyOf`. The description rides on each option.
  - An option's `preview` goes under `_meta["_claude/askUserQuestionOption"]`.
  - Each question also gets a free-text field, `question_<n>_custom` ("Other").
  - For one question, the question text is the request's `message`. For several, each field's `description` holds its question.
- **The answer goes back in the CLI's own shape.**
  - The adapter folds the response into the tool's `updatedInput.answers`, keyed by the exact question text, with the label as the value. A multi-select answer is joined with commas. A typed note next to a pick goes into `annotations[question].notes`.
  - The tool's own `call()` reads this exactly as it reads the CLI dialog's answer.
  - `decline` answers with nothing: the model is told the user skipped. `cancel` aborts the tool use.
  - So if Coqo answers the elicitation with the option label unchanged, the skill gets what it would get in the terminal.
- **Today Coqo sessions have no `AskUserQuestion` at all.**
  - The adapter offers the tool only when the client advertises `clientCapabilities.elicitation.form`. Otherwise it adds the tool to `disallowedTools` (`dist/v1/acp-agent.js`).
  - Coqo's ACP client does not advertise it yet. #339 will.
- **The other question routes.**
  - `ExitPlanMode` is a permission request with options.
  - An MCP server's elicitation takes the same `elicitation/create` path.
  - Claude Code's own confirmations come as `AskUserQuestion` under a `confirm_<uuid>` id, with no tool call.
- **Skills depend on the exact text.** gstack writes `<gstack-qid:…>` markers into the question text and a `(recommended)` suffix on one label. Its own `PreToolUse` and `PostToolUse` hooks on `AskUserQuestion` parse both, to auto-decide and to log. Coqo must therefore never rewrite a question's text or labels: anchoring only adds where the question is shown.

### Anchor sources (question 1)

38 questions. 24 belong in the code, 8 only in the chat, and 6 either way. Plans are always chat-only (see the decision).

**Each source on its own:**

| Source | Fired | Right place (precision) | Recall (of 24) |
|---|---|---|---|
| Explicit `path:line` in the question, header, options or previews | 7 | **100 %** (7/7) | 29 % |
| Option preview quoting lines that are in a file (2+ lines in a row) | 1 | 100 % (1/1) | 4 % |
| Words and identifiers of the question in the turn's read or edited files | 19 | **68 %** (13/19) | 50 % |
| Selection, gated: the prompt points at it ("this", "here") and shares a term, or the question's terms are in it | 3 | 100 % (3/3) | 12 % |
| Selection, ungated (any selection) | 6 | 50 % (3/6) | – |

Notes on the sources:

- **The turn's files level off at about 70 %.** Their precision stays at 68–86 % for every lowest score from 2 to 6, and reaches 100 % only at 7, with 25 % recall.
- **The first pass was worse.** Before the one round of tuning, the turn's files scored 58 % (7/12) with 25 % recall. The tuning added a 3-line window, 4-letter stems, skipped import lines and gave an edited line one extra term.
- **They miss abstract questions.** Their wrong anchors come from questions about the product ("What do you want someone to do when they land on the order page?" pinned to a SQL line) and from approach choices.
- **The selection needs its gate.** Without the gate, a stale selection pins an unrelated question (2 of 6). An MCP server's "Create the pull request?" after "open a PR for this" was the case where the gate's single shared term mattered.

**The cascade:** the first source that finds a place; otherwise the question stays in the chat, with "Pin to code".

| Order | Right | Pinned wrongly | Left in the chat but belonged in the code |
|---|---|---|---|
| explicit → preview → turn files → selection (gated) | 82 % (31/38) | 8 % (3) | 11 % (4) |
| explicit → preview → selection (gated), no turn files | 66 % (25/38) | **0 %** | 34 % (13) |
| explicit only | 55 % (21/38) | 0 % | 45 % (17) |
| explicit → turn files → selection, ungated | 71 % | 16 % | 13 % |

Other things the explicit source showed:

- **A wrong line falls through.** A finding with a line past the end of the file (gstack's `cart.ts:120`) moves on to the next source instead of pinning a wrong line.
- **A bare file name resolves.** `orders.ts:42` and `index.ts:2` (two files have that name) resolved through the turn's files.
- **Things that look like references are not.** `10:30`, `localhost:3000`, `Node 20.11` and `1:4` never resolve.

#286's `agents/changedBy` knows only which session last changed a file, not which lines. The turn's edit tool calls carry their hunks' lines (`locations[].line` from the adapter's `file-edit` reporter), so they are the better signal within a turn. `changedBy` can only rank files after a restart.

### Findings written as text (question 2)

**Samples** (`real-coqo-*` are the two real runs):

| Output | Findings | Right | False positives | Missed |
|---|---|---|---|---|
| gstack `/review` format | 4 | 4 | 0 | 0 |
| superpowers code-reviewer format | 6 | 6 | 0 | 0 |
| pr-review-toolkit `[file:line]` format | 5 | 5 | 0 | 0 |
| code-review plugin (blob links) | 2 | 2 | 0 | 0 |
| feature-dev reviewer ("`path`, line 30") | 3 | 3 | 0 | 0 |
| real: brainstorming in Coqo | 5 | 5 | 0 | 0 |
| real: requesting-code-review in Coqo | 10 | 9 | 0 | 1 |
| **all** | **35** | **34** | **0** | **1** |

- **The miss** is a bare continuation, "`shop/orders.py:17-18`, `:32`, `:35`". The `:32` has no path. That is cheap to add.
- **Without the workspace check** (syntax only: a path with an extension or a directory, outside code blocks), 2 of 36 references would be wrong. Both are real paths with a wrong line or a missing file (`cart.ts:120`, `CLAUDE.md:12`).
- **Code blocks are skipped.** Stack traces and diffs in fenced code are not findings.

**The real corpus** (156 comments, 30 PRs):

- **What the syntax pass finds.** It finds 165 plausible references outside code. 18 % (30/165) of them are not a real file or line at the PR head:
  - 16 name no such file, mostly stale names, plus `localhost:8080` and `bpm:120`;
  - 3 are past the end of the file;
  - 19 are file names that several files share, which the corpus can't narrow without the turn's files;
  - 54 point at a real file whose length the corpus doesn't have.
- **With the workspace check, nothing wrong is shown.** None of the 81 references shown at a checked line is a bad path or line, by construction.
- **Spot check of the meaning.** For 15 random references, does the line say what the comment says?
  - All 9 from pull requests whose head matched the reviewed commit were right.
  - All 6 from one pull request whose head had moved on after the review had drifted.
  - Coqo detects references while the message arrives, against the live files, and pins them with `coqo-anchors` at once, so this drift does not apply.
- **Misses.** A hand check of the lines that name a repository file and a line number but gave no reference found about 3 real misses in roughly 170: "`.gitignore` change (line 157)", and "`CLAUDE.md` … (line 50)" with words in between.

### Plain-text questions (question 3)

- **On the 18 endings** the classifier was right 16 times (89 %), and 11 of 16 before tuning. A question that names code was found with 100 % precision (6/6) and 86 % recall.
- **Real output rarely ends with a question mark.**
  - None of the 156 real review comments ends with a question.
  - Both real Coqo runs ended with an imperative instead: "Tell me if I've got any of this wrong." and "…are your decisions to make. Tell me which way you want each". The "?" heuristic misses both, and the second one is exactly the decision the user should see in the code.
  - Many endings that do end in "?" are offers ("Want me to fix these?"), which name no place.

### The hook fallback (question 4)

The mechanism is documented:

- Since Claude Code 2.1.85, a `PreToolUse` hook can answer `AskUserQuestion`. It returns `permissionDecision: "allow"` with `updatedInput` holding `questions` and `answers`. Its own UI collects the answers. See the [hooks reference](https://code.claude.com/docs/en/hooks) and the [changelog](https://code.claude.com/docs/en/changelog).
- Plugin hooks (`hooks/hooks.json`) load with `--plugin-dir` and the SDK's local `plugins` option, which Coqo already uses for its plugin (`agent-claude::plugin::session_meta`).

Against PLAN §7.3:

| Rule | The hook |
|---|---|
| No change to the `claude` binary | ✓ The hook is a plugin file; the binary is untouched |
| No wrapping | ✓ Claude Code runs the hook as its own documented extension; Coqo does not sit between it and the user |
| Loaded only for Coqo sessions | ✓ The same session-only plugin directory under Coqo's data directory; nothing goes into the user's configuration |
| No credentials, no proxying | ✓ The hook talks to `coqod` over its socket, never to Anthropic |

So the hook is policy-safe. It is still the wrong default, for three reasons:

- **It isn't needed for ACP sessions.** The adapter route carries the question once Coqo advertises form elicitation (#339).
- **It competes with the skill's own hooks.** gstack's auto-decide `PreToolUse` hook answers the same tool. Hook results combine as deny > defer > ask > allow, and two `allow`s with different `updatedInput` have no documented winner. Coqo's hook could override a skill's own decision, which is the opposite of letting skills work their way. Through the adapter, Claude Code runs the skill's hooks first and calls `canUseTool` only when they leave the question to the user.
- **It has a time limit.** A command hook times out after 600 s by default, and the output of a timed-out hook is discarded. A question can wait longer than that.

### The tested skills (question 5)

| Skill | How it asks or reports | In Coqo today | With this decision |
|---|---|---|---|
| superpowers `brainstorming` (MIT) | One open question per message, in prose; play-backs ending "what's wrong or missing" | **Real run:** Claude used Coqo's `ask_question`, pinned to `shop/config.py:7`, instead of ending its message with the question. The cause is guide rule 2: "do not ask in chat: call ask_question". The answer came back as a tool result in the same turn, and the skill went on to its play-back. The content was kept, but the conversation's shape changed. | The guide defers to the skill's own way of asking (#346). A prose question stays prose. Its `file:line` references (5/5) become gutter notes |
| superpowers `requesting-code-review` (MIT) | Strengths / Issues / Assessment with `File: path:line` | **Real run:** 10 findings, 9 detected and none wrong. It left 3 decisions in prose ("Tell me which way you want each") and did *not* call `ask_question`. Guide rule 2 is not applied consistently either. | Findings become gutter notes linked to the message. The decisions stay in the chat |
| gstack `/review`, `/office-hours`, `/plan-eng-review` (MIT; read only, since it needs its own install) | `AskUserQuestion` decision briefs with `file:line` in the text; its own hooks on the tool; a documented prose fallback when the tool is missing | `AskUserQuestion` is not offered, so by its preamble gstack uses the prose fallback, and its hooks (auto-decide, logging) never fire | The tool exists again (#339). Its hooks run in Claude Code as written. The question is pinned to its explicit `file:line` (100 % precision). The answer is the unchanged label |
| `feature-dev` (Apache-2.0) | Phase 3 lists its clarifying questions; Phase 4 and Phase 6 ask in prose | Prose, or `ask_question` because of the guide | With `AskUserQuestion` back, Claude may pick it. Phase 6 findings carry `file:line`, Phase 3 questions usually don't (the turn's files would be a guess) |
| `code-review` (Apache-2.0) | Non-interactive; findings as GitHub blob links | – | Blob links are detected (2/2) |

**Do the skills get the answers they expect?**

- **Through the elicitation, yes.** The answer is the label the skill offered, keyed by its question text, in the CLI's shape. gstack's `(recommended)` and qid markers survive because Coqo never rewrites them.
- **Through Coqo's `ask_question`, not quite.** That is today's behaviour for prose-asking skills, caused by guide rule 2. The skill's question becomes a different tool with a different result shape, and a hook that a skill sets on `AskUserQuestion` never sees it.

## Decision

1. **Structured questions: go, on the adapter route.**
   - `AskUserQuestion`, MCP elicitations and permission requests with options reach Coqo as #339's question. Coqo answers through the original request, with the labels and question text unchanged. Esc and cancel are a `decline`, never a hang and never a block.
   - Coqo pins the question to code when one of these finds a place: an explicit `path:line`, an option preview, or the selection behind its gate. All three were 100 % precise on the samples.
   - **The fallback:** otherwise it stays a chat card, and "Pin to code" pins it by hand.
   - Plan approval (`ExitPlanMode`) is never pinned. A plan spans files, so its card links them instead.
   - The turn's files (about 70 % precise) do not pin on their own. Whether they offer a one-click suggestion is open question 2.
   - The chat always keeps the compact card, as ADR 0016's chat card does for `ask_question`.
   - The question goes into the ADR 0016 store as a question whose origin is the pending request, so markers, the popover, the pane, F8, alerts and `needs_input` all apply.
2. **Findings in text: go.**
   - References in an assistant message become dismissible gutter notes linked back to that message. They must be outside code blocks and pass the workspace check: a real file, resolved by the turn's files when the name is ambiguous, at a real line.
   - They are pinned with `coqo-anchors` when the message arrives, so they follow edits.
   - In the samples this gave 0 false positives with 97 % recall. In the real corpus the check removed the 18 % that a syntax-only detector would have shown wrongly.
   - Whether this is on by default is open question 3.
3. **Plain-text questions: no automatic pinning.**
   - The heuristic catches a "?" that names code. Real skill output ends with "Tell me…" instead: 0 of 156 real comments and neither real run ended with a question mark.
   - **The fallback:** the manual "Pin to code" on a chat message, which pins it to the message's first detected reference, or to a place the user picks.
4. **The hook: policy-safe, not used for ACP.**
   - The adapter route covers ACP sessions. Coqo ships no `PreToolUse` hook for `AskUserQuestion` there, so a skill's own hooks keep the last word.
   - The hook stays the documented fallback for the terminal-agent mode (the Claude Code TUI in a Coqo terminal), if Coqo's plugin is ever loaded there. It isn't today.
5. **The guide follows the skill.** Guide v5 tells Claude to ask the way the skill it follows says (its own format, or `AskUserQuestion`), and to keep `ask_question` for Coqo's own judgement calls. Coqo then pins the question itself.

## Open questions for the maintainer

1. **The setting for structured questions.** What is its name, which values does it take, and what is the default? The issue suggests `questions.anchor_skill_questions = auto|ask|off`, and this ADR assumes `auto`, from the maintainer's intent. Should `ask` (pin only after a click) exist at all?
2. **The turn's files.** Should a guess from the turn's files show on the chat card as a one-click suggestion, such as "Pin to `cart.ts:33`?" Or should it pin with a "guessed" look? Or not show at all? The guess is about 70 % precise, and wrong 3 times in 38 in the cascade.
3. **Findings as gutter notes.** On by default? A possible name is `questions.findings_in_gutter = auto|off`. Should a note's colour follow the severity words ("critical" and "important" as warnings, everything else as info)? And should the notes of one message be dismissible all at once?
4. **A form with several questions.** An `AskUserQuestion` form can hold 2–4 questions, each with its own anchor. Should it be one popover at the first question's anchor, listing the rest as "Also anchored"? Or one marker per question, all answered together?
5. **"Pin to code" on any message.** Is the manual action for plain-text questions (decision 3) wanted in the first version?

## Consequences

- **Skills keep their own tools and hooks.** Coqo changes only where a question is shown. A skill that asks with `AskUserQuestion` gets the label it offered, keyed by its question text, as in the terminal.
- **#339 must advertise `elicitation.form`.** Until it does, `AskUserQuestion` isn't offered in Coqo sessions, and gstack-style skills fall back to prose.
- **The question store gains an origin.** Pending ACP requests become questions, and answering one answers the request instead of returning an MCP tool result (#344).
- **The guide changes.** Rule 2 of the operating guide (v4) changes how skills ask today. v5 narrows it, and `coqo-evals` must still pass `ambiguous-default` (#346).
- **Two detector forms are cheap fixes for the implementation:** bare continuations (`, :32`) and "`file` … (line N)" with words in between.
- **Follow-ups under #68:**
  - #344: pinning structured skill questions (blocked by #339);
  - #345: findings as gutter notes (needs the maintainer's answers to questions 3 and 5);
  - #346: guide v5 (blocked by #339).
