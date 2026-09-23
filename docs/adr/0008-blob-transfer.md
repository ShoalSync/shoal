# 0008: Own chunk transfer protocol on the Tidefell block store

- Status: accepted
- Date: 2026-09-23

## Context

The original design named iroh-blobs for content transfer. iroh-blobs brings its own storage
(a redb database with BLAKE3 outboards), its own downloader and its own ALPN. Tidefell already
has a block store (`store`) with verified reads, availability bitmaps and LRU eviction, and plans
its own multi-peer scheduler (`fetch`). Chunks are at most 1 MiB.

## Decision

`tidefell/blobs/1` is a small request/response protocol over iroh QUIC streams, served from the
`store` crate. A chunk is requested by its BLAKE3 hash and verified by hashing the received
bytes before it is stored. iroh-blobs is not a dependency.

## Consequences

- One storage layout and one eviction policy.
- Verification is per whole chunk rather than streaming; with chunks of at most 1 MiB this costs
  at most 1 MiB of buffered, unverified data per in-flight request.
- Chunk hashes remain plain BLAKE3 and so stay compatible with iroh-blobs if we ever want to
  interoperate or switch.
