# 24. Agent-drafted commit messages

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #224 (epic #90; PLAN §7.7, §7.3)

## Context

PLAN §7.7 asks for commits "with an agent-drafted message". The Git pane's commit box (#91) needed a button that asks an agent for a message for what is staged. Four things had to be settled:

- which agent session does the drafting, without changing the user's chats;
- what the agent is told, and how much of a big diff it gets;
- how to keep a drafting agent from doing anything but answering;
- what the user sees while it works, and when nothing can draft.

## Decision

**A short-lived session of its own.** Each draft starts a new session, and closes it when the reply is in or the user cancels:
- **Provider:** the default one: the user's own Claude Code over ACP (`claude-acp`) when it is available, else the first available provider. Coqo only launches the user's unmodified Claude Code (PLAN §7.3).
- **Title:** "Commit message".
- **Workspace:** none, with the root's folder as `cwd`. So the session:
  - isn't listed under CHATS and doesn't count in the workspace's activity badges;
  - isn't covered by the workspace's permission rules;
  - gets no Coqo MCP tools.
- **Mode:** the mode that asks before every change, set first: Claude Code's `default` ("Manual"), when the agent has it and isn't in it. The user's default may be one that acts on its own (`auto`, `acceptEdits`).
  - `plan` mode was tried first, against the real Claude Code. In plan mode Claude Code runs the turn on a model of its own (Sonnet), even while the session's `model` option reads Haiku.
- **Settings:** the cheapest settings the agent offers, for this session only, through `sessions/setConfigOption`. They are chosen after the mode is set, because a mode can change the model:
  - a Haiku model, when the `model` option has one;
  - else the lowest `thought_level` value.
  
  Coqo saves nothing, so the user's chats and their next session keep their own settings.

A user's chat was never used. A draft in the middle of a conversation would interleave with that work and put the whole diff into its context.

**The prompt.** One text block:
- **The ask:**
  - a Conventional Commits message only: `type(scope): summary` in the imperative, at most 72 characters, then an optional body wrapped at 72 columns;
  - in the style of the repository's recent commits;
  - no preamble and no code fences;
  - "don't use any tools and don't change any files".
- **Recent commits:** the last 10 commit subjects (`git/log`), so the type and scope habits match.
- **Staged files:** `git/diffFiles` `HEAD` → index, listed with their change kind and line counts (at most 200).
- **The diff:** `git/diff` for each file, printed as `git diff` does.
  - At most 60 files are fetched. Each is capped at 8 000 characters, and the whole diff at 40 000.
  - A diff is cut at a line end and marked "cut for length", and the prompt says when files were left out.

**No tools.** Everything the agent needs is in the prompt, and it is asked not to use tools. In the Manual mode anything else is a permission request, and Coqo rejects every one (`reject_once`, else `reject_always`, else cancelled) and never remembers it.

**The reply.** The agent's message chunks of that turn replace the commit box's text. They are trimmed, and taken out of a code fence or quotes if they came in one. The user edits the message and presses Commit; nothing is committed until then.

**The button.** "Draft" sits beside Commit:
- **Ready:** raised, with the sparkles icon. The tooltip names the provider.
- **Drafting:** "Drafting…" in `sand`, with a loader icon. A click cancels: it closes the session, which cancels its turn, and the commit box keeps what it had.
- **Disabled:** in `text_subtle`. The tooltip gives the reason:
  - coqod isn't connected;
  - no agent is available (with the provider's own setup message);
  - nothing is staged.
- **Failure:** shown in `rose` under the buttons. A click dismisses it.
- **Command:** "Draft commit message" (`git.draft_message`, in the Git group of the command palette, ADR 0023) shows the Git pane and starts a draft.

## Consequences

- A draft costs one short agent turn on the user's own account. Against the real Claude Code a draft took about 4 seconds; most of its roughly 25 000 tokens are Claude Code's own system prompt.
- While a draft runs, its session shows in the agents overview. Its "finished" alert stands only from the turn's end to the close.
- An agent without modes or config options still drafts, with its defaults. Then the prompt and the rejected permissions are what keep it from acting.
- Very large changes are summarised from a partial diff, and the prompt says so. A smarter choice of what to send (by hunk, say) can come later.
- Other agents (Codex, a native provider) get the same flow once they are providers, with no change here.
