# 0032: Transfer efficiency

- Status: accepted
- Date: 2026-09-27

## Context

The wire formats add little to file content. Measured with `tidefell-proto` on its real
encodings, with 256 KiB chunks:

| Item | Size |
| --- | --- |
| `Chunk` frame header | 39 B per chunk |
| `Request` | about 32 B per requested chunk |
| blobs `Hello`, sent on every stream (one request of up to 8 chunks) | 181 B |
| Manifest, once per file | 32 B plus 36 B per chunk: 0.013% of the file |
| `Availability` for a 4 GiB file | 2 KB |
| Signed `EntryVersion` | 265 to 400 B, 361 B each in a batch of 512 |

That is about 95 B per chunk, 0.04% of the content, well below QUIC's own packet overhead of
roughly 3 to 4%. Content-defined chunks already avoid resending unchanged or duplicate data.

Three costs are not small:

- **Round trips.** `materialize` writes one file at a time, and each file costs at least three
  round trips in sequence: its manifest, an availability query to each candidate peer, and its
  chunk requests. Over a relayed path with 100 ms round trips, 10,000 small photos take at least
  50 minutes whatever the bandwidth. Within one large file it is efficient: up to 16 requests of
  8 chunks per peer are in flight, from several peers at once.
- **Small files.** A 2 KB file costs about 0.7 KB of protocol: its version, a manifest request
  and response, a blobs `Hello`, a request and a chunk frame.
- **Index size.** Every signed version carries `folder`, which its batch already names, and
  `author`, which must equal the signer: 64 B of about 360, 18% of the index. A folder of
  100,000 files sends about 36 MB of index on its first sync.

## Decision

The engine removes the serial round trips without changing sync/1 or blobs/1. The protocol
changes wait for sync/2 and blobs/2 (ADR 0023).

- **Several files at once.** `materialize` writes up to 8 files concurrently, and keeps at most
  64 MiB of chunks in flight across them. Writes whose paths, or previous paths in a rename,
  overlap still run in order, and each file is still moved into place under the folder's file
  lock. Opening a placeholder (ADR 0016) is not held behind background writes.
- **No manifest for single-chunk files.** A file smaller than the reference chunker's minimum
  chunk (64 KiB) is one chunk, whose hash equals the file's content hash. The engine requests
  that chunk directly, verifies it, and records the one-chunk manifest the reference chunker
  would have produced, so it can serve and report availability for it like any other. If a peer
  answers `Missing`, because it split the file differently, the engine falls back to fetching
  the manifest.
- **Small files share requests.** Chunk requests for files being written at the same time are
  batched into one `Request` per peer, up to the 64 chunks blobs/1 allows, so many small files
  cost one stream instead of one each.
- **No availability query to full replicas.** A peer that reports itself a full replica
  (`ReplicaRequest`) holds every current file, so the engine requests chunks from it directly and
  only queries availability if it answers `Missing`. With a single candidate peer, the query is
  skipped as well.
- **Measured.** A test syncs 1,000 files of 2 KB and one file of 256 MiB between two daemons,
  counting streams and requests per file and adding a fixed delay to every request. It records
  the numbers before and after each change in its pull request.
- Step 2 decided:
  - The test is `crates/engine/tests/transfer.rs`, between two engines on localhost, and runs
    only when asked (`cargo test --release -p tidefell-engine --test transfer -- --ignored`).
    `Engine::set_request_delay` adds the delay before every request the device sends: manifest,
    availability, replica and chunk requests. It is zero unless set.
  - Requests are counted by kind, and the metrics show them as `tidefell_requests_total` with
    `kind` (`manifest`, `availability`, `replica`, `chunks`) and `tidefell_chunks_requested_total`.
    Each request opens a stream.
  - Baseline, 100 ms per request: 1,000 files of 2 KB take 308 s, at 3 streams each (one
    manifest, one availability query and one chunk request); the 256 MiB file takes 8.8 s, in
    105 chunk requests for its 834 chunks, plus one manifest and one availability query.

### For sync/2 and blobs/2

Recorded here so Phase 6's protocol versions pick them up:

- a signed version drops `folder` and `author`, which the batch and the signer already give;
- blobs authenticates once per connection instead of sending the certificate on every stream.

### Not adopted

- **Compressing index batches.** Hashes and signatures, most of each version, do not compress.
- **Smaller chunks.** They would cut resent data on small edits but add per-chunk overhead and
  manifest size for every file; 256 KiB on average stays.

## Consequences

- A folder of many small files syncs in about one round trip per batch of files instead of
  three per file, which matters most on relayed and mobile links.
- More writes run at once, so the engine holds more partial downloads; the 64 MiB limit and the
  per-peer request limit bound it.
- The single-chunk shortcut depends on the reference chunker's minimum size. Peers with other
  parameters still interoperate through the fallback.
- No wire format changes; sync/1 and blobs/1 peers are unaffected.
