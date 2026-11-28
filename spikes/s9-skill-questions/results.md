## 1. Structured questions: anchor sources

38 questions: 24 belong in the code, 8 in the chat only, 6 either.

Each source on its own (plans excluded, as in the cascade):

| Source | Fired | Right place (precision) | Recall over the 24 that belong in the code |
|---|---|---|---|
| explicit path:line | 7 | 100 % (7/7) | 29 % (7/24) |
| option preview | 1 | 100 % (1/1) | 4 % (1/24) |
| files of the turn | 19 | 68 % (13/19) | 50 % (12/24) |
| selection | 3 | 100 % (3/3) | 12 % (3/24) |
| selection, ungated (any selection) | 6 | 50 % (3/6) | |

Files of the turn by the lowest score it accepts (terms: an identifier 3, a word 1, a line the turn edited 1):

| Lowest score | Fired | Precision | Recall |
|---|---|---|---|
| 2 | 24 | 75 % (18/24) | 67 % (16/24) |
| 3 | 19 | 68 % (13/19) | 50 % (12/24) |
| 4 | 14 | 71 % (10/14) | 38 % (9/24) |
| 5 | 10 | 70 % (7/10) | 29 % (7/24) |
| 6 | 7 | 86 % (6/7) | 25 % (6/24) |
| 7 | 6 | 100 % (6/6) | 25 % (6/24) |

The cascade (first source that finds a place; else the chat with "Pin to code"):

| Order | Right | Pinned to the wrong place | Left in the chat that belonged in the code |
|---|---|---|---|
| explicit → preview → turn files → selection (gated) | 82 % (31/38) | 8 % (3/38) | 11 % (4/38) |
| explicit → preview → selection (gated) → turn files | 82 % (31/38) | 8 % (3/38) | 11 % (4/38) |
| explicit → turn files → selection, ungated | 71 % (27/38) | 16 % (6/38) | 13 % (5/38) |
| explicit and selection (gated) only | 66 % (25/38) | 0 % (0/38) | 34 % (13/38) |
| explicit only | 55 % (21/38) | 0 % (0/38) | 45 % (17/38) |

Per question (first order):

| Case | Skill | Shown at | |
|---|---|---|---|
| gstack-review-1 | gstack /review (Step 5c ASK item) | src/api/orders.ts:48-50 (explicit path:line) | ok |
| gstack-review-2 | gstack /review (finding line in the question) | src/api/orders.ts:42-42 (explicit path:line) | ok |
| gstack-review-3 | gstack /review (batched ASK items) | src/api/auth.ts:30-30 (explicit path:line) | ok |
| gstack-review-4 | gstack /review (ambiguous basename) | src/index.ts:2-2 (explicit path:line) | ok |
| gstack-review-5 | gstack /review (line past the end of the file) | src/cart/cart.ts:23-24 (files of the turn) | ok |
| gstack-ship-1 | gstack /ship (not about lines; numbers that look like refs) | chat | ok |
| gstack-review-greptile | gstack /review (bot comment triage) | src/cart/discounts.ts:25-25 (explicit path:line) | ok |
| code-review-url | code-review plugin (GitHub blob link) | src/api/auth.ts:16-18 (explicit path:line) | ok |
| feature-dev-1 | feature-dev (Phase 3 clarifying) | src/cart/cart.ts:33-35 (files of the turn) | ok |
| feature-dev-2 | feature-dev (Phase 3 clarifying) | chat | **wrong** |
| feature-dev-3 | feature-dev (Phase 3 clarifying) | src/config.ts:5-6 (files of the turn) | ok |
| feature-dev-4 | feature-dev (Phase 3 clarifying) | chat | **wrong** |
| feature-dev-5 | feature-dev (Phase 3 clarifying) | chat | **wrong** |
| feature-dev-6a | feature-dev (Phase 3, multi-question form, question 1) | src/config.ts:12-13 (files of the turn) | ok |
| feature-dev-6b | feature-dev (Phase 3, multi-question form, question 2) | chat | **wrong** |
| feature-dev-7 | feature-dev (Phase 4 approach choice) | src/cart/cart.ts:45-47 (files of the turn) | **wrong** |
| feature-dev-8 | feature-dev (Phase 6 review findings) | src/api/orders.ts:49-49 (explicit path:line) | ok |
| brainstorm-1 | superpowers brainstorming (open question) | src/api/orders.ts:49-51 (files of the turn) | **wrong** |
| brainstorm-2 | superpowers brainstorming (lay out the approaches) | src/cart/discounts.ts:25-27 (files of the turn) | ok |
| brainstorm-3 | superpowers brainstorming (Socratic follow-up) | chat | ok |
| brainstorm-4 | superpowers brainstorming (visual companion offer) | chat | ok |
| executing-plans-1 | superpowers executing-plans (stop and ask: destructive) | src/db/migrations/0003_orders.sql:1-13 (files of the turn) | ok |
| executing-plans-2 | superpowers executing-plans (stop and ask: push) | chat | ok |
| executing-plans-3 | superpowers executing-plans (stop and ask: broken plan) | src/api/orders.ts:42-42 (files of the turn) | ok |
| mid-task-1 | mid-task question after an edit | src/cart/cart.ts:14-16 (files of the turn) | **wrong** |
| mid-task-2 | mid-task question after a failing test | tests/cart.test.ts:11-13 (files of the turn) | ok |
| mid-task-3 | mid-task question about a value | src/config.ts:8-10 (files of the turn) | ok |
| selection-1 | question about the selection | src/cart/discounts.ts:21-23 (selection) | ok |
| selection-2 | question about the selection | src/cart/cart.ts:33-36 (selection) | ok |
| selection-3 | stale selection, unrelated question | chat | ok |
| selection-4 | stale selection, project question | chat | ok |
| selection-5 | question about the selection, no tool calls | src/api/orders.ts:19-33 (selection) | ok |
| plan-1 | plan approval (ExitPlanMode), several files | chat | ok |
| plan-2 | plan approval (ExitPlanMode), one location | chat | ok |
| preview-1 | options with code previews (current code) | src/api/orders.ts:19-20 (option preview) | ok |
| preview-2 | options with code previews (new code only) | chat | ok |
| mcp-elicitation-1 | an MCP server's elicitation (GitHub) | chat | ok |
| confirm-1 | Claude Code's own confirmation (confirm_ id) | chat | ok |

