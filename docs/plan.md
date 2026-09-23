# Implementation plan

This document describes how the Tidefell repositories fit together, how they stay
maintainable, and the phased delivery plan. It is the canonical reference for cross-repo work.

## Core rule

**The monorepo (`tidefell/tidefell`) is the only producer of shared definitions. The other
repositories are consumers that pin a single released version and never hand-copy anything.**

Sharing happens through released artifacts with generated bindings, not through shared source,
submodules or copied files. The contract between repositories is enforced by builds, not by
contributors remembering it.

## Repositories

| Repository | Owns | Consumes |
| --- | --- | --- |
| `tidefell/tidefell` | All Rust code, wire specs, test vectors, glossary, ADRs, user and admin docs | Nothing from the other repos |
| `tidefell/android` | Kotlin + Compose app, foreground service, `DocumentsProvider` | `org.tidefell:tidefell-ffi` AAR |
| `tidefell/infra` | Nix deployment of relay, DNS, hosted web peer, full-replica storage | Release binaries, config JSON Schema |
| `tidefell/site` | Website, blog, docs hosting | Built mdBook and specs from a release |

## How the repositories reference each other

```
tidefell/tidefell (source of truth)
├─ crates/*   code
├─ specs/     wire protocol per ALPN + test vectors
├─ docs/      mdBook, ADRs, glossary, repo map
└─ release vX.Y.Z
   ├─ tidefell-ffi AAR (generated Kotlin inside)  → android
   ├─ tidefelld / web binaries (Nix flake outputs) → infra
   ├─ tidefelld config JSON Schema                 → infra (validated in CI)
   ├─ OpenAPI + .proto for `api`                   → external clients
   └─ built mdBook + specs                         → site
```

| Edge | Artifact | How the consumer pins it |
| --- | --- | --- |
| monorepo → android | `org.tidefell:tidefell-ffi` AAR with uniffi-generated Kotlin, published by monorepo release CI to GitHub Packages Maven | `gradle/libs.versions.toml`: `tidefell = "X.Y.Z"`. A `tidefellFfiLocalPath` Gradle property switches to a composite build against a local monorepo checkout for FFI development. |
| monorepo → infra | Monorepo `flake.nix` exposes `packages.tidefelld`, `packages.tidefell-web`, and a NixOS module | Infra `flake.nix` input pinned to the release tag; `flake.lock` records the exact revision. Infra CI validates all configs against the schema from the pinned version. Relay and DNS use upstream iroh packages, pinned the same way. |
| monorepo → site | Built mdBook tarball + `specs/*.md` attached to the release | Site pins the tag; site CI fetches and embeds. Docs live next to code so they change in the same pull request. |
| downstream → monorepo | Nothing | A downstream need becomes a monorepo issue first, then a release, then a pin bump. |

Rejected alternatives: git submodules for code; hand-written Kotlin copies of `proto` types;
Android talking gRPC to a separate daemon (it runs the full peer via FFI; one path, not two).

## Maintainability

1. **Machine-enforced crate boundaries.** `cargo xtask check-deps` reads `cargo metadata` and
   fails CI when:
   - `proto` or `sdk` depends (transitively) on an AGPL crate;
   - `engine` depends on `fs-fuse` or `web`;
   - `ffi` depends on any workspace crate other than `sdk` and `engine`.

   `cargo-deny` enforces licenses and advisories for third-party dependencies.
2. **One public surface.** `sdk` is the only API for third parties. `ffi` exposes `sdk` types plus
   engine lifecycle and nothing else. `cargo-semver-checks` runs on `proto` and `sdk`.
3. **Spec-first wire changes.** A pull request that changes a wire message updates
   `specs/<alpn>.md` in the same pull request. `specs/vectors/*.json` holds golden encodings;
   `proto` tests decode them, and Android later runs the same vectors through the FFI. A test
   asserts the ALPN constants match the spec files.
4. **One version for the whole workspace.** `workspace.package.version`, tagged `vX.Y.Z`. Every
   artifact carries that version; downstream repos pin exactly one string. Protocol ALPN versions
   are independent of release versions.
5. **Generated, never hand-written, at every boundary:** Kotlin via uniffi, OpenAPI via `utoipa`,
   config schema via `schemars`, CLI reference generated into `docs/`.
6. **Contract-first change order:** monorepo change → release (pre-release tags are fine) →
   downstream pin bump. Never the reverse. Automated dependency updates open the pin-bump pull
   request in each downstream repo.

## Coherence across repositories

- **`CONVENTIONS.md` at the root of every repository**, with the same shape: what the repo owns,
  what it consumes and at which pin, the change order above, and links into the monorepo docs.
  Anyone (or any tool) starting work in a repository reads it first.
- **Shared vocabulary:** `docs/glossary.md` in the monorepo. All repositories use its terms in
  the sense defined there.
- **Architecture decision records:** `docs/adr/NNNN-*.md` in the monorepo for decisions that
  span repositories. Downstream work cites an ADR rather than re-deriving a decision.
- **Repository map:** `docs/repos.md`, the canonical answer to "which repository does this belong
  in".
- **Machine checks over prose.** Configs are schema-validated, bindings are generated, boundaries
  are tested. Prose covers only what cannot be checked.
- **Cross-repo work** is tracked as a single monorepo issue labelled `repo:android`,
  `repo:infra` or `repo:site`, on one organisation-level project board.

