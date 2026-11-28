# 29. The agent's own question tools in the chat

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #339. Builds on [ADR 0015](0015-agent-state-and-status.md) (states) and [ADR 0016](0016-anchored-questions.md) (Coqo's `ask_question`). [ADR 0030](0030-skill-questions-in-code.md) (spike S9, #340) reads the same adapter code and decides where these questions are pinned.

## Context

Agents ask the user things with their own tools, not only with Coqo's `ask_question`:

- Claude Code's `AskUserQuestion`, which skills use too;
- plan approval (`ExitPlanMode`);
- an MCP server's form (an elicitation);
- any ACP permission request with options.

Before #339, Coqo showed permission requests but nothing else, so an agent's question could stall or fail without the user seeing it. The issue asks for every such question to work in the chat:

- a card with single and multi-select, an "Other" answer, previews and full keyboard use;
- the answer goes back through the request that asked, in the shape the agent expects;
- skipping is a clean decline, never a hang;
- the session shows as waiting;
- the question survives a UI reconnect and a daemon restart.

## What we tested

The real adapter: claude-agent-acp 0.113.0 (the pinned `ADAPTER_VERSION`) on the user's own, unmodified Claude Code. We ran 4 prompts in all, through the ignored tests in `crates/acp-host/tests/real_agent.rs`, in a temporary workspace. Nothing under `~/.claude` was read. We also read the adapter's code where it was installed.

| Run | What happened |
|---|---|
| 1–2. Ask a question with your question tool (Coqo as it was) | Claude said it had no `AskUserQuestion` or other question tool. The adapter puts `AskUserQuestion` in `disallowedTools` when the client does not advertise `clientCapabilities.elicitation.form`, and Coqo didn't. Nothing reached the user. |
| 3. The same, with form elicitation advertised (`a_real_agent_asks_with_ask_user_question`) | Claude asked two questions in one `elicitation/create` form: a single-select ("Which colour do you like?") and a multi-select (pets), each with an "Other" box. The session went to `needs_input` with "Waiting for your answer: Which colour do you like?". We answered Red, then Cat and Axolotl, and Claude replied "Colour: Red; Pets: Cat, Axolotl". |
| 4. Plan mode, then "write hello.txt" (`a_real_agent_asks_to_approve_a_plan`) | `ExitPlanMode` came as `session/request_permission` titled "Approve Plan", kind `switch_mode`, with the plan as text content and `rawInput.plan`. Its options were `exit-plan-default` "Yes, manually approve edits" (`allow_once`), `exit-plan-auto` and `exit-plan-bypass` (`allow_always`), and `reject` "No, keep planning" (`reject_once`). We rejected it: the turn ended with no text, and no `hello.txt` was written. |

**How the adapter shapes `AskUserQuestion`** (`dist/elicitation.js`; ADR 0030 has the full trace):

- **One form per tool call.** It is scoped to the session and the tool call's id. Claude Code's own confirmations come with no tool call.
- **A field per question, `question_<n>`.**
  - A single-select question is a `string` with `oneOf`; a multi-select one is an `array` with `items.anyOf`.
  - The option label is both the value (`const`) and the `title`. The description rides on the option, and the preview is under `_meta["_claude/askUserQuestionOption"].preview`.
- **An "Other" box per question,** `question_<n>_custom`.
- **Where the text goes.** A lone question is in the request's `message` and its header in the field's `title`. With several, each field's `description` holds its question.
- **The answer.** The adapter folds the response into the tool's own answer, keyed by the exact question text, with the label as the value. `decline` tells the model the user skipped; `cancel` aborts the tool use. A client error is answered "Could not present the question to the user".

## Decision

