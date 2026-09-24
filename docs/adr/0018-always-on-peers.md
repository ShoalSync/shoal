# 0018: Always-on peers without a central server

- Status: accepted
- Date: 2026-09-24

## Context

Peer-to-peer sync only works while two peers are online at the same time. A phone and a laptop
that are rarely awake together sync slowly or not at all. A NAS or home server fixes this for
people who have one, as a full replica that is always online.

The plan's Phase 5 called for a "hosted web peer and full-replica storage in infra": a server
run by the project that holds users' files and serves them in a browser. That conflicts with
Tidefell's promise that files live only on machines the user chooses. Every replica today
stores plaintext, so a hosted replica would give its operator every user's files, and a web
UI needs plaintext on the server to render and serve them.

The need is real, though. Someone without a NAS should be able to have an always-on peer.

## Options

**A. An encrypted storage peer.** A new kind of peer that stores and serves a folder's
content without being able to read it:

- Member devices encrypt content chunks with keys derived from a per-folder key. Chunk IDs
  the storage peer sees are keyed hashes, so it cannot test whether a folder contains a
  known file.
- Entry metadata (paths, sizes, times) is encrypted. The storage peer sees opaque signed
  envelopes, sequence numbers and version vectors, and it stores and forwards them.
  Conflict resolution stays on member devices.
- The folder key is wrapped for each member device's key and distributed as signed grants
  through sync, so new devices receive it from existing ones. User keys still never leave
  their machine (ADR 0015).
- A storage peer is certified as storage-only, so it can never be given the folder key.

Anyone could run one, including the project, a friend or a commercial provider, without
being trusted with content. It needs a new protocol version (sync/2 and blobs/2) with specs
and vectors. Deduplication then works only within a folder. The web peer cannot run on
such a replica.

**B. A self-run always-on peer.** No protocol change. The infra NixOS modules and the docs
make it easy to run `tidefelld` as a full replica, with the web peer optional, on a server
the user controls, such as a small VPS. It is a NAS that happens to live in a data centre:
trusted, plaintext, the user's own machine. The project runs no storage or web service.

**C. B now, A later.** Phase 5 ships B and removes the hosted service from the plan. A
becomes its own phase with a specs-first design, because it changes the wire protocol.

## Decision

Option C. Phase 5 ships B, and the project runs no storage or web service. A is a later phase
with a specs-first design. C gives people without a NAS an always-on peer in weeks rather than
months, keeps the no-central-server promise true today, and leaves A as a clean addition that
neither B nor the web peer blocks.

## Consequences

- The site and docs stop implying a project-run service. The plan's Phase 5 exit criterion
  is a self-run always-on peer deployed from pinned artifacts.
- A later phase adds encrypted replicas, and folders choose between plaintext replicas (with
  web access) and encrypted ones (storage only).
