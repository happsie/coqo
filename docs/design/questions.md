# Anchored questions

This is how a question or note that an agent pins to code shows up in Coqo (design board 2, PLAN §7.5, ADR 0016). The daemon side, meaning the store, the anchors and the `ask_question` / `await_answer` / `annotate` tools, is #73. This page covers the editor side.

The code lives in these places:
- `crates/coqo/src/questions/`: the store per workspace, the popover, the Questions pane and navigation.
- `crates/ui/src/editor/markers.rs`: markers and overlays on `CodeEditor`.
- `crates/coqo/src/chat/tab.rs`: the question card in the chat transcript.

![A waiting decision question open at its code, with the Questions pane](screenshots/questions-dark.png)

![The same in the light theme](screenshots/questions-light.png)

## What the user sees

1. **An agent asks.**
   - A diamond marker appears in the gutter of every file tab that shows the anchored lines.
   - Those lines get a tint.
   - The Questions pane counts the question.
   - The asking chat gets a card for it.
2. **The agent blocks on it** (`waiting`, from `ask_question` or `await_answer`).
   - The marker pulses, and the popover opens by itself in any file tab on screen that shows the code.
   - The popover never takes the keyboard: whatever the user is typing keeps the focus.
3. **The user opens it.** Any of these does it, and each one opens the file tab if needed, scrolls the anchored line near the top and puts the keyboard in the popover:
   - a click on the marker;
   - `F8` / `Shift F8`;
   - a click on a card in the Questions pane;
   - a click on the chat card.
4. **The user answers.**
   - The answer is an option, a typed reply, Yes/No, or OK.
   - Sending it calls `questions/answer`. Dismiss calls `questions/dismiss`.
   - The waiting agent gets the result, and the marker, the popover and the pane card go away.
   - The chat card changes to "Answered: …".

## Markers

- **Diamond:** a 9 px `marker-diamond` icon in the 20 px gap between the line numbers (and fold icons) and the code, on the first anchored line.
  - The board draws it before the line number. The editor has no hook into the line-number column, so it sits after the number.
  - It is clickable (`code-marker` / marker id).
- **Strip:** the editor's 3 px gutter-decoration strip, in the marker colour, covering every anchored line.
- **Tint:** a band behind the anchored lines, 13 % of the marker colour (20 % while waiting), from the gutter to 10 px before the right edge, with the right corners rounded 8 px.
- **Colours:**
  - `sand` for a question that needs the user (decision, clarification, confirmation, info);
  - `blue` for an info note;
  - `rose` for a warning note.
- **Waiting:** while an agent is blocked on the question, the diamond pulses between 35 % and 100 % opacity, and the tint is stronger.
- **Edits:** markers track edits.
  - The ranges are the daemon's resolved ones (`questions/list` after `buffer/open`, then `questions/changes`). They are used as they are when the resolution matches the open buffer's revision; otherwise the editor's own tracked range is kept.
  - Typing above a question moves its marker immediately, and the daemon's next resolution agrees.
- **Orphaned anchors** (the code is gone) have no marker. The popover then opens at the top of the editor and shows the original snippet.

## Popover

The popover is a floating card over the code, 620 px wide at most. It is placed by these rules:

- It goes below the anchored lines with an 8 px gap, and a `surface` caret at 40 px from its left edge points up at the lines.
- If it doesn't fit below but fits above, it goes above with the caret pointing down.
- If it fits on neither side, it goes on the roomier side and its content scrolls. It never covers the lines it asks about.

The board draws the popover inline, with the code pushed apart around it (a view zone). gpui-component's editor has no view zones, so Coqo floats it and scrolls the anchor near the top when it is opened instead. Follow-up: #228.

What the popover contains, top to bottom:

- **Header:**
  - the kind badge, upper case on the kind colour with `panel` text (DECISION `sand`, CLARIFICATION `blue`, CONFIRM `sage`, INFO `lavender`, NOTE `blue`, WARNING `rose`);
  - "provider · how long ago";
  - while the agent waits, "Blocking the agent" with a pulsing `sand` dot on the right.
- **Body:** the first line as a 15 px semibold title, then the rest as markdown.
- **Snippet:** when the anchor is orphaned or fuzzy, a `panel` box with a `sand` alert icon and "Code moved: it isn't here any more…" (orphaned) or "Code moved: it changed since it was asked…" (fuzzy), then the original lines in mono. Extra anchors are listed as "Also anchored: path:line".
- **Answer controls:**
  - *Decision:* option cards lettered A, B, C, with a label and description; the selected card gets a `sand` border. Below them, "Or reply in your own words".
  - *Clarification:* a reply field.
  - *Confirmation:* a Yes / No row.
  - *Info and notes:* no controls.
- **Footer:**
  - on the left: **Open in chat**, **Dismiss**, and the hint "F8 next question";
  - on the right: **Later**, and the primary button (**Send answer**, **OK** for info, **Dismiss** for a note). The primary button is disabled until there is an answer.

## Keyboard

| Key | Where | Does |
| --- | --- | --- |
| `F8` / `Shift F8` | anywhere in the window | Opens the next / previous open question. The order is file path, then line, across files, wrapping around. It starts from the open popover, else the cursor in the active file tab. |
| `Enter` | in the popover | Sends the answer. In the reply field, `Shift Enter` is a new line. |
| `Esc` | in the popover | Closes it without answering. The keyboard goes back to the code. |

## Questions pane

The Questions pane is a sidebar pane, between Files and Git, showing the current workspace's questions and notes.

- **Tabs:** **Open N** and **Answered N**. The answered history is behind the second tab.
- **Open list:** grouped under file headers (mono, `text_subtle`), in F8 order.
- **Cards:**
  - the kind label in the kind colour, with "· BLOCKING" while an agent waits;
  - the age;
  - the title;
  - the location.
  - A waiting question's card has a `sand` border. A confirmation's card has inline Yes / No buttons.
  - A click on a card opens it in the editor.
- **Answered cards:** "Answered: …" or "Dismissed".

The board shows these cards in a right-hand column of the editor. Coqo puts them in a sidebar pane, so the editor keeps its width and the list works with any tab.

## Chat card

The chat card shows a `SessionUpdate::Question` in the asking chat's transcript. A later state of the same question replaces the card in place.

- **Contents:** a diamond, the kind label, "Asked about src/cart.rs:42 · Open in editor" ("Note on" for notes), the title, and the state ("Waiting for your answer", "Answered: …" or "Dismissed").
- **Click:** opens the popover.

## For other parts of the UI

`WorkspaceQuestions` (one per workspace, `questions::questions_for`) holds:
- the live list;
- `open_count`, `waiting_count` and `note_count` for badges and alerts (#76);
- `QuestionsEvent::Waiting(id)` when an agent starts waiting.

To regenerate the screenshots, run `COQO_SHOT_DIR=<dir> cargo test -p coqo -- --ignored questions_screenshots` (offscreen, needs a GPU) and downscale the 2× PNGs to 1440×900 as `questions-<appearance>.png`.
