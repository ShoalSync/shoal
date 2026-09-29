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
   engine lifecycle and nothing else. `cargo-semver-checks` runs on `proto` and `sdk` against the
   last release: it reports breaking changes before 1.0 and fails on them from 1.0 (ADR 0026).
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
| Always-on peers | Self-run full replica now; encrypted storage peer in a later phase | ADR 0018 |
| Web uploads and share links | Uploads and read-only share links in the web peer, off by default | ADR 0019 |
| Camera-roll backup | Android, opt-in, upload only, originals kept by default | ADR 0020 |
| Retention settings | Editable at runtime, recycle-bin size shown | ADR 0021 |
| Confirmed copies | Full replicas confirm holding a file; eviction and removing originals wait for one | ADR 0022 |
| Encrypted storage peers | Encrypted folders (sync/2, blobs/2), HPKE key grants, storage mode in `tidefelld` | ADR 0023 |
| Sync consistency and trust | Changes build on what is on disk; same-path and equal-vector rules; bounded exchange with deadlines; authors vouched for by member certificates | ADR 0024 |
| Phone folders | Any folder on the phone, per-folder sync options, removal through the recycle bin | ADR 0025 |
| Compatibility | None before 1.0; from 1.0 a deprecation period, then removal | ADR 0026 |
| File content on disk | Kept once per device; files are the content, the block store holds the rest | ADR 0027 |
| Other devices' files on the phone | Download action and read-only documents provider roots | ADR 0028 |
| Features from other sync tools | Ignore patterns, watching, local discovery, version history, bandwidth limits, pins, upload links, conflict resolution | ADR 0029 |
| Diagnostics and reporting | Local logs and crash files, detailed status, redacted diagnostics bundles, doctor, activity history, opt-in metrics; no telemetry | ADR 0030 |
| Static analysis and fuzzing | Stricter lints on peers' data, property tests, nightly fuzzing, workflow, Kotlin, shell, Nix and spelling checks | ADR 0031 |
| Transfer efficiency | Files written concurrently, no manifest for single-chunk files, small files batched per request, no availability query to full replicas; wire changes wait for sync/2 | ADR 0032 |
| File names across filesystems | NFC enforced, each folder probes its filesystem, names that fold together follow the same-path rule, unstorable names listed, not escaped | ADR 0033 |
| Metadata storage and recovery | One transactional database per folder, written per change; `metadata_dir` for fast storage; checked on open, rebuilt per folder from disk, peers and sidecars | ADR 0034 |
| Memory at scale | Manifests, chunk index and version bodies on disk behind a cache; memory budgets tested nightly | ADR 0035 |
| Simulation testing | Seeded multi-device simulation over an in-memory transport, with convergence and no-loss invariants | ADR 0036 |
| Disk space and I/O errors | Folder markers and an unavailable state, paused mass deletes, free-space floors, failed metadata writes stop applying changes | ADR 0037 |
| Threat model | One threat model document; device revocation next; Android keys in the Keystore; relay metadata documented | ADR 0038 |
| Android battery | Measured per release and counted on the device; usage telemetry only through its own ADR | ADR 0039 |

## Phases

