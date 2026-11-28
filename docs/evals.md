# Agent evals

Does Claude use Coqo's editor tools at the right moments? `coqo-evals` (`crates/evals`) runs a few short scenarios against **your own, signed-in Claude Code** and checks which `coqo-editor` tools it called (#216).

It spends real usage (each scenario is one short prompt, about 25–30k context tokens), so it is **never part of `cargo test` or CI**. CI only builds the crate and runs its unit tests, which check the scenario checks.

## Running it

```sh
cargo run -p coqo-evals                    # all scenarios, with Coqo's operating guide
cargo run -p coqo-evals -- --without-guide # the same with the guide off (COQO_CLAUDE_GUIDE=0), for an A/B
cargo run -p coqo-evals -- --only show-where --keep
cargo run -p coqo-evals -- --list
```

You need what the chat needs: `claude` on `PATH` and signed in, Node.js 22 or newer, and the Claude ACP adapter installed in Coqo's data directory (from the chat's install button).

What it does:

1. Builds `coqod` (same profile), unless you pass `--coqod <path>`.
2. Starts a private `coqod --foreground` with a temporary `COQO_RUNTIME_DIR` and `COQO_DATA_DIR` under `/tmp/coqo-eval-<pid>` (a short path, for the socket). It copies the adapter from your data directory there and never touches your running Coqo.
3. For each scenario, copies its fixture (`evals/fixtures/<name>`) to a fresh folder, opens it as a workspace and starts a `claude-acp` session. It plays the UI over the protocol: it publishes the scenario's editor context (`editor/context`: open tabs and a selection), follows `editor/reveals`, answers the agent's questions (`questions/answer`) with the scenario's answer, and answers permission requests (allow, except shell commands, which it rejects; whatever your Claude Code settings already allow is not asked).
4. After the turn, reads the session's tool-use stats (`SessionInfo.toolUse`) plus the questions' anchors and the reveals, and checks them.
5. Prints a Markdown table: Coqo tools called, other tool calls, context tokens at the end of the turn, pass or fail. Then it stops the daemon and deletes the temporary directory (`--keep` leaves it, with `coqod.log` and the transcripts in its `coqo.db`).

Claude Code saves its own transcripts of these sessions, as of any session, under its own directory for the temporary folders. The eval never reads, changes or deletes anything there.

## Scenarios

