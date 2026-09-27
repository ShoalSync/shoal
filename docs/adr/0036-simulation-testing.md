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

## Consequences

- Cross-device orderings are explored systematically instead of found by users.
- The transport seam also makes other engine tests faster and more deterministic.
- Nightly runs use more runner time; seeds that fail are kept, so it is spent once per bug.
