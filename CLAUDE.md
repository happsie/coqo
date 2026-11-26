# CLAUDE.md: working on Coqo

Coqo is an agent-first desktop editor written in Rust: a GPUI UI plus a background daemon, `coqod`. The plan is in [`docs/PLAN.md`](docs/PLAN.md), and architecture decisions are in [`docs/adr/`](docs/adr/). Several agents work on this repo in parallel, and **GitHub issues are the single source of truth for what to work on and who is working on it.**

## GitHub issues: how work is organised

- **Milestones** `M0` … `M10` follow the roadmap in PLAN §12. Work in the **lowest open milestone** unless a maintainer says otherwise.
- **Epics** (`type:epic`) group a milestone's work as GitHub **sub-issues**. Don't implement an epic directly. Pick one of its sub-issues.
- **Tasks** (`type:task`) are sized for **one PR**. **Spikes** (`type:spike`) are time-boxed investigations whose output is an ADR plus a throwaway prototype under `spikes/`.
- **Dependencies** use GitHub's native *blocked by* links, and each issue's **Dependencies** section repeats them. An issue is unblocked only when every issue it is blocked by is **closed**.
- Labels:
  - `agent-ready`: fully specified, and an agent can take it once unblocked.
  - `human-needed`: needs a human decision or action. Don't take it. Comment if you can help prepare it.
  - `status:in-progress`, `status:blocked`: the workflow state (see below). PRs are merged without review, so `status:needs-review` isn't used.
  - `area:*`: the part of the system. Use it to avoid two agents editing the same area at once.

## Picking up an issue

1. **Find candidates:** open, `agent-ready`, in the current milestone, not assigned, not in progress, and **not blocked** (GitHub's native `is:blocked` knows about open blockers).
   ```sh
   gh issue list -R happsie/coqo --milestone "M0 · Spikes & foundations" --label agent-ready \
     --search "no:assignee -label:status:in-progress -is:blocked" --limit 50
   ```
2. **Double-check it's unblocked:** every issue it is blocked by must be closed.
   ```sh
   gh api repos/happsie/coqo/issues/<n>/dependencies/blocked_by --jq '.[] | "\(.number) \(.state) \(.title)"'
   ```
   If the API isn't available, read the issue's **Dependencies** section and check each `#n`.
3. **Claim it before writing code**, so other agents can see it's taken:
   ```sh
   gh issue edit <n> -R happsie/coqo --add-assignee @me --add-label status:in-progress
   gh issue comment <n> -R happsie/coqo --body "Claiming. Plan: <2–3 lines on the approach>."
   ```
   If someone claimed it in the meantime (re-check after your edit), un-claim it and pick another.
4. **Prefer issues in a different `area:*`** from the ones already `status:in-progress`, to avoid merge conflicts.

## Doing the work

- Branch: `issue-<n>-<short-slug>`, created from `main`.
- Stay inside the issue's **Scope**. If you find extra work, **open a new issue** (use the task template, link it to the right epic and dependencies) instead of growing the PR.
- Read the PLAN sections and any ADRs linked from the issue first. If the issue links a design board, match it.
- **Design canvas (private):** https://claude.ai/artifact/8oQ9MtXi4nJNA51jW2oN2m. It stays private on purpose. Read it with the **Artifact tool** (`action: "read"`, that `url`, and `path` set to the board file), **not** with WebFetch or curl. Board files, matching the "Board" names in issues:
  - `project/Main.dc.html`: 1 · Workbench (rail, files, full-width chat tab, ⚡ bolt popover)
  - `project/AnchoredQuestion.dc.html`: 2 · Anchored question
  - `project/Walkthrough.dc.html`: 3 · Code walkthrough
  - `project/DiffReview.dc.html`: 4 · Diff review
  - `project/PRInbox.dc.html`: 5 · PR Inbox
  - `project/AgentsAlerts.dc.html`: 6 · Agents & alerts
  - `project/TraceExplorer.dc.html`: 7 · Live trace
  - `project/ModsSettings.dc.html`: 8 · Settings

  If you have no Artifact tool, ask the maintainer for screenshots. Never copy the canvas into the public repo; take only the tokens and layout you need, and record the visual language in `docs/design/` as PLAN §14 describes.
- If the implementation has to differ from PLAN.md, write or update an ADR in `docs/adr/` and update PLAN.md in the same PR.
- Spikes: answer the questions in the issue, write `docs/adr/NNNN-<topic>.md` with findings, measurements and a clear **go / fallback** decision, and keep prototype code in `spikes/<id>/`.

## Opening and merging the PR

- The PR title is a conventional commit, e.g. `feat(daemon): idle shutdown policy (#42)`.
- The PR body includes `Closes #<n>`, a short summary, how you tested it, and screenshots for UI changes.
- Tick the issue's acceptance-criteria checkboxes as they are met, either in the issue or by listing them in the PR.
- **Merge it yourself, straight away. No review is needed.** Wait for CI to go green if the PR has checks, then squash-merge and delete the branch. `Closes #<n>` closes the issue.
  ```sh
  gh pr checks <pr> -R happsie/coqo --watch   # skip if no checks are reported
  gh pr merge <pr> -R happsie/coqo --squash --delete-branch
  gh issue edit <n> -R happsie/coqo --remove-label status:in-progress
  ```
- If CI fails, fix it on the branch and push again. Never merge a red PR, and never bypass branch protection.
- If the merge conflicts with `main`, rebase onto `main`, re-run the checks locally, push, and merge.

## When you get stuck

- **Blocked by another issue or a decision:** add `status:blocked`, comment with what you need and from whom, then unassign yourself if you're moving on.
- **Spec unclear:** comment on the issue with specific questions. Don't guess on anything user-visible or architectural.
- **Abandoning work:** comment with your progress, push your branch, then remove the assignee and the `status:*` labels.

## Definition of done (every task)

- CI is green (fmt, clippy `-D warnings`, tests, `cargo-deny`, API-boundary check).
- New behaviour has tests, and UI work matches its design board.
- Docs, ADR and PLAN.md are updated if the design changed.
- The PR is linked with `Closes #<n>` and merged.

## Hard rules

- **Claude policy (PLAN §7.3):**
  - Never implement a claude.ai login or OAuth flow in Coqo.
  - Never read `~/.claude` credential files.
  - Never modify or wrap the `claude` binary.
  - Never proxy subscription traffic.
  - Coqo only launches the user's own, unmodified Claude Code, and the user signs in there.
- **Licences:** Apache-2.0 project. Never add GPL/AGPL dependencies. Zed's `editor`, `project` and `language` crates are GPL and off limits; `gpui` and `gpui-component` are Apache-2.0 and fine.
- **Mod boundary:** crates under `mods/` depend only on `coqo-mod-api`, never on kernel internals.
- **Secrets:** only in the OS keychain, never in files, logs or test fixtures.