| Name | Prompt (shortened) | Editor context | Passes when |
|---|---|---|---|
| `explain-selection` | "Explain the selected function" | `pricing.py`, `apply_discount` selected | `editor_context` called |
| `ambiguous-default` | "Turn on free shipping in the config. I haven't decided on the threshold yet, and that's my call, not yours." (nothing in the code hints at a value) | `config.py`, `pricing.py` open | `ask_question` anchored to `config.py` lines 6–10 |
| `skill-asks` | a skill's instructions say to ask decisions with `AskUserQuestion`; free shipping, threshold undecided (#346) | `config.py` open | a question form asked (`AskUserQuestion`) and no `ask_question` |
| `three-steps` | sales tax "in three phases": a `TAX_RATE` setting, applying it in `total()`, then tests and a README line | none | `set_status` at least twice and `set_acceptance_criteria` |
| `show-where` | "Show me where refunds are handled" | `README.md` open | `editor_reveal` of `orders.py` |
| `plain-question` | list vs tuple, no code | none | no Coqo tool at all (no over-use) |
| `walkthrough` | "Walk me through this change." on `evals/changes/shop-tax-rate.patch` (uncommitted, on a one-commit repo of the fixture) | none | `publish_walkthrough` called, and its walkthrough covers every hunk of `git diff -U0 HEAD` with a step anchor or a trivial hunk (the coverage guard's rule, #99) |
| `address-review` | no typed prompt: two pending review comments on the same change (round to cents in `apply_tax`; test a zero tax rate) are sent with `review/send`, as "Send 2 comments to Claude" does (#95) | none | `reply_to_review` at least twice, and an agent reply in every thread of the review |

Add a scenario in `crates/evals/src/scenarios.rs` (a prompt or review comments, the editor's tabs, an answer for questions, and a check over the outcome); add fixture files under `evals/fixtures/`, and a change to ask about as a patch under `evals/changes/`. Keep prompts short: every run costs usage.

Shell commands are rejected, except plain read-only git ones (`git diff`, `status`, `log`, `show`, with nothing chained or redirected), which a walkthrough needs.

## Baseline

Claude Code 2.1.332, adapter 0.113.0, Coqo's Claude Code plugin loaded, guide v3, 2026-11-27 (#238). Results vary between runs; treat one run as a smoke test, not a score.

| Scenario | Guide on: Coqo tools, context tokens | Guide off: Coqo tools, context tokens |
|---|---|---|
| `explain-selection` | pass: `editor_context`, 26.4k | pass: `editor_context`, 25.9k |
| `ambiguous-default` | pass: `set_acceptance_criteria`×2, `set_status`×2, `ask_question`, `editor_reveal`, 36.8k | pass: `ask_question`, `publish_walkthrough`, 36.6k |
| `three-steps` | pass: `set_acceptance_criteria`×2, `set_status`×5, `publish_walkthrough`, 42.8k | pass: `set_acceptance_criteria`×2, `set_status`×4, `publish_walkthrough`, 40.3k |
| `show-where` | pass: `editor_reveal`, 28.1k | pass: `editor_reveal`, 27.6k |
| `plain-question` | pass: none, 23.5k | pass: none, 23.0k |
| `walkthrough` | pass: `publish_walkthrough` (6 steps, 0 of 8 hunks uncovered), 39.3k | pass: `publish_walkthrough` (5 steps, 0 of 8 hunks uncovered), 38.7k |
| **Passed** | **6/6** | **6/6** |

What changed since #216 (guide v1, 3/5 either way): the two scenarios were reworded to test the behaviour fairly (`ambiguous-default` used to say "give it a sensible default", which hands Claude the choice; `three-steps` is now a task in named phases over four files), and the triggers got sharper in three places: two rules at the top of the guide, the first sentence of the `set_status` and `ask_question` descriptions, and the MCP `instructions`. Since the instructions reach Claude with or without the guide, the guide-off column passes too; the guide's own effect is small now.

In `ambiguous-default`, both runs asked with `ask_question` anchored to the threshold in `config.py` (lines 6–10) rather than picking a value. The first guide-on run asked a clarification at line 7 (and set 50.0 only after the answer) but the eval then tried to answer it a second time, when the answered question came back as an update; the eval now answers each question once, while it is open, and the row is from a re-run.

Over-use to watch: for what becomes a one-line edit, `ambiguous-default` also got acceptance criteria and statuses (guide on) or a walkthrough (guide off). `plain-question` only covers a question with no code; a one-step edit scenario is #255.

`address-review` (#95, guide v4, 2026-11-28, one run): pass with `reply_to_review`×2 (both threads resolved, each reply saying what changed and why), plus `set_acceptance_criteria`×2 and `set_status`, 37.8k context tokens.

Earlier, #216 (guide v1): `explain-selection`, `show-where` and `plain-question` passed with and without the guide; `ambiguous-default` failed both ways (Claude picked 50.0 itself); `three-steps` got `set_acceptance_criteria` and `editor_reveal` but no `set_status` with the guide, and no Coqo tool without it. The guide costs about 0.2–0.5k context tokens per turn.

Guide v5 (#346, 2026-11-28, one run of six scenarios, guide on): 6/6 pass. `skill-asks` used `AskUserQuestion` and no `ask_question` (29.5k tokens); `ambiguous-default` still used `ask_question` anchored to `config.py` (32.3k); `explain-selection` 27.9k, `three-steps` 42.3k (`set_acceptance_criteria`×2, `set_status`×4), `walkthrough` 40.8k, `address-review` 36.4k (`reply_to_review`×2). `show-where` and `plain-question` were not re-run (unchanged by the guide's edit).
