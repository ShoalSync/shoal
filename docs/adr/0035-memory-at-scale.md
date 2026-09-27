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

## Consequences

- A small NAS can hold a large full replica.
- Serving a chunk not in the cache costs a database read before the file read.
- Memory becomes a tested budget instead of a side effect.