| Phase | Scope | Exit criterion |
| --- | --- | --- |
| **0** Scaffold | Four repositories, licenses, CI | Done |
| **1** Foundations | `proto` message types and vectors, `chunker`, `store`, `meta` with unit tests; `xtask check-deps`, `cargo-deny`, `cargo-semver-checks`; glossary, ADRs, repo map; `CONVENTIONS.md` everywhere; monorepo `flake.nix`; release workflow | `v0.1.0` tag produces versioned artifacts; boundary violations fail CI |
| **2** Sync | `identity`, `net` (iroh endpoint, sync/1), blobs/1 on the block store, `fetch`, `engine`, `daemon`; two-peer tests in `tests/`; `tidefelld config schema`; infra relay and DNS as NixOS modules | Two daemons sync a folder end to end; infra CI validates config |
| **3** Surfaces | `api` (REST with generated OpenAPI; ADR 0009), `cli`, `web` peer with accounts; `sdk` stabilised; mdBook built in release; site pulls docs by tag | Set up, share a folder and restore from the recycle bin via CLI and web |
| **4** Clients | On-demand partial replicas; `pair/1`; `ffi` and the AAR release asset; Android Gradle project, foreground service, `DocumentsProvider` | Android app syncs against a full replica using the published AAR |
| **4b** Desktop placeholders | `fs-fuse` on the same on-demand engine support | A Linux partial replica mounts a folder and opens placeholders on demand |
| **5** Always on and sharing | Self-run always-on full replica deployed by infra; web peer uploads and share links; camera-roll backup; retention settings | A self-run always-on peer, deployed from pinned artifacts only, keeps a phone and a laptop in sync while they are never online together |
| **5b** Confirmed copies | The engine records which full replicas hold each file made on a device; eviction and the Android app's removal of backed-up originals rely on it | Android removes a backed-up photo only after a full replica confirms it holds the photo, and a partial replica never evicts a file it made without such a copy |
| **5c** Review fixes | The fixes from the codebase review: sync correctness and flow control (ADR 0024), Android safety, supply chain, hardening and maintainability | Every high finding has a regression test that failed before its fix; a folder with 100,000 entries completes its first sync; releases are built off shared runners and signed |
| **5d** Phone folders | Per-device sync direction and local deletes in the engine; any folder on the phone with backup only, two-way sync and remove after backup (ADR 0025) | On an emulator, a phone folder syncs with a daemon on each option, and removal waits for the daemon's confirmed copy |
| **5e** Features from other sync tools | Ignore patterns, watching for changes, local network discovery, version history, bandwidth limits and pause, pins on partial replicas, upload-only links, conflict resolution (ADR 0029) | See the Phase 5e breakdown |
| **5f** Diagnostics and reporting | Logs and crash files on every platform, status and problems, diagnostics bundle and doctor, activity history, metrics, crash reports through the issue tracker (ADR 0030) | See the Phase 5f breakdown |
| **5g** Static analysis and fuzzing | Unused dependencies and the public API checked; stricter lints in `proto`, `net` and `store`; property tests; fuzzing; workflow, Android, shell, Nix and spelling checks in every repository (ADR 0031) | See the Phase 5g breakdown |
| **5h** Transfer efficiency | Concurrent file writes, the single-chunk shortcut, cross-file chunk requests and direct requests to full replicas, measured before and after (ADR 0032) | See the Phase 5h breakdown |
| **5i** File names across filesystems | NFC on the wire, filesystem probing, per-device collisions after folding, unstorable names as problems (ADR 0033) | See the Phase 5i breakdown |
| **5j** Metadata storage | Per-folder databases, `metadata_dir`, integrity checks, per-folder rebuild with sidecars for kept files (ADR 0034) | See the Phase 5j breakdown |
| **5k** Robustness | Memory budgets, simulation testing, folder markers, free-space floors and error handling (ADR 0035, ADR 0036, ADR 0037) | See the Phase 5k breakdown |
| **5l** Security and battery | Threat model, Android Keystore, device revocation ADR, battery benchmark and counters (ADR 0038, ADR 0039) | See the Phase 5l breakdown |
| **6** Encrypted storage peers | `crypto` crate, sync/2 and blobs/2 specs first, encrypted folders on member devices, storage mode in `tidefelld`, `role-storage-peer` in infra | A phone-like and a laptop-like member, never online together, sync an encrypted folder through a storage peer whose disk holds no plaintext paths or content |
| **7** iOS client (planned) | Swift app over the `ffi` crate's uniffi Swift bindings, shipped as an XCFramework release asset; File Provider extension for placeholders; pairing; camera-roll backup | An iPhone app, built against the published XCFramework, pairs with a full replica, shows a folder in the Files app and opens a placeholder on demand |
| **8** Windows client (planned) | `tidefelld` as a Windows service; placeholders through the Cloud Files API (a new `fs-cfapi` crate); tray app for pairing and status; signed installer | A Windows partial replica shows a folder in Explorer with placeholders that open on demand |

### Client phases (outline)

