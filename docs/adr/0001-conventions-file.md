# 0001: CONVENTIONS.md in every repository

- Status: accepted
- Date: 2026-09-23

## Context

Four repositories are developed independently by different contributors and tools. Each needs a
single, predictable place that states what the repository owns, what it consumes, and how
changes flow between repositories.

## Decision

Every repository has a `CONVENTIONS.md` at its root with the same sections: *Owns*, *Consumes*,
*Change order*, *Local rules*, *Read first*. It links into the monorepo's `docs/` rather than
duplicating content.

## Consequences

- Anyone starting work in any repository has one file to read first.
- Cross-repo facts live once in the monorepo; `CONVENTIONS.md` files stay short and link.
