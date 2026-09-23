# 0013: Zola for the website, mdBook for documentation

- Status: accepted
- Date: 2026-09-23

## Context

`tidefell/site` hosts the website, blog and documentation. Documentation is written in the
monorepo next to the code (plan: "docs live next to code").

## Decision

- The monorepo's `docs/` is an mdBook. Each release builds it and attaches the output.
- `tidefell/site` is a Zola site for the landing pages and blog. Its build fetches the mdBook
  output of the pinned release and serves it under `/docs/<version>/`, with `/docs/latest/`
  pointing at the pinned version.
- Both generators are single binaries in nixpkgs; neither repository needs Node.

## Consequences

- Documentation for any release is reproducible from its tag.
- The site's pin (`TIDEFELL_VERSION`) decides which docs are live, like every other
  downstream pin.