## Decisions

| Decision | Choice | Record |
| --- | --- | --- |
| Per-repo conventions file | `CONVENTIONS.md` | ADR 0001 |
| JVM/Android namespace and Maven group | `org.tidefell` | ADR 0002 |
| Infrastructure tooling | Nix (flakes, NixOS modules) | ADR 0003 |
| Android integration | Full peer via FFI, no separate daemon | ADR 0004 |
| Versioning | Single workspace version, one pin per consumer | ADR 0005 |
| Wire encoding and signing | postcard, Ed25519 signed envelopes | ADR 0006 |
| Minimum supported Rust version | 1.91 | ADR 0007 |
| Chunk transfer | Own `blobs/1` protocol on the `store` crate, no iroh-blobs | ADR 0008 |

## Phases

| Phase | Scope | Exit criterion |
| --- | --- | --- |
| **0** Scaffold | Four repositories, licenses, CI | Done |
| **1** Foundations | `proto` message types and vectors, `chunker`, `store`, `meta` with unit tests; `xtask check-deps`, `cargo-deny`, `cargo-semver-checks`; glossary, ADRs, repo map; `CONVENTIONS.md` everywhere; monorepo `flake.nix`; release workflow | `v0.1.0` tag produces versioned artifacts; boundary violations fail CI |
| **2** Sync | `identity`, `net` (iroh endpoint, sync/1), blobs/1 on the block store, `fetch`, `engine`, `daemon`; two-peer tests in `tests/`; `tidefelld config schema`; infra relay and DNS as NixOS modules | Two daemons sync a folder end to end; infra CI validates config |
| **3** Surfaces | `api` (REST + gRPC, generated OpenAPI and proto), `cli`, `web` peer with accounts; `sdk` stabilised; mdBook built in release; site pulls docs by tag | Set up, share a folder and restore from the recycle bin via CLI and web |
| **4** Clients | `ffi` and AAR publishing; Android Gradle project, foreground service, `DocumentsProvider`; `fs-fuse` | Android app syncs against a full replica using the published AAR |
| **5** Hosted | Hosted web peer and full-replica storage in infra; camera-roll backup; retention UX | Hosted deployment runs from pinned artifacts only |

### Phase 1 work breakdown

1. Governance: `CONVENTIONS.md` in all repositories; `docs/glossary.md`, `docs/repos.md`,
   ADRs 0001–0005.
2. Guard rails: `xtask check-deps`, `deny.toml` and `cargo-deny` in CI, MSRV job on 1.85.
3. `proto`: identifiers (device, folder, file, chunk hash), signed envelope, sync/1 message
   types with a stable binary encoding, golden vectors in `specs/vectors/`.
4. `chunker`: FastCDC parameters fixed in the spec, BLAKE3 chunk and file hashes, chunk index.
5. `store`: content-addressed block store on disk, availability bitmap, LRU eviction for partial
   replicas.
6. `meta`: file tree, versions, vector clocks, tombstones, recycle bin with retention.
7. Release: `flake.nix`, tag-driven release workflow producing binaries and checksums.

### Status

- Phase 1 is complete and released as `v0.1.0`.
- Phase 2 steps 1–4 are implemented: identity, `net`, `fetch`, `engine` and `tidefelld`, with
  an end-to-end test of two daemon processes syncing a folder.
- Step 5: the NixOS module for `tidefelld` is in the monorepo flake. The monorepo side of
  Phase 2 is released as `v0.2.0`; the infra flake, with relay and DNS server hosts pinned to
  that release, is the remaining step.

### Phase 2 work breakdown

Delivered as a sequence of pull requests; each merges before the next is pushed.

1. **Identity and wire updates.** ADR 0008. `identity` crate: device keys (the device's
   Ed25519 key is also its iroh endpoint key, so `DeviceId` = iroh `EndpointId`), user keys,
   device certificates (a user key signs "this device belongs to me"), and folder membership
   (a folder lists the users allowed to sync it; a device is a member if it presents a valid
   certificate from one of those users). `proto`: signed envelopes take any Ed25519 signer;
   `Hello` carries the device certificate; `blobs/1` messages and spec.
2. **`net`.** iroh endpoint built from the device key; relay (default, custom or disabled) and
   address lookup configuration; `sync/1` and `blobs/1` over QUIC streams with frame I/O.
   Storage is reached through traits that `engine` implements, so `net` does not depend on
   `meta` or `store`. In-process multi-peer tests with relays disabled.
3. **`fetch`.** Rarest-first scheduler over per-peer availability bitmaps, with per-peer
   concurrency limits, retry with backoff, and peer failure handling. Transport-agnostic and
   tested with simulated peers.
4. **`engine` and `daemon`.** Replica policies (full; partial with a byte budget), folder
   scanning (polling first, notification later), materialising files atomically from chunks,
   deletes into the recycle bin with chunks pinned until purge. `tidefelld` with TOML config,
   `tidefelld config schema` (JSON Schema via `schemars`), `init`, `id` and `run`. End-to-end
   test: two daemons sync a folder, including edits, deletes and a conflict.
5. **Nix and infra.** NixOS module for `tidefelld` in the monorepo flake; relay and DNS server
   NixOS configurations in `tidefell/infra`, pinned to the monorepo release.