Phases 7 and 8 get their own decisions and breakdown when they start. Both wait for CI
runners: a Mac for iOS, a Windows machine or VM for Windows. Known constraints:

- **iOS.** A new `tidefell/ios` repository consumes a `TidefellFFI.xcframework` release asset,
  as Android consumes the AAR (ADR 0014). The File Provider extension runs under tight memory
  and background-time limits, so the engine either runs inside the extension or in the app
  with the extension talking to it through an app group; this is the phase's first decision.
  The same File Provider framework also serves macOS, which could follow cheaply. Building
  and testing need macOS runners: hosted macOS minutes are expensive, so a self-hosted Mac is
  likely.
- **Windows.** Rust code stays in the monorepo: `fs-cfapi` beside `fs-fuse` on the same engine
  support (ADR 0016), and the release adds a Windows build and installer. CI needs Windows
  runners, hosted or a self-hosted Windows VM.

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
- Phase 4b is complete: released as `v0.5.0`. A Linux partial replica mounts a folder with
  FUSE, and the exit criterion runs in CI against a real mount.
- Phase 5 is decided: ADRs 0018–0021 are accepted, with option C of ADR 0018.
- Phase 5 step 2 is implemented: retention changes at runtime, recycle-bin summaries and
  emptying the recycle bin, in the engine, API, sdk and CLI.
- Phase 5 step 3 is implemented: web uploads and share links (expiry, password, download
  limit, revocation), off unless `[web] share_links = true`.
- Phase 5 step 4 is implemented: `tidefell/infra` has `role-always-on` and a three-VM test of
  the exit criterion, and the guide explains running an always-on peer.
- Phase 5 step 5 is implemented: the Android bindings expose retention, the recycle-bin
  summary, emptying the recycle bin and deleting files. Camera-roll backup follows `v0.6.0`.
- Phase 5 step 6 is implemented: the README and the book describe self-run always-on peers,
  uploads, share links and per-device retention; no page suggests a project-run service.
- Phase 5 is complete: released as `v0.6.0`; `tidefell/infra` and `tidefell/site` pin it.
  `tidefell/android` builds against it with camera-roll backup and retention and recycle-bin
  settings, and its emulator exit criterion backs up a photo to a daemon. The setting to remove
  originals after backup is deferred until the engine can report which peers hold a file.
- Phases 5b and 6 are decided: ADRs 0022 and 0023 are accepted. Phases 7 and 8 wait for CI
  runners.
- Phase 5b is complete: released as `v0.7.0`. Full replicas report their kind, the engine
  records confirmed copies, and a partial replica evicts a file it made only once one exists.
  `tidefell/android` pins it and offers to remove backed-up photos from the phone, off by
  default; its emulator exit criterion checks that a backed-up photo becomes eligible only after
  the daemon, a full replica, confirms it. `tidefell/infra` and `tidefell/site` still pin
  `v0.6.0`.