## 2. Findings in text: review outputs

`real-coqo-*` are real outputs of the skills in a temporary Coqo session; the others are written in each skill's published format.

| Output | Findings | Detected right | Wrong (false positive) | Missed |
|---|---|---|---|---|
| code-review-plugin.md | 2 | 2 | 0  | 0  |
| feature-dev-review.md | 3 | 3 | 0  | 0  |
| gstack-review.md | 4 | 4 | 0  | 0  |
| pr-review-toolkit.md | 5 | 5 | 0  | 0  |
| real-coqo-brainstorming.md | 5 | 5 | 0  | 0  |
| real-coqo-requesting-code-review.md | 10 | 9 | 0  | 1 shop/orders.py:32 |
| superpowers-code-reviewer.md | 6 | 6 | 0  | 0  |
| **all** | 35 | 34 | 0 | 1 |

Without the workspace check (syntax only: a path with an extension or a directory, outside code blocks): 36 shown, 6 % (2/36) wrong (duplicates of one location count again).

## 3. Plain-text questions at the end of a turn (samples)

| Ending | Skill | Expected | Got | |
|---|---|---|---|---|
| I found three issues, the worst being the SQL in listOrders. Do you wa… | feature-dev Phase 6 | offer | offer | ok |
| If it's mostly for people checking on an order, I'd lead with status a… | superpowers brainstorming | other | other | ok |
| Today a second coupon replaces the first one without a word. Should a … | superpowers brainstorming | code | code at src/cart/cart.ts:33 | ok |
| That's how I understand the gift card part. What's wrong or missing?… | superpowers brainstorming (play back) | other | other | ok |
| D2 — cancelOrder deletes without an existence check (orders.ts:42). Re… | gstack prose fallback (D-brief) | code | code at src/api/orders.ts:42 | ok |
| Before I design this, a few questions: 1. Should the volume discount t… | feature-dev Phase 3 (question list) | code | code at src/api/orders.ts:12 | ok |
| The tests pass now. Want me to open a PR?… | free-form | offer | offer | ok |
| Let me know if you'd like the backoff too?… | free-form | offer | offer | ok |
| I set the session TTL to 24 hours to match the old service. Is 24 hour… | free-form | code | code at src/config.ts:8 | ok |
| Which CI provider does your team use?… | free-form | other | other | ok |
| Done: the migration copies legacy_orders and drops it. Tests pass.… | free-form | none | none | ok |
| The migration drops legacy_orders, which can't be undone. Run 0003_ord… | superpowers executing-plans (stop and ask) | code | code at src/db/migrations/0003_orders.sql:1 | ok |
| Do your customers expect prices with or without VAT?… | free-form | other | other | ok |
| Should I go ahead and implement the plan?… | free-form | offer | offer | ok |
| `login` counts attempts but never resets them. Should the lockout last… | free-form | code | code at src/api/auth.ts:23 | ok |
| Is this for the web shop only, or for the mobile app as well?… | free-form | other | other | ok |
| (real-coqo-brainstorming.md)… | superpowers brainstorming, real run in Coqo (play back) | other | none | **wrong** |
| (real-coqo-requesting-code-review.md)… | superpowers requesting-code-review, real run in Coqo | code | none | **wrong** |

Right: 89 % (16/18). "Names code": precision 100 % (6/6), recall 86 % (6/7).
