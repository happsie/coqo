---
description: Walk through a change in Coqo, step by step (publish_walkthrough)
argument-hint: "[what to cover: a commit range, branch or PR; default: uncommitted changes]"
---

Write a code walkthrough of this change and publish it to Coqo with `publish_walkthrough`, following the `coqo-walkthrough` skill: every hunk covered by a step or listed as trivial, steps in reading order, acceptance criteria referenced.

What to cover: $ARGUMENTS

If that is empty, cover the uncommitted changes in this workspace (`worktree` scope against `HEAD`), or, if there are none, the commits of this branch that are not on its base branch.