- Phase 5c is decided: ADR 0024 is accepted. Steps 2 and 3 are implemented: local changes build
  on what is on disk, equal vectors are concurrent, entries at the same path are written by the
  ADR 0024 rule, counters have a ceiling, `.tidefell` is reserved, manifests are checked against
  sizes, index batches are paged and not echoed, and handshakes and requests have deadlines. A
  test syncs 100,000 entries per replica. Step 4 is implemented in `tidefell/android`
  (tidefell/android#9, against `v0.7.0`): a photo edited after its backup is not offered for
  removal and is backed up again, backup runs one at a time, the sync service stops at the
  Android 15 time limit, and app data is excluded from backups and device transfer.
- `v0.8.0` releases Phase 5c steps 2 and 3. `tidefell/infra`, `tidefell/site` and
  `tidefell/android` pin it.
- Phase 5c step 7 is implemented in this repository: `engine.rs` is split by concern
  (folders, files, the recycle bin and peers), folder defaults and the retention and mode
  conversions live once in the engine and are shared by the API, the bindings and the daemon
  configuration, and every crate forbids unsafe code. The Android part is tidefell/android#11 (merged):
  view models hold each screen's state, and node and `MediaStore` work runs on
  `Dispatchers.IO`. Steps 5 and 6 are not started.
- Phase 5d is decided: ADR 0025 (phone folders on Android) and ADR 0026 (compatibility from
  1.0). Step 2 is implemented: per-device `direction` and `local deletes` settings, files
  dropped from one device, and `remove_here` into the device's recycle bin, in the engine,
  configuration, API, sdk, CLI and bindings. A new file at a path whose entry was never on disk
  here is kept beside the other device's file unless its content matches.
- `v0.9.0` releases Phase 5d step 2. `tidefell/infra`, `tidefell/site` and `tidefell/android` pin
  it. Step 3 is implemented in `tidefell/android` (tidefell/android#12): any folder on the phone
  with the three sync options, clean up and the recycle bin, and the emulator exit criterion.
- ADR 0027: content is kept once per device. Files on disk are no longer copied into the block
  store; chunks are read from them, and deleted files are kept for the recycle bin by moving them
  aside.
- ADR 0028: a phone folder's files that are only on other devices can be downloaded from the
  app, and each phone folder is a read-only root of the documents provider listing them.
- `v0.10.0` releases ADRs 0027 and 0028, with engine state format 6. The Android side of
  ADR 0028 is tidefell/android#13.
- Phase 5e is decided: ADR 0029 (features from other sync tools) and its breakdown below. Step 2
  (local network discovery) is implemented in this repository; the Android multicast lock
  follows with the `v0.11.0` pin. Steps 3 (ignore patterns) and 4 (watching) are implemented.
  Step 5 (version history) is implemented in this repository; the Android screen follows with
  the `v0.12.0` pin. Steps 6 (bandwidth limits and pause) and 7 (pins) are implemented; their
  Android settings and screens follow with the `v0.12.0` pin. Step 8 (upload-only links) is
  implemented. Step 9's sync/1 change, version vectors with holes so conflicts last until
  they are resolved, is implemented, and so are resolving, the conflict event, the CLI and the web screens;
  the Android notification and screen follow with the `v0.12.0` pin.
- `v0.11.0` releases Phase 5e steps 2 to 4 (local discovery, ignore patterns and watching) and
  the recycle bin's local clock (tidefell/tidefell#46). The bindings' `Network` gains
  `localDiscovery`.
- `v0.12.0` releases the rest of Phase 5e: version history, bandwidth limits and pause, pins,
  upload-only links and conflict resolution (steps 5 to 9), with Phase 5f step 2 (logs and
  crashes) and folder root markers (ADR 0037). sync/1 version vectors can have holes; engine
  state is format 9 and meta snapshots format 2, upgraded on load. The Android screens follow
  with the `v0.12.0` pin.
- Phase 5f is decided: ADR 0030 (diagnostics and reporting). Step 2 (logs and crashes) is
  implemented, and its Android side came with the `v0.12.0` pin. Step 3 (status and problems)
  is implemented; the app's screens follow with the next Android pin. Step 4 (the doctor
  and the diagnostics bundle) is implemented; the app's screens follow with the next Android
  pin. Step 5 (activity history) is implemented; the app's screen follows with the next
  Android pin. Step 6 (metrics) is implemented in `tidefelld`; the scrape option in
  `tidefell/infra` comes with its pin after the next release.

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

### Phase 5 work breakdown

Delivered as a sequence of pull requests; each merges before the next is pushed. ADR 0018
takes option C: a self-run always-on peer now, and encrypted storage peers in a later phase.

1. **Decisions.** ADRs 0018–0021 and this breakdown.
2. **Retention settings (ADR 0021).** Engine, API, sdk and CLI: change retention, recycle-bin
   summary, empty the recycle bin. The web peer and Android surfaces follow in their own steps.
3. **Web uploads and share links (ADR 0019).** Uploads and new folders through the web peer;
   share links with expiry, password, download limit and revocation; link pages; tests that
   drive the web peer over HTTP.
4. **Always-on peer (ADR 0018, option B).** `tidefell/infra`: a NixOS role for a self-run full
   replica with the web peer, relay and TLS, pinned to a release; a guide page on running one
   on a small VPS. The exit criterion runs in infra CI: a phone-like partial replica and a
   laptop-like one, never online together, sync through the always-on peer.
5. **Android bindings.** The `ffi` crate exposes what the Android app needs from steps 2 and 5:
   changing retention, the recycle-bin summary on each folder, emptying the recycle bin, and
   deleting files. They ship in `v0.6.0`, which the app builds against.
6. **Site and docs.** Remove any suggestion of a project-run service; document the always-on
   peer, sharing and backup.
7. **Release.** `v0.6.0`, and the infra and site pin bumps.
8. **Camera-roll backup (ADR 0020).** `tidefell/android`, against the `v0.6.0` AAR: backup
   settings, `MediaStore` tracking, `WorkManager` scheduling, and retention and recycle-bin
   settings. Tested on the emulator with seeded media. The Android pin moves to `v0.6.0` here.
   The off-by-default setting to remove originals after backup is deferred: it needs the engine
   to report which peers hold a file.

### Phase 5b work breakdown

1. **Decisions.** ADRs 0022 and 0023, and the breakdowns of Phases 5b and 6.
2. **Replica kind on the wire.** blobs/1 `ReplicaRequest` and `Replica`, appended, with
   golden vectors, spec and `net` handlers.
3. **Confirmed copies in the engine.** Records of which full replicas hold each file,
   persisted; the checks that fill them in; eviction requires one. Two-daemon tests with a full
   and a partial peer.
4. **Surfaces.** API, sdk, CLI (`ls`, `status`), web peer and `ffi` report files without a
   confirmed copy. Release `v0.7.0`.
5. **Removing originals (ADR 0020, 0022).** `tidefell/android` against `v0.7.0`: the
   off-by-default setting on Android 11 and later, the eligibility notification and the system
   delete request. The emulator exit criterion checks that a backed-up photo becomes eligible
   only after the daemon, a full replica, confirms it.

### Phase 5c work breakdown

Each fix starts with a test that reproduces the finding and fails, then makes it pass.

1. **Decisions.** ADR 0024 and the sync/1 spec changes.
2. **Sync correctness.** Local changes build on the vector of what is on disk; equal vectors
   are concurrent; entries at the same path are written by the ADR 0024 rule; counters have a
   ceiling and checked increments; `.tidefell` is reserved in path validation; manifests are
   checked against the version's size and dropped when assembly fails.
3. **Flow control and deadlines.** Paged index batches written from their own task, no echo,
   deadlines on handshakes and requests, a 64 KiB limit before authentication, and no network
   I/O under a folder's lock. A test syncs 100,000 entries. Release `v0.8.0`, and the infra,
   site and Android pins.
4. **Android safety.** Re-check that a photo is unchanged before offering to remove it and back
   it up again if it changed; handle the Android 15 foreground-service timeout; exclude app
   data from device transfer; one backup run at a time.
5. **Supply chain.** Releases on a dedicated runner with no pull-request jobs, a pinned
   toolchain and actions pinned by commit, release assets signed with build provenance, and a
   release only for tags on `main` that passed CI.
6. **Hardening.** Author certificates (the stream in `v0.8.x`, enforcement in the release after),
   confirmed copies from current members only, web upload limits and login throttling, relay
   access tokens and systemd hardening in infra.
7. **Maintainability.** Split `engine.rs`, view models and background dispatchers in the Android
   app, shared type conversions, and `unsafe_code = "forbid"` across the workspace.

### Phase 5d work breakdown

Phone folders (ADR 0025). Nothing has been released publicly, so nothing migrates (ADR 0026).

1. **Decisions.** ADRs 0025 and 0026 and this breakdown.
2. **Engine settings.** Per-device `direction` (`both`, `send-only`) and `local deletes`
   (`everywhere`, `this-device`) on each folder, in the engine state, configuration, API, sdk,
   CLI and `ffi`. Files dropped on this device are listed as on other devices and are not
   fetched again, even for new versions, unless opened. Moving a file to the local recycle bin
   without recording a delete. Release `v0.9.0`, and the infra, site and Android pins.
3. **Android folders.** `tidefell/android`: All files access, the folder picker with the camera
   folder first, the three sync options in order (backup only, two-way sync, remove after
   backup), "Delete from all devices", clean up, and `MediaStore` announcements for files
   written into shared storage. The camera backup and its removal notification are removed.
4. **Exit criterion.** On the emulator: a file added to a phone folder reaches the daemon on
   each option; deleting it on the phone leaves it on the daemon; with remove after backup it
   moves to the phone's recycle bin only after the daemon confirms it; with two-way sync a file
   added on the daemon appears in the phone folder.

### Phase 5e work breakdown

Features from other sync tools (ADR 0029). It can run alongside Phase 5c steps 5 and 6.

1. **Decisions.** ADR 0029 and this breakdown.
2. **Local network discovery.** mDNS address lookup in the engine, `local_discovery` in the
   configuration, API and bindings, and the multicast lock on Android. A test syncs two
   daemons with the relay disabled and no address lookup.
3. **Ignore patterns.** `.tidefellignore` with `#include` and the built-in defaults, applied
   to scans, sending and writing. Ignored paths are listed as on other devices.
4. **Watching.** `notify` watches in `tidefelld`, scans of the touched directories, the
   one-hour safety scan, and the polling fallback shown in `tidefell status`. Release
   `v0.11.0`, and the infra, site and Android pins.
5. **Version history.** Replaced content kept as previous versions, retention and thinning,
   restore as a new version, engine state format 7, and history in the API, CLI, web and
   Android app.
6. **Bandwidth limits and pause.** Rate limits with local peers exempt, and device and folder
   pause that survives restarts.
7. **Pins.** Pinned files and directories on partial replicas, fetched in the background and
   never evicted, from the CLI, web and Android app.
8. **Upload-only links.** Upload-only directory links in the web peer, with size and count
   limits.
9. **Conflict resolution.** The sync/1 change that keeps conflict copies until they are
   resolved, same-content versions merged without a conflict, the `ConflictDetected` event,
   `open_version`, and `resolve` with keep a version and keep both. `tidefell conflicts` and
   `tidefell resolve`, the conflict screen and folder banner in the web, and the notification and
   conflict screen in the Android app. Release `v0.12.0`, and the infra, site and Android pins.

Exit criterion: with the relay disabled, two daemons on one network sync directly; an ignored
file never leaves its device; a change is picked up within two seconds without a full scan; a
file replaced by another device's edit is restored from its history; a paused folder transfers
nothing; a pinned directory stays on a partial replica over its budget; an upload-only link
accepts a file without showing the directory; and a conflict survives a later edit, prompts on
Android, and is resolved from the CLI by keeping both versions.

### Phase 5f work breakdown

Diagnostics and reporting (ADR 0030). It runs alongside Phase 5e. Step 2 comes first, right
after Phase 5e step 4 (`v0.11.0`), since Android reports have no engine logs today.

1. **Decisions.** ADR 0030 and this breakdown.
2. **Logs and crashes.** The rotating JSON log file, logcat on Android, the runtime log level,
   the recent-problems buffer, the panic hook and crash files, fatal engine-task panics, and the
   crash notice on the next start. The Android side (the app's log calls, the uncaught-exception
   handler, restart after a crash) is in `tidefell/android`.
3. **Status and problems.** Peer connections, folder states, per-path problems and transfer
   rates in the engine, API, sdk, bindings, CLI, web and Android app.
4. **Diagnostics bundle and doctor.** The redacted bundle and its preview, `tidefell doctor`
   and the app and web checks, and the clock in sync/1's `Hello`.
5. **Activity history.** Who made each file's current version, and the per-folder activity
   log, in the API, CLI, web and Android app.
6. **Metrics.** The opt-in Prometheus endpoint and the scrape option in `tidefell/infra`.
7. **Reporting a crash.** The pre-filled issue from the CLI, web and app, and `report_url`.
   Release, with the infra, site and Android pins.

Exit criterion: a forced panic on `tidefelld` and on Android leaves a crash file and a notice
on the next start; a diagnostics bundle from each holds logs, status, problems and the crash,
with no plain path or full ID unless paths were included; a folder on a read-only disk lists the
problem in the CLI and the app; `tidefell doctor` warns about a peer whose clock is five minutes
off; and `/metrics` counts bytes during a sync.

### Phase 5g work breakdown

Static analysis and fuzzing (ADR 0031). It can run alongside Phases 5e and 5f.

1. **Decisions.** ADR 0031 and this breakdown.
2. **Dependencies and API.** `cargo machete`, with the unused dependencies it found removed,
   and `cargo semver-checks` on `proto` and `sdk` against the last release.
3. **Stricter lints.** The four restriction lints in the library code of `proto`, `net` and
   `store`, with each existing site fixed or given a reason.
4. **Property tests.** Round trips and no-panic decoding in `proto`, and convergence in `meta`.
5. **Fuzzing.** The `fuzz/` crate and its targets, and the nightly job on `main`. Crashes found
   become regression tests.
6. **Workflow checks.** `actionlint` and `zizmor` in all four repositories, with or after the
   action pinning of Phase 5c step 5.
7. **Android.** Android Lint and `detekt` in `tidefell/android` CI, with baselines.
8. **Smaller checks.** `shellcheck`, `statix` and `deadnix` as flake checks, and `typos`, in every
   repository that has files for them.

Exit criterion: a pull request adding an unchecked index to `proto` fails CI; the nightly job runs
every fuzz target for ten minutes without a crash; a new Android Lint error fails the Android CI;
an unpinned action fails `zizmor` in each repository; and `shellcheck`, `statix`, `deadnix` and
`typos` run in every repository's CI.

### Phase 5h work breakdown

Transfer efficiency (ADR 0032). Engine changes only; sync/1 and blobs/1 are unchanged.

1. **Decisions.** ADR 0032 and this breakdown.
2. **Measurement.** A test syncing 1,000 files of 2 KB and one file of 256 MiB between two
   daemons, with a fixed delay added to every request, that reports streams, requests and time.
   Its first numbers are the baseline.
3. **Concurrent writes.** Up to 8 files at once with at most 64 MiB in flight, overlapping paths
   in order, placeholders opened ahead of background writes.
4. **Single-chunk files.** Chunks requested by content hash for files under 64 KiB, the
   one-chunk manifest recorded, and the manifest fetched when a peer answers `Missing`.
5. **Fewer requests.** Chunk requests batched across files, and no availability query to full
   replicas or a single candidate peer.

Exit criterion: with a 100 ms delay per request, 1,000 files of 2 KB sync at least five times
faster than the step 2 baseline, the 256 MiB file no slower, and every file is intact.

### Phase 5i work breakdown

File names across filesystems (ADR 0033). It comes before the macOS, iOS and Windows clients
(Phases 7 and 8), and first because phone folders already use case-insensitive storage.

1. **Decisions.** ADR 0033 and this breakdown.
2. **Reproduce.** An emulator test in `tidefell/android`: a daemon shares `Photo.jpg` and
   `photo.jpg` with different content into a phone folder in shared storage, and a name with `:`
   into one on a FAT image. Record what happens today.
3. **NFC.** The scanner normalises names and remembers the on-disk spelling; sync/1 rejects
   paths that are not NFC.
4. **Probing.** Case and normalisation sensitivity, refused characters and names, and name length
   per folder, in `tidefell doctor` and the API.
5. **Collisions and refused names.** The same-path rule applied after each device's folding,
   case-only directories merged, refused names listed as problems with a rename offered, and
   case-only renames recorded. An engine test runs with a simulated case-insensitive, FAT-like
   folder.

Exit criterion: on the emulator, `Photo.jpg` and `photo.jpg` both survive in a phone folder
under distinct names while the daemon keeps both paths unchanged, and a name with `:` is listed
as a problem instead of failing on every pass.

### Phase 5j work breakdown

Metadata storage and recovery (ADR 0034). It decides whether the 100,000-entry target of Phase 5c
holds, so it comes before larger folders are promised.

1. **Decisions.** ADR 0034 and this breakdown.
2. **Measure and choose.** Save size and time, and folder-lock time, at 10,000, 100,000 and
   1,000,000 entries with today's files, then `redb` against SQLite on the same workload. The
   result is recorded in ADR 0034.
3. **Durable renames.** fsync the directory after every rename into place, in the engine and
   `meta`.
4. **Per-folder databases.** The tables, per-change transactions, durable commits for local
   changes and index batches, snapshot reads without the folder lock, and conversion of existing
   `meta` and `state` files.
5. **`metadata_dir`.** The setting in the configuration, CLI and API, the refusal to start when
   the databases were left behind, and the guide.
6. **Detection and rebuild.** Integrity checks on open, daily and in `tidefell doctor`; sidecars
   for kept files; the `rebuilding` state; per-folder rebuild without stopping other folders;
   `tidefell folder rebuild`.

Exit criterion: at 100,000 entries, applying a batch of 512 versions writes less than 1 MB and
holds the folder lock for under 50 ms; with one folder's database corrupted, `tidefelld` starts,
rebuilds that folder with its recycle bin and history intact, and serves the others throughout;
and killing `tidefelld` during a first sync loses nothing when it restarts.

### Phase 5k work breakdown

Robustness (ADR 0035, ADR 0036, ADR 0037). Step 2 is urgent; the rest follow Phase 5j, whose
databases they build on.

1. **Decisions.** ADRs 0035, 0036 and 0037 and this breakdown.
2. **Folder roots.** The `.tidefell/folder` marker, the `unavailable` state, no root created
   outside create and join, and the paused mass delete. A test unmounts a folder's directory and
   checks that no delete leaves the device.
3. **Disk space and errors.** Free-space floors per volume, writes that wait above the floor,
   failed metadata writes that stop applying changes, and classified file errors, all shown as
   problems.
4. **Memory.** Records on disk behind the cache, index batches read from the database, the
   synthetic-folder generator and the nightly memory budgets.
5. **Simulation.** The transport seam, the in-memory transport, the harness with its operations
   and invariants, seed shrinking, 200 seeds per pull request and 10,000 nightly.

Exit criterion: removing a folder's drive while `tidefelld` runs, and starting it before the
drive mounts, deletes nothing anywhere; a full disk pauses downloads and resumes them when space
returns; a synthetic full replica of 1,000,000 files and 10 TiB stays under 300 MB of resident
memory; and 10,000 simulation seeds pass.

### Phase 5l work breakdown

Security and battery (ADR 0038, ADR 0039).

1. **Decisions.** ADRs 0038 and 0039 and this breakdown.
2. **Threat model.** `docs/security/threat-model.md`, and a pull request template that asks
   whether a change affects it.
3. **Device revocation.** Its ADR, before Phase 6.
4. **Android keys.** The device key wrapped with an Android Keystore key, with existing keys
   converted on upgrade, in `tidefell/android`.
5. **Battery.** The scripted benchmark on the emulator and a device, the app's counters in the
   diagnostics bundle, budgets from the first results, then the changes the benchmark supports.

Exit criterion: the threat model covers every adversary in ADR 0038; a copy of the Android app's
storage does not contain a usable device key; and a release's notes include its battery figures.

### Phase 6 work breakdown

1. **Decisions.** ADR 0023, in the Phase 5b decisions pull request.
2. **Specs.** sync/2, blobs/2, the encryption key, key grant and sealed version formats, and
   the key derivations, each with golden vectors, merged before any code.
3. **`crypto` crate.** Folder key derivations, chunk and metadata sealing and HPKE grants,
   checked against the vectors.
4. **Protocol.** v2 messages in `proto` and sync/2 and blobs/2 in `net`, alongside v1.
5. **Encrypted folders on members.** Creating and joining, grants for new devices, rotation
   when a member is removed, and decryption into the folder directory. CLI and API support.
6. **Storage mode.** `[storage]` in `tidefelld`: stored folders with members and quotas,
   version retention and chunk collection, operator commands.
7. **Infra.** `role-storage-peer`, and a VM test of the exit criterion that also checks the
   storage peer's disk for plaintext.
8. **Docs, site and release.** Guide pages for encrypted folders and running a storage peer.
   Release `v0.10.0`, and the infra, site and Android pins.
