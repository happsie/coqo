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
| `ambiguous-default` | "I'm not sure what the free-shipping threshold should default to…" | `config.py`, `pricing.py` open | `ask_question` anchored to `config.py` lines 6–10 |
| `three-steps` | add `tax_rate` to `total()`, a test, and a README line | none | `set_status` at least twice and `set_acceptance_criteria` |
| `show-where` | "Show me where refunds are handled" | `README.md` open | `editor_reveal` of `orders.py` |
| `plain-question` | list vs tuple, no code | none | no Coqo tool at all (no over-use) |

Add a scenario in `crates/evals/src/scenarios.rs` (a prompt, the editor's tabs, an answer for questions, and a check over the outcome); add fixture files under `evals/fixtures/`. Keep prompts short: every run costs usage.

## Baseline

Claude Code 2.1.332, adapter 0.113.0, guide v1, 2026-11-27 (#216). Results vary between runs; treat one run as a smoke test, not a score.

| Scenario | Guide on: Coqo tools, context tokens | Guide off: Coqo tools, context tokens |
|---|---|---|
| `explain-selection` | pass: `editor_context`, 25.8k | pass: `editor_context`, 25.6k |
| `ambiguous-default` | FAIL: none (it picked 50.0 itself), 28.0k | FAIL: none, 26.9k |
| `three-steps` | FAIL: `set_acceptance_criteria`×2, `editor_reveal` (no `set_status`), 31.7k | FAIL: none, 27.4k |
| `show-where` | pass: `editor_reveal`, 27.5k | pass: `editor_reveal`, 27.1k |
| `plain-question` | pass: none, 23.2k | pass: none, 22.7k |
| **Passed** | **3/5** | **3/5** |

The guide costs about 0.2–0.5k context tokens per turn. With it, the multi-step task got acceptance criteria (registered, then verified) and a reveal of the change; without it, no Coqo tool. Neither run asked a question in `ambiguous-default`: the prompt says "give it a sensible default", which Claude reads as permission to decide.
