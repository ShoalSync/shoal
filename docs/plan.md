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
   ├─ tidefell-ffi AAR asset (generated Kotlin)   → android
   ├─ tidefelld / web binaries (Nix flake outputs) → infra
   ├─ tidefelld config JSON Schema                 → infra (validated in CI)
   ├─ OpenAPI document for `api`                   → external clients
   └─ built mdBook + specs                         → site
```

| Edge | Artifact | How the consumer pins it |
| --- | --- | --- |
| monorepo → android | `tidefell-ffi-X.Y.Z.aar` with uniffi-generated Kotlin, attached to the release (ADR 0014) | `gradle/libs.versions.toml`: `tidefell = "X.Y.Z"`; the build downloads and checks the AAR. A `tidefellFfiLocalPath` Gradle property switches to a composite build against a local monorepo checkout for FFI development. |
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
| API | REST/JSON with generated OpenAPI; gRPC deferred | ADR 0009 |
| Local control and web peer placement | Unix socket; web peer inside `tidefelld` | ADR 0010 |
| Web accounts | Local logins mapped to Tidefell users; no user keys on servers | ADR 0011 |
| Web UI | Server-rendered with htmx | ADR 0012 |
| Website and docs | Zola site, mdBook docs per release | ADR 0013 |
| Delivering the ffi AAR | Release asset fetched with the org GitHub App token; Maven later | ADR 0014 |
| Adding devices | Pairing over the network (`pair/1`); user keys never leave their machine | ADR 0015 |
| Partial replicas | Placeholders, fetch on open, LRU eviction to a budget; Android `DocumentsProvider` | ADR 0016 |
| Linux placeholders | `tidefelld` mounts folders read-write with FUSE (`fuser`) | ADR 0017 |

## Phases

| Phase | Scope | Exit criterion |
| --- | --- | --- |
| **0** Scaffold | Four repositories, licenses, CI | Done |
| **1** Foundations | `proto` message types and vectors, `chunker`, `store`, `meta` with unit tests; `xtask check-deps`, `cargo-deny`, `cargo-semver-checks`; glossary, ADRs, repo map; `CONVENTIONS.md` everywhere; monorepo `flake.nix`; release workflow | `v0.1.0` tag produces versioned artifacts; boundary violations fail CI |
| **2** Sync | `identity`, `net` (iroh endpoint, sync/1), blobs/1 on the block store, `fetch`, `engine`, `daemon`; two-peer tests in `tests/`; `tidefelld config schema`; infra relay and DNS as NixOS modules | Two daemons sync a folder end to end; infra CI validates config |
| **3** Surfaces | `api` (REST with generated OpenAPI; ADR 0009), `cli`, `web` peer with accounts; `sdk` stabilised; mdBook built in release; site pulls docs by tag | Set up, share a folder and restore from the recycle bin via CLI and web |
| **4** Clients | On-demand partial replicas; `pair/1`; `ffi` and the AAR release asset; Android Gradle project, foreground service, `DocumentsProvider` | Android app syncs against a full replica using the published AAR |
| **4b** Desktop placeholders | `fs-fuse` on the same on-demand engine support | A Linux partial replica mounts a folder and opens placeholders on demand |
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
- Phase 2 is complete: the monorepo side is released as `v0.2.0`, and `tidefell/infra`
  pins it with relay, DNS and full-replica roles.
- Phase 3 steps 1–6 are implemented: the `tidefell` CLI, the web peer, and the book built by
  each release for the site to serve. It completes with the `v0.3.0` release and the site pin.
- Phase 3 is complete: released as `v0.3.0`, and `tidefell/site` serves its docs.
- Phase 4 is complete: released as `v0.4.0`. `tidefell/android` pins it and runs the exit
  criterion on an emulator in CI.
- Phase 4b is in progress: decisions in ADR 0017; steps 1–4 are implemented, with the exit
  criterion running in CI against a real FUSE mount.

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

### Phase 3 work breakdown

Exit criterion: set up a device, share a folder with another device, and restore a deleted file
from the recycle bin, both through the CLI and through the web peer.

1. **Decisions.** ADRs 0009–0013 and this breakdown.
2. **Engine operations.** Add, join and remove folders at runtime (persisted in the data
   directory alongside configured folders), restore from the recycle bin, list files, and a
   stream of status events.
3. **`sdk` and `api`.** `sdk` (MIT OR Apache-2.0) owns the `/v1` request and response types
   and their OpenAPI schemas, and provides a typed client over a Unix socket or HTTP. `api`
   serves them with axum on the control socket, including server-sent events and
   `/v1/openapi.json`. The OpenAPI document is published with each release.
4. **`tidefell` CLI.** `status`, `folder create|join|list|remove|share|unshare|scan`, `files`,
   `peer list|add|remove`, `recycle list|restore`, `events`. Talks to the daemon only through
   `sdk`. Web accounts are managed on the web peer's host with `tidefelld web account`, since
   they live in its data directory.
5. **`web`.** Accounts (argon2id) linked to user IDs, sessions with CSRF protection, and a
   server-rendered htmx UI: sign in, folders, file listing and download, recycle bin with
   restore, and account management for admins. Enabled by `[web]` in `tidefelld`.
6. **Docs and site.** `docs/` becomes an mdBook built by the release workflow; `tidefell/site`
   becomes a Zola site that serves the pinned release's docs.

### Phase 4 work breakdown

Exit criterion: the Android app, built against the AAR attached to a monorepo release, pairs
with a `tidefelld` full replica, joins a folder, lists it, opens a file on demand, and syncs a
file it saves back to the daemon. Android CI runs this against the release's `tidefelld` on an
emulator.

1. **Decisions.** ADRs 0014–0016 and this breakdown.
2. **Partial replicas on demand (ADR 0016).** Placeholders in the engine's local state; `open`
   and `write`; pinning while open; LRU eviction to the budget that turns materialised files
   back into placeholders. Exposed through `sdk`/`api` (`tidefell open`), and the web peer can
   download placeholders. Tests with a full and a partial replica in process.
3. **Pairing (ADR 0015).** `specs/pair-v1.md` and vectors; `proto` messages; pairing code and
   confirmation code in `identity`; the `pair/1` handler in `net`; an uncertified,
   pairing-only engine mode; the approver side in the engine, `api` and `tidefell pair <code>`.
   Test: a new device pairs with a daemon and then syncs a folder.
4. **`ffi`.** uniffi (proc-macro) surface over `engine`: lifecycle, pairing, folders, files,
   `open`/`write`, recycle bin, peers, and events through a callback interface. Kotlin JVM
   tests in monorepo CI load the host library and sync with an in-process peer.
5. **AAR.** `bindings/android/`, a Gradle Android library built with `cargo-ndk` for
   `arm64-v8a`, `armeabi-v7a` and `x86_64`; CI builds it and the release workflow attaches
   `tidefell-ffi-X.Y.Z.aar`. Released as `v0.4.0-rc.1` for the Android work to pin.
6. **Android app.** Gradle project (Kotlin, Jetpack Compose, Material 3; minimum SDK 29):
   foreground service hosting the engine, pairing screen with a QR code, folder list and join,
   `DocumentsProvider`, storage budget setting. CI builds and unit-tests it, then runs the exit
   criterion on an emulator against the pinned release's `tidefelld`.
7. **Release.** `v0.4.0` and the Android pin bump.

### Phase 4b work breakdown

Exit criterion: a `tidefelld` partial replica on Linux mounts a folder. Listing it shows every
file, reading a placeholder fetches it from a full replica, and a file saved through the mount
reaches the full replica. CI runs this with a real FUSE mount on the self-hosted runner.

1. **Decisions.** ADR 0017 and this breakdown.
2. **Engine `delete` and `rename`.** `delete` records a delete that sends the file to peers'
   recycle bins. `rename` moves an entry, or every entry under a directory, to a new path while
   keeping its content reference, so a placeholder is renamed without a fetch and a rename over
   an existing file replaces it. Tests with a full and a partial replica in process.
3. **`fs-fuse`.** A `fuser` filesystem over a small backend trait that the engine implements:
   inodes from folder metadata; `lookup`, `getattr` and `readdir`; `open` and `read` through the
   engine's `open` and `close`; staged writes recorded with `write`; `create`, `unlink`,
   `rename`, in-memory `mkdir` and `rmdir`, truncation and times; kernel cache invalidation on
   engine events. Unit tests drive the handlers against a fake backend without the kernel.
4. **Daemon mounts.** `mount = "/path"` on a folder in `tidefelld`'s config and schema;
   mounting at start, unmounting on shutdown (including SIGTERM), and clearing a stale mount
   left by a crash. A guide page on mounting. The end-to-end exit-criterion test
   runs in CI against a real FUSE mount.
5. **Release.** `v0.5.0`.
