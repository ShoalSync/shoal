# 0035: Memory at scale

- Status: accepted
- Date: 2026-09-27

## Context

The engine holds every folder's metadata in memory: every signed version, every manifest with
its full chunk list, the chunk index, and the maps of files on disk. That is small for a phone
folder and large for a full replica. At 256 KiB per chunk, 10 TiB is about 40 million chunks;
at 36 B per chunk in manifests, about 1.4 GB, and the chunk index roughly doubles it. The
versions of 1,000,000 files add about 360 MB. ADR 0027 needs manifests kept, because they are
how a device serves chunks from files on disk. Many NAS devices, the always-on peers of
ADR 0018, have 1 to 4 GB of memory.

## Decision

- With the per-folder databases of ADR 0034, manifests, the chunk index, signed version bodies,
  the recycle bin and history stay on disk and are read when needed, through a cache of recently
  used records (default 64 MiB per device, `cache_mb` in the configuration).
- In memory stay what every pass needs: each live entry's path, current content and version
  vector, and the layout that decides where entries go on disk (ADR 0024, ADR 0033).
- Index batches for peers are read from the database in sequence order, not built in memory.
- **Targets**, measured on a synthetic full replica: 1,000,000 files and 10 TiB of recorded
  content stay under 300 MB of resident memory with the default cache, and a phone folder of
  50,000 photos under 60 MB.
- A generator builds such folders from metadata alone, with sparse files, so the measurement
  needs no real data. It runs in the nightly job of ADR 0031 and fails if a target is exceeded.

## Implementation notes

- Step 4, first part (measurement and the largest costs):
  - `tidefell_engine::synth` writes a folder's metadata directly beside sparse files that
    match it, and `cargo run --release -p tidefell-engine --example memory -- run <files>
    <bytes>` measures a separate engine process on it: resident memory after it opens the
    folder, after a full scan, after status reads, and idle. The `Memory` workflow reports it
    nightly for 100,000 files of 1 TiB and a phone folder of 50,000 photos, and for 1,000,000
    files of 10 TiB when dispatched; it does not fail yet.
  - Manifests and the chunk index are on disk, read through a small cache, with what changed
    since the last confirmed commit kept in memory so it reads back at once. Chunks no longer
    cost memory: 100,000 files of 1 TiB went from 3.4 GB to under 200 MB.
  - Signed versions are read from the database to serve peers, and kept in memory only until
    saved. Version vectors are sorted vectors, encoded exactly as before. Local records share
    one copy of each path with their index by content. Scans compare files with the records
    under the lock instead of copying them all, and group names only on drives that fold
    them. The layout keeps only the paths of entries written elsewhere than their own. Each
    folder's database caches 4 MiB.
  - Measured, release build, glibc's allocator: 100,000 small files take 189 MB after a scan
    (786 MB before), about 1.5 KB per file, of which some 35 MB is memory freed but kept by the
    allocator; a phone folder of 50,000 photos takes 117 MB. Both are over the targets, which
    allow about 300 bytes per file. What remains per file is each live entry's version with its
    folder ID and author in full and the hash map around it (about 450 bytes), the local
    record and its map node (about 300 bytes), and the allocator's slack.
- Step 4, second part (towards 300 bytes a file):
  - The full-size measurement, 1,000,000 files of 10 TiB, before it: 1.16 GB after opening,
    3.2 GB after a scan and idle, and 2.4 s for the first status read.
  - Every program running the engine uses mimalloc (`tidefell_engine::Allocator`), which
    returns freed memory and keeps less aside than glibc's allocator; the measurement uses it
    too, so it measures what ships. 100,000 files idle at 145 MB instead of 194 MB, and
    1,000,000 at 1.39 GB instead of 3.2 GB, though a first scan of those still peaks at 3 GB.
  - Each entry's current version is kept compactly: the folder ID once for the folder, devices
    numbered, and a vector of one device's changes as two numbers. Versions are handed out as
    views and made in full only on request; development builds check each comes back exactly.
    100,000 files idle at 130 MB.
  - Measured with `dhat`, what remains per file at idle is about 815 bytes of heap: the hash
    map of entries (about 300, mostly empty slots), the local records' B-tree (about 270),
    the index of local records by content (about 90), the layout (about 55), and paths and
    vectors held twice. The redb page cache and the network's buffers are fixed costs.
  - Entries are kept densely: side by side in one vector, found through a table of positions,
    with their rare conflict copies boxed. Local records likewise, indexed by path, content and
    entry, each holding its base vector as two numbers and its path shared with its entry's
    version. The layout is made without a copy of each path, and finds clashes as it fills its
    table of places; a property test checks it against the layout as it was made before.
  - mimalloc is started with `MIMALLOC_PURGE_DELAY=0` and `MIMALLOC_ARENA_EAGER_COMMIT=0`
    (`tidefell_engine::ALLOCATOR_SETTINGS`): by default it returns freed memory only when it
    next runs, which an idle engine may not do for hours. These are read when a program starts,
    so `tidefelld` starts itself again with them if they are missing; the Android app sets them
    before it loads the engine.
  - Temporary costs that grew with the folder: the files made here but not yet confirmed on a
    full replica are found without reading every manifest (a first scan of 1,000,000 files
    peaked at 2.2 GB for it), and the sync plan and file list find files by entry without a map
    of every file.
  - A database over 1 GiB is checked a minute after it opens rather than as it opens: checking
    10 TiB of chunk index took 94 s, so the folder took over 100 s to start. A failed check
    opens it again, checking it whatever its size, as the daily check does (ADR 0034).
  - Measured: 1,000 files 17 MB (the fixed cost); a phone folder of 50,000 photos 40 MB, within
    its target; 100,000 files 55 MB, about 385 bytes a file; 1,000,000 files of 10 TiB 444 MB
    idle and 517 MB at most, opening in 9.5 s. The heap at 1,000,000 is about 350 bytes a file:
    the entry (152), the local record (104), its path (40, once) and the tables that find them
    (52). That is over the target of 300 MB: getting under it means not keeping in memory what
    a local record repeats of its entry's version, or each version's copy ID.

## Consequences

- A small NAS can hold a large full replica.
- Serving a chunk not in the cache costs a database read before the file read.
- Memory becomes a tested budget instead of a side effect.
