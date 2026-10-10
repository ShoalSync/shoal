# 0031: Static analysis and fuzzing

- Status: accepted
- Date: 2026-09-27

## Context

The monorepo's CI runs `cargo fmt`, `clippy -D warnings`, `cargo-deny` (advisories, licences,
bans and sources), a build on the MSRV and `cargo xtask check-deps`, and every crate forbids
unsafe code. `cargo machete` (unused dependencies) and `cargo semver-checks` on `proto` and
`sdk` are being added separately. The other repositories build and test only: nothing lints the
Android app's Kotlin, the infra and site Nix files, the shell scripts or any repository's
workflows.

A trial on the current code found:

- **Clippy's pedantic group:** 422 warnings, almost all documentation style
  (`missing_errors_doc`, `must_use_candidate`). Not worth enabling as a whole.
- **Restriction lints** that catch panics on bad input: 74 `unwrap`, 88 unchecked arithmetic and
  28 indexing sites. The indexing sites in `proto` and `store`, which handle peers' data, are all
  bounded today, but nothing stops a new one that is not.
- **Android Lint:** no errors, 14 minor warnings, never run in CI.

ADR 0024 promises that no input from any peer crashes a replica, corrupts its state or makes it
diverge. That promise is tested only by unit tests with inputs someone thought of.

## Decision

The checks below are adopted, in the order of the Phase 5g breakdown in the plan. Each lands in
its own pull request. Existing findings are fixed or recorded in a baseline, so no check starts
with a large cleanup, and every new finding fails CI.

### Stricter lints where peers' data is handled

- `proto`, `net` and `store` warn on `clippy::indexing_slicing`,
  `clippy::arithmetic_side_effects`, `clippy::unwrap_used` and `clippy::expect_used` in library
  code. CI's `-D warnings` makes them failures.
- An existing site that is correct is annotated `#[expect(lint, reason = "...")]` saying why it
  cannot panic, so the reason is reviewed with the code.
- Step 3 decided:
  - The lints are set at the top of each crate's `lib.rs` for builds that are not tests, so
    test code may still unwrap.
  - Most sites were fixed rather than annotated: saturating counters and lengths, `get` in
    place of indexing, checked frame lengths and deadlines. Annotated are postcard encoding
    in memory, shifts bounded by a loop, base32 and hex digits, and a chunk's shard directory.
  - Locks in `net` are taken through one helper that passes a poisoned lock's panic on, since
    the panic that poisoned it has already stopped the node (ADR 0030).
  - The clock offset from a peer's `Hello` could overflow on a clock the peer made up: a
    panic in a debug build, a wrapped value in a release one. It now saturates, and the
    doctor, which took its absolute value, handles any offset, with a test. A version
    vector whose hole ends at the largest counter overflowed the same way when its ranges were
    read; `meta` refuses such vectors before reading them, but `proto` no longer relies on
    that, with a test.

### Property tests

- `proto`: every wire message and signed envelope survives an encode and decode round trip, and
  decoding arbitrary bytes returns an error or a value, never a panic.
- `meta`: replicas that apply the same versions in any order reach the same state, the same
  winners and the same recycle bin (convergence); version vector comparison is a partial order.
- They use `proptest`, which the chunker already uses, and run with the normal tests.
- Step 4 decided:
  - `proto`: sync/1, blobs/1 and pair/1 messages survive framing; signed versions and
    certificates survive signing, encoding and verifying, and a version's signed body is its
    one encoding; pairing codes and IDs survive their text forms. Arbitrary bytes and text go
    through every decoder, verifier, pairing-code and path parser without a panic, and a path
    that is accepted has no empty, `.` or `..` component, no leading `/` and no backslash.
    Comparing vectors is reflexive, antisymmetric and transitive, and a merge is at least both
    sides.
  - `meta`: three replicas make random changes and sync in random pairs; applying every
    version they made, in order, reversed and shuffled, gives the same winners and conflict
    copies.
  - **The recycle bin is not the same everywhere.** A delete or a decision recycles the version
    it replaced on the replicas that stored that version (ADR 0027, ADR 0029); a replica that
    received the decision first never stores the version, so it has nothing to keep. The
    property checks instead that every recycled item is a version that was made; a deleted
    entry's version only while the entry is deleted; and a lost conflict under the ID derived
    from its signed form, never the winner or an open conflict.
  - **IPv6 scopes are not carried.** serde, and so postcard, encodes a socket address without
    an IPv6 address's scope and flow information, so a link-local address in a pairing code or
    `Complete.addrs` arrives without its interface. Carrying it is a wire change and is not made
    here; the property generates addresses as the wire carries them.

### Fuzzing

- A `fuzz/` crate with `cargo-fuzz` targets for what arrives from outside:
  - frame and message decoding for sync/1, blobs/1 and pair/1;
  - signed-envelope verification and `verify_authored`;
  - path validation, checking that an accepted path never leaves the folder;
  - pairing codes;
  - engine state files, which may be truncated or corrupted on disk.
- A scheduled job on `main` runs each target for ten minutes every night, on a pinned nightly
  toolchain used for nothing else. It does not run on pull requests, because three self-hosted
  runners on one machine serve every repository.
- A crash becomes a regression test in the crate it hit, and its input joins the corpus.

### Workflow checks

- `actionlint` and `zizmor` check `.github/workflows` in all four repositories. `zizmor`
  findings of medium severity or higher fail CI.
- Pinning actions by commit is Phase 5c step 5; this lands with or after it, so `zizmor` then
  keeps them pinned.

### Android

- Android Lint runs in CI (`lintDebug`). Errors fail; existing warnings go in
  `lint-baseline.xml`.
- `detekt`, with its formatting rules, checks the Kotlin code, starting from a baseline of
  existing findings.

### Smaller checks

- `shellcheck` on the shell scripts in every repository.
- `statix` and `deadnix` on the Nix files of the monorepo, infra and site, as flake checks, so
  `nix flake check` runs them locally and in CI.
- `typos` on code, comments and docs in every repository, with a shared list of project words.

### Not adopted

- **Deferred:** CodeQL, which overlaps clippy and `cargo-deny` for Rust today; revisit when the
  repositories are public, where it is free and adds a security dashboard. `cargo-mutants`, which
  measures how well tests catch changes; run by hand on `meta` and `engine` before 1.0, since it
  is too slow for the shared runner.
- **Not needed:** clippy's pedantic group as a whole (documentation-style noise), Miri and
  `cargo-geiger` (no unsafe code), `cargo-udeps` (`cargo machete` covers it without nightly),
  `cargo-audit` (`cargo-deny` checks the same advisories).

## Consequences

- A new unchecked index, overflow or `unwrap` on peers' data fails CI in the crates that parse
  it, and fuzzing looks for the cases reviews miss.
- The nightly fuzz job needs a nightly toolchain on the runner and about an hour of its time
  each night.
- Kotlin, Nix, shell and workflow files get checked for the first time; baselines keep the first
  pull requests small.
- `typos` needs a list of project words, and `zizmor` depends on actions being pinned first.
