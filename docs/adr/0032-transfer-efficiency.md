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
- Step 3 decided:
  - A pass writes up to 8 files at once, in plan order. A write waits for every earlier one
    that touches the same path, or a path inside or above one of its own, counting the
    previous path of a moved file.
  - The 64 MiB is counted by the sizes of the files being written; a larger file is written
    on its own.
  - Files are assembled into their temporary file before the folder's file lock is taken, so
    the lock is held only to put the file in place, and opening a placeholder waits for no
    download or assembly. A file assembled while a file it reads chunks from was being
    replaced fails its hash; it is assembled again under the lock before its manifest is
    dropped.
  - With 100 ms per request, the 1,000 small files take 38.8 s instead of 308 s; the 256 MiB
    file takes 8.7 s.
- Step 4 decided:
  - The shortcut covers files under 64 KiB, the empty file included, which has no chunks.
    The one-chunk manifest is recorded before the fetch, so this device serves the chunk, and
    compaction keeps it, like any other.
  - If the chunk cannot be fetched, the guessed manifest is dropped and peers are asked for
    theirs; when it differs, its chunks are fetched instead.
  - With 100 ms per request, the 1,000 small files take 26.3 s, at 2 streams each; the
    256 MiB file takes 8.6 s.
- Step 5 decided:
  - The first chunk request for a peer waits 2 ms for others; those for the same peer and
    folder then go as one request of up to 64 distinct chunks, and each download gets the
    answers for its own. A busy answer ends each download's answers where the peer stopped.
  - A peer's answer to whether it is a full replica is kept for ten minutes, and refreshed
    whenever copies are confirmed; one that cannot say counts as partial. It is only asked
    when more than one peer could send a file.
  - A peer assumed to hold everything that answers `Missing` is asked for its availability,
    then treated like any other.
  - With 100 ms per request, the 1,000 small files take 14.1 s, at 0.21 streams each, 22 times
    faster than the baseline; the 256 MiB file takes 8.4 s, without an availability query.
    That meets the exit criterion.

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

## Amendment: requests under way together, and encrypted downloads alike (2026-10-22)

Measured with `transfer.rs`, release build, two engines on localhost, a 256 MiB file, its rate
after the sender's scan; a fill (as of a Windows placeholder) on a partial replica. The test
now also measures the file in an encrypted folder, and a fill (`measure_a_fill`).

- **Requests to a peer overlap.** A download kept 16 chunks in flight per peer, in requests of
  8, and the batching above joined those two into one request: each round trip carried 16
  chunks, and the next set waited for it, so with 100 ms per request the 256 MiB file took
  8.4 s, about one request at a time. Now:
  - a download keeps up to 64 chunks, and at most 16 MiB by their lengths, in flight to each
    peer (one larger chunk still goes alone), in requests of 8;
  - batching never joins two requests of one download: it still joins different downloads'
    requests, as of many small files, into one of up to 64 chunks, and sends the rest
    alongside;
  - a test with paused time checks that a download has several requests to one peer under way
    at once.
- **Encrypted downloads use the same scheduler.** They asked the peers sharing the folder one
  after another, in no set order, taking everything the first had and asking another only for
  the rest. Now a blobs/2 source gives the scheduler what each peer holds (its availability
  request, which members and storage peers both answer) and the sealed chunks it sends, opened
  and checked against their IDs; a chunk's ID stands in for its hash, which only opening it
  tells. So they ask every peer what it holds, spread their chunks across peers, back off busy
  ones and stop when paused, as unencrypted ones do. A test checks a member and a storage peer
  holding the same file each send part of it. Content is marked unobtainable (ADR 0040) only if
  some peer answered, as before.
- **A fill fetches its next window while it hands over the current one**, so at most two
  windows of about 16 MiB are held, rather than one at a time with the download standing still
  at the end of each (ADR 0042's amendment).

| 256 MiB, MiB/s | 0 ms before | 0 ms after | 100 ms before | 100 ms after |
|---|---|---|---|---|
| Plain, written | 90 | 126 | 32 | 81 |
| Encrypted, written | 48 | 112 | 39 | 78 |
| Plain, filled | 144 | 188 | 69 | 113 |
| Encrypted, filled | 129 | 178 | 68 | 124 |

The 1,000 small files take as long as before (1.5 s; 14 s with 100 ms per request).

Peers are still chosen by the fewest chunks in flight, not by measured speed: a faster peer
answers sooner and is handed more, but each peer has the same limit, and the last chunks of a
download are not asked of a second peer when the first is slow. Both are left for later.


## Amendment: a stalled peer, and one that struggles (2026-10-24)

Choosing the holder with the fewest chunks in flight already favours a faster peer. Two gaps
remained: a peer that stops answering held up the end of a download until its requests failed,
and one that kept failing or answered busy was given as much as before once it was usable again.

- **Hedging, in the endgame only.** Once every chunk still needed is in flight, one outstanding
  for `hedge_after` (2 s) is asked of one more holder with room, oldest first, and the first
  copy to arrive is taken; the other only frees its peer, and is not stored twice. Before the
  endgame nothing is asked twice: a chunk waiting for room is a better use of it.
- **A window per peer.** Each peer may have at most its window of chunks in flight, under
  `max_in_flight_per_peer`: it starts there, halves when the peer fails or answers busy, and
  grows by one for each chunk it sends. A peer that is fine keeps what it had.

Measured as before, against main on the same machine at the same time, transfers are unchanged.
