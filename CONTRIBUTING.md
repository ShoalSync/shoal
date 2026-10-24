# Contributing to Shoal

Thanks for your interest in Shoal. This document covers how changes get in.

## Licence of contributions

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed under MIT OR
Apache-2.0, without any additional terms or conditions.

## Workflow

Shoal uses trunk-based development:

- `main` is always releasable. Nobody pushes to it directly.
- Work on short-lived branches and open a pull request against `main` early.
- Keep pull requests small and focused; split large changes behind feature flags.
- Rebase on `main` rather than merging `main` into your branch.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add rarest-first piece selection
fix(store): release eviction lock on error
docs: describe recycle bin retention
chore: bump iroh to 0.x
```

Common types: `feat`, `fix`, `docs`, `chore`, `refactor`, `test`, `perf`, `ci`.
Mark breaking changes with `!` (for example `feat(proto)!:`) and a `BREAKING CHANGE:` footer.

## CI gates

A pull request can merge only when CI is green. `nix flake check` runs formatting, clippy, the
tests, the crate boundaries and the MSRV build the way CI does, reusing compiled dependencies
until `Cargo.lock` changes. Without Nix, run the same checks with cargo:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny --all-features check
cargo machete
```

CI also runs `cargo semver-checks` on `proto` and `sdk` against the last release. Before 1.0 it
only reports breaking changes in the job summary (ADR 0026); from 1.0 they fail the check.

## Reporting security issues

Please do not open public issues for security vulnerabilities. Contact the maintainers
privately instead.
