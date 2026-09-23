# Conventions

Read this before changing anything in this repository.

## Owns

This monorepo is the **source of truth** for Tidefell. It owns:

- All Rust code (`crates/`, `xtask/`).
- Wire protocol specs and golden test vectors (`specs/`).
- Glossary, ADRs, repository map and user/admin docs (`docs/`).
- Every artifact that other repositories consume: the `org.tidefell:tidefell-ffi` AAR, release
  binaries, the Nix flake, config JSON Schema, OpenAPI and built docs.

## Consumes

Nothing from the other Tidefell repositories.

## Change order

Monorepo change → release tag → downstream pin bump. See [docs/repos.md](docs/repos.md).
A change that affects a downstream repository is tracked by one issue here, labelled
`repo:android`, `repo:infra` or `repo:site`.

## Local rules

- **Vocabulary:** use the terms in [docs/glossary.md](docs/glossary.md). Add new terms there.
- **Crate boundaries** (checked by `cargo xtask check-deps`):
  - `proto` and `sdk` are MIT OR Apache-2.0 and never depend on an AGPL crate.
  - `engine` never depends on `fs-fuse` or `web`.
  - `ffi` depends only on `sdk` and `engine` among workspace crates.
- **Wire changes:** update `specs/<alpn>.md` and `specs/vectors/` in the same pull request.
  A breaking change bumps the ALPN version. See [ADR 0006](docs/adr/0006-wire-encoding.md).
- **On-disk formats** (`store`, `meta`) carry an explicit format version; changing them needs a
  migration or a version bump with a test.
- **Generated code** (bindings, schemas, OpenAPI) is never edited by hand.
- **Decisions** that affect more than one crate or repository get an ADR in `docs/adr/`.
- **Checks before pushing:** `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
  `cargo xtask check-deps`.
- **MSRV** is 1.91; CI builds on it. Do not use newer language or library features.
- **Commits:** Conventional Commits; see [CONTRIBUTING.md](CONTRIBUTING.md).

## Read first

1. [docs/plan.md](docs/plan.md)
2. [docs/glossary.md](docs/glossary.md)
3. [docs/adr/](docs/adr/)
4. [specs/](specs/)
