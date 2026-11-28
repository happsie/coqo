# 1. Record architecture decisions

- **Status:** Accepted
- **Date:** 2026-11-26

## Context

Coqo is built by several agents and humans working in parallel. Decisions about architecture, dependencies and protocols need to be written down where everyone (human or agent) can find them, with the reasoning, so they aren't re-argued or silently reversed. `docs/PLAN.md` holds the overall plan, but it describes the current design rather than why it changed.

## Decision

We record architecturally significant decisions as Architecture Decision Records (ADRs), following Michael Nygard's format.

- ADRs live in `docs/adr/` and are named `NNNN-<kebab-case-topic>.md`, numbered in order and never reused.
- Each ADR has a title, **Status** (Proposed, Accepted, Deprecated, or Superseded by ADR-NNNN), **Date**, and the sections **Context**, **Decision** and **Consequences**.
- Spikes (`type:spike` issues) end with an ADR that includes their findings, measurements and a clear **go / fallback** decision. Prototype code goes in `docs/spikes/<id>/`.
- When an implementation has to differ from `docs/PLAN.md`, the same PR adds or updates an ADR and updates PLAN.md.
- Accepted ADRs are not rewritten. To change a decision, write a new ADR that supersedes the old one and update the old one's status.

## Consequences

- Decisions and their reasons are visible in the repo and reviewed in PRs like code.
- Agents picking up an issue can read the relevant ADRs before starting, as `CLAUDE.md` requires.
- There is a small overhead per significant decision, which we accept.
