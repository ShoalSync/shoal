# 0036: Simulation testing

- Status: accepted
- Date: 2026-09-27

## Context

Unit tests and the property tests planned in ADR 0031 check pieces in isolation, `meta`'s
convergence included. The bugs that mattered were found between pieces, late: a displaced entry
overwritten before it was moved aside (found as a flaky test), conflict copies superseded by the
next edit without anyone deciding (ADR 0029 amendment), and deletes expiring by another
device's clock (#46). Each needed a particular order of events across devices that no test had
written down.

## Decision

- A simulation harness runs several engines in one process, over an in-memory transport in
  place of iroh, each with its own clock and folder directory. A seed drives everything random:
  which device does what, message delays and order, and when things fail.
- **Operations:** create, edit, rename, move and delete files and directories; edit the same
  file on several devices; partition and heal the network; restart a device; lose a device's
  metadata (ADR 0034); skew or jump a device's clock; fill a device's disk (ADR 0037); change
  sync options and membership.
- **Invariants,** checked once the network heals and all devices are idle:
  - every full replica holds the same live files and index;
  - no content is lost: every version ever made is live, a conflict copy, or in some device's
    recycle bin or history within its retention;
  - nothing is overwritten without being kept first (ADR 0027);
  - no engine panics or stalls.
- A failure prints its seed and the operations that led to it, shortened automatically to the
  fewest that still fail. Each seed that found a bug becomes a regression test.
- CI runs 200 seeds per pull request; the nightly job of ADR 0031 runs 10,000.
- The engine gains a transport seam so the in-memory transport can replace iroh in tests; this
  is the main cost. The network simulation may use `turmoil` or be built on that seam alone.

## Implementation notes

- The seam is in `net`: the protocols use a connection that is iroh's or, with the `simulation`
  feature, a link on `net::Network`, an in-memory network that refuses dials across a partition
  and fails what was in flight when one starts, never as a clean end of stream. Engines join it
  with `Engine::start_simulated`; `turmoil` was not used, since the engines' own threads, files
  and databases are real and its control of time would not reach them.
- The harness is the `sim` crate. A seed chooses 40 steps for three devices: files written,
  renamed and deleted, folders deleted, scans, partitions and heals, restarts, clocks skewed or
  jumped by up to two days (`Engine::set_clock_offset`, one clock per engine), metadata lost and
  rebuilt, a full disk, send-only and both ways, members added and removed. Files and devices are
  picked by number, wrapped to what exists, so a shortened list of steps still runs.
- Once settled, the network healed, disks freed and every device syncing both ways and scanned,
  every device must have the same files and index, and no engine may have failed or stalled. No
  content is lost is checked per device: whatever a device had in its folder, unless its user
  removed it there, must still be on that device, as a file or kept in its reserved directory.
- Timing is real, so a seed fails only sometimes, more often under load. The harness tries a
  seed again (`TIDEFELL_SIM_REPEAT`) and shortens a failing one by running eight copies of each
  candidate at once. Seeds that found bugs are kept as lists of steps in
  `crates/sim/tests/regressions/`. CI runs 200 seeds on each pull request, the Nix checks 8, and
  the nightly Simulation workflow 10,000, a new range each night.
- Metadata is lost only once the devices agree: a version a device made and sent to nobody is
  lost with its metadata, and if its user had overwritten the only copy of the version before
  it, no device can ever have that one. It stays unavailable, a problem the folder shows, rather
  than a stall the harness could tell from a bug.
- What it found and the fixes, before this ADR's step was done:
  - A file deleted before a scan saw it, while a peer gave its path to another entry or moved
    it, was not recorded as deleted: its entry stayed, with its content nowhere.
  - A delete planned from older state removed a file edited and scanned since.
  - A deleted version an edit made apart from the delete brought back, perhaps to be deleted
    again, was dropped from the recycle bin's kept files instead of becoming a previous
    version; and a delete applied to an older version than the one deleted lost that version.
    Kept files are now named with their entry, so they can be.
  - An engine stopped while a task held its network node kept answering peers from state no
    longer kept: the node is now closed whatever holds it.
  - A file written by the engine was recorded with what was on disk after, so a user's save in
    between was taken for the engine's file and never synced.
  - A chunk the records said a file held, and which was not there, made the write fail until
    the next scan: such a chunk is now fetched instead.
  - A file moved over another of the same size and modification time, to the millisecond, as
    files the engine writes can be, was taken for the one it replaced: inodes are compared too,
    kept while the engine runs.
  - The old path of a file the engine moved aside was tidied after another entry's file had
    taken it, losing that file's record.

## Consequences

- Cross-device orderings are explored systematically instead of found by users.
- The transport seam also makes other engine tests faster and more deterministic.
- Nightly runs use more runner time; seeds that fail are kept, so it is spent once per bug.
