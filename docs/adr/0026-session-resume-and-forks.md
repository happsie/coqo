# 26. Session resume, ended sessions, new chats with context, forks and retry

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #67 (epic #52; PLAN §4.3)

## Context

Sessions were already durable when #67 started:

- transcripts and the agent's session ids are in SQLite;
- a turn cut off by a daemon kill ends as `interrupted`;
- the next prompt reattaches the agent's session with ACP `session/resume`, else `session/load`.

A real Claude Code check, with a new daemon and a new adapter process, confirmed it. The resumed session remembered a code word from before the restart (`a_real_agent_titles_and_resumes_after_a_restart` in `crates/acp-host/tests/real_agent.rs`).

One gap was left. When the resume failed, the manager quietly started a **fresh** agent session behind the old transcript. The user saw their whole chat, but the agent knew none of it. #67 also asks for forking from a message and retrying the last turn.

The questions:

- what to do when an agent can't resume;
- how to go on from a chat whose agent session is gone;
- how to fork;
- how to retry;
- how to reopen a closed chat.

## Decision

**An agent session that can't be resumed ends the Coqo session; it never restarts silently.** When a session that has turns needs its agent and:

- the provider can't resume at all, or
- the agent refuses the session: a new `ProviderError::SessionLost`, which acp-host returns when `session/resume` and `session/load` answer with an error. Claude's adapter answers "Resource not found".

then the manager:

- records an `ended {reason}` update;
- moves the state to `error` with "Can't resume: …";
- refuses this prompt and every later one with `SESSION_ENDED` (1110).

If the agent could not be **asked** (the process didn't start, or exited, or timed out: `Failed`), the prompt fails with `PROVIDER_ERROR` and the session is unchanged, so the next prompt tries again. A session without turns still starts afresh, since there is nothing to lose.

**Going on: `sessions/create` with `from: {session, through?}`.** The new session:

- records `started_from {session, title, through}`;
- takes the source's title (as a derived title, so the agent's own title can replace it);
- sends the source transcript as context with its **first** prompt.

The context is one text block before the user's own blocks. It holds the prompts and replies in full, tool calls by title, the newest 60 000 characters, and a line saying it is context. It is not recorded in the new transcript, which shows a "Continues “…”" notice instead.

The chat shows the ended state in place of the composer: "The agent can't resume this chat" and **Start a new chat with this context**. The composer's text goes along to the new chat.

**Fork = the same mechanism with `through`.** "Fork from here" on a prompt passes the seq just before that prompt's `user_message`, and puts the prompt in the new chat's composer, to edit and send. The palette's **Fork chat** forks the whole chat.

ACP's `session/fork` is not used:

- it is behind the `unstable_session_fork` feature of the ACP crates;
- it forks at the end of the agent's history, with no point to fork from;
- agents may not have it.

A native fork, when the session is forked at its end and the agent supports it, is follow-up work.

**Retry = `sessions/retry`.** It sends the last `user_message` again as a new turn, through the normal prompt path. The chat offers **Retry** on the notice of a failed or stopped last turn. The palette has "Retry the last prompt".

**Reopen = `sessions/reopen`.** It clears `closed` and records `reopened`. The next prompt resumes the agent's session, or ends the session as above.

CHATS gains a collapsed **History** list of the workspace's closed chats: the 10 newest, and only titled ones, so an empty chat left by a provider switch stays out. A click reopens a chat and shows it. A closed chat's tab offers **Reopen**, and the palette has "Reopen closed chat", which reopens the newest.

## Consequences

- No chat ever pairs a transcript with an agent that silently lost it. The price is an explicit step, "Start a new chat with this context", when an agent loses a session. A context replay costs tokens, up to about 15k for a long chat.
- The context is plain text, so it works with every provider. It loses tool output and thinking, which the agent can re-read or re-run.
- A forked or continued session is a normal session with its own agent session. Closing or ending the source doesn't touch it.
- `ProviderError::SessionLost` is the provider's way to say "gone for good". Providers other than acp-host fall back to the old rule: `Unsupported` ends the session, and anything else is retried.
- Follow-ups: a native ACP fork at the end of a session, once `session/fork` is stable; a summary for very long contexts in place of cutting.
