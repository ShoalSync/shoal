# 0044: Licence: MIT OR Apache-2.0

- Status: accepted
- Date: 2026-10-24

## Context

Tidefell was AGPL-3.0-only, apart from its `proto` and `sdk` crates, which were MIT OR
Apache-2.0 so other clients could interoperate. Contributors signed a CLA so the project could
relicense. Shoal goes public (ADR 0043) under the licence most of the Rust ecosystem uses, with
no CLA, so contributing is as simple as it can be.

Every commit in the four repositories is the owner's, the owner's agents' or Renovate's
dependency bumps, so the owner can relicense all of it.

## Decision

- **Everything is `MIT OR Apache-2.0`**: each crate (`LICENSE-MIT` and `LICENSE-APACHE` at the
  root, linked into each crate's directory), the Android app, the infrastructure flake and the
  site's code. This supersedes the AGPL split and the CLA.
- **No CLA.** Contributions are licensed inbound as outbound, by the standard clause in the README
  and CONTRIBUTING.md.
- **Earlier versions too.** The README states that all earlier versions, including those under the
  name Tidefell, are also available under MIT OR Apache-2.0. Old commits keep their AGPL `LICENSE`
  file, but the statement covers them.
- **The name is protected by a trademark policy, not the licence** (TRADEMARKS.md): forks rename.
- `cargo xtask check-deps` checks that each workspace crate is MIT OR Apache-2.0, instead of the
  AGPL boundary. Third-party dependencies stay permissive-only (`deny.toml`).

## Consequences

- Shoal can be embedded, forked and sold without passing changes back. That is accepted in
  exchange for adoption and simple contribution.
- Binary releases must carry their dependencies' notices: a third-party licences file in the
  release assets, the MSI and the archives, made with `cargo about`. That comes with the move to
  public CI.
