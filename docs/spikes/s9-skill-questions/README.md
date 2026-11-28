# Spike S9: skills' questions and findings in the code

Throwaway prototype for issue #340. The findings and the decision are in [`docs/adr/0030-skill-questions-in-code.md`](../../adr/0030-skill-questions-in-code.md).

This directory is its own Cargo workspace, so the Coqo workspace and CI don't build it.

| Path | What it is |
|---|---|
| `src/refs.rs` | The `file:line` detector. It finds references and checks each one against the workspace. |
| `src/anchor.rs` | The anchor resolver for a structured question. Its sources are an explicit `path:line`, an option preview, the turn's files and the selection. |
| `src/findings.rs` | Turns a message's references into gutter notes. |
| `src/open_question.rs` | Classifies how a turn ends: no question, an offer, a question that names code, or another question. |
| `src/main.rs` | Prints the measurements as Markdown (`results.md`). |
| `samples/questions.json` | 38 structured questions, with the turn's tool calls, the selection and the ground-truth anchor. |
| `samples/workspace/` | The small TypeScript fixture the questions are about. |
| `samples/findings/` | Review outputs: five in each skill's published format, and two real runs in a temporary Coqo session (`real-coqo-*`). |
| `samples/shop/`, `samples/shop-taxed/` | The eval's shop fixture, without and with `evals/changes/shop-tax-rate.patch`. The real runs were made on these. |
| `samples/endings.json` | Turn endings for the plain-text heuristic. |
| `fetch-corpus.py` | Collects public Claude review comments with each repository's file list. The output is not committed. |
| `evals-skill-runs.patch` | The local-only change to `coqo-evals` that ran the two real prompts. |

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo test --release
cargo run --release > results.md
python3 -I fetch-corpus.py /some/dir/reviews.json 15   # spends GitHub API calls
cargo run --release -- --corpus /some/dir/reviews.json
```

To rerun the real prompts, apply `evals-skill-runs.patch` and put each skill under `<dir>/<scenario>/.claude/skills/`. Then run `COQO_EVAL_EXTRA=<dir> cargo run -p coqo-evals -- --only s9-brainstorm`. Each run spends one prompt of your own Claude Code usage.
