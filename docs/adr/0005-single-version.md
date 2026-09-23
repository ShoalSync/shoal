# 0005: One version for all monorepo artifacts

- Status: accepted
- Date: 2026-09-23

## Context

The monorepo produces crates, binaries, an AAR, schemas and docs. Independent versions would
force downstream repositories to track compatibility matrices.

## Decision

All workspace crates share `workspace.package.version`. A release is a `vX.Y.Z` tag, and every
artifact built from it carries `X.Y.Z`. Protocol versions are expressed separately, in ALPN
strings, and change only on breaking wire changes.

## Consequences

- Each downstream repository pins one string.
- Crates may be re-released unchanged; that is acceptable while nothing is published to
  crates.io.