1. **Advertise form elicitation, and only forms.** `initialize` sends `elicitation: { form: {} }`. This brings `AskUserQuestion` back, so the adapter no longer disables it for Coqo sessions. URL-mode elicitation is not advertised: Coqo would have to open a page for the agent, and nothing needs it yet. Coqo never removes or replaces a tool in the user's Claude Code.
2. **One provider-neutral shape.** A question is a `PermissionRequest` with a non-empty `questions` list: `AgentQuestion { id, header, question, kind (single | multi | text | number), options [{ value, label, description, preview }], other, required, anchor }`.
   - It rides the existing permission pipeline: `permission_requested`, `sessions/respondPermission`, the fold, pending requests on reconnect and alerts. No new route or topic was needed.
   - New outcomes: `answered { answers: [{ id, picks, text }] }` and `declined`. A request without questions takes neither.
   - `anchor` is an optional hint (`path`, `line`, `end_line`) for the pinning that ADR 0030 decides (#344). The card does not use it yet.
3. **The ACP host maps forms both ways** (`acp-host/src/elicit.rs`).
   - **Fields become questions.** `oneOf` / `enum` is single, an `anyOf` array is multi, a boolean is a Yes/No single, a plain string is text, and a number or integer is number. A `<key>_custom` string folds into its question as "Other".
   - **Nothing is rewritten.** Question text, headers and labels pass through exactly as sent: skills parse markers in them (gstack's `<gstack-qid:…>` and `(recommended)`, ADR 0030).
   - **Answers go back under the agent's own keys.** A pick is the option's own value, and typed text goes in the companion field (or the field itself for a text question). A number is parsed, a boolean is `true` or `false`.
   - **The other outcomes.** `declined` is `decline`, and anything else (a stopped turn, a closed session) is `cancel`.
   - **A form Coqo can't show is an error** (`invalid_params`), never a silent hang: a required field of an unsupported type, or no field at all. A form for no session is cancelled.
   - **MCP elicitations** take the same path.
4. **The manager checks answers** before they reach the agent: known ids, no duplicates, picks among the options, at most one pick for a single-select, text only where the question takes it, and required questions answered. A bad answer is `invalid_params` and the question keeps waiting. Workspace rules never answer a question.
5. **States** (extends ADR 0015):
   - A waiting question puts the session in `needs_input`, with "Waiting for your answer: <first line of the question>" as the reason. A waiting permission request wins: `needs_permission` holds until it is answered, then the state falls back to `needs_input` while questions still wait, else `working`.
   - The agent's own `needs_input` reports don't overwrite a waiting question's reason.
   - The overview, the tab badge, the CHATS dot and alerts all follow the state.
6. **Withdrawal and restarts.**
   - When the agent takes its request back (ACP `$/cancel_request`), the question resolves as `cancelled` and the state settles. Stopping the turn cancels it the same way.
   - A daemon restart ends the turn as `interrupted` (ADR 0026), and that now includes a session left in `needs_input` mid-turn. Before, only `working` and `needs_permission` counted. An agent's own `needs_input` after its turn ended is left alone.
   - A cancelled question with nothing said after it stays answerable in the chat. **Send as a message** sends the answers as the next prompt, which resumes the session. So a question asked before a restart is not lost, even though the adapter's request is.
7. **Plan approval stays a permission card.** The card shows the plan's text as markdown, with the adapter's options unchanged.
8. **The fallback, if an agent can't carry its questions:** its question tool stays disabled by its adapter, and the agent asks in prose. Coqo never removes tools or wraps the agent to get around that.

## Consequences

- **Skills and plugins that use `AskUserQuestion` work in Coqo as they do in the terminal,** and their own hooks on the tool still run first, inside Claude Code.
- **Tests.**
  - `fake_agent.rs`: the fake ACP agent's `ask` command sends an adapter-shaped form; the test answers it, declines it and cancels it.
  - `agents/tests/sessions.rs`: the checks on answers, withdrawal, and the restart.
  - `chat/tests.rs`: the card by keyboard and by click, validation, skip, "Send as a message", a UI restart while waiting, and plan approval.
  - The two real-adapter runs stay as ignored tests.
- **#266 still holds.** `PromptInput` gained edge keys: `Up` on the field's first line and `Down` on its last are reported, so the card's arrows can leave the "Other" field while digits typed in it stay text. It now also consumes the `Enter` it submits with, which used to be typed as a trailing newline.
- **Open.**
  - URL-mode elicitations (a sign-in page an MCP server asks the user to open) are refused until a provider needs them.
  - The anchor hint is set by no provider yet; ADR 0030's resolver fills it (#344).
