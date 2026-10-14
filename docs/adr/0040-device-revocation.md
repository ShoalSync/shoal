# 0040: Device revocation

- Status: accepted
- Date: 2026-10-14

## Context

A device certificate is valid forever (ADR 0015). A lost or stolen phone keeps syncing every
folder its user is a member of, and the only way to stop it today is to remove the whole user
from every folder on every replica (`docs/security/threat-model.md`). ADR 0038 decides a
revocation list signed by the user key, synced with folders, which peers check before accepting
a device's versions or serving it, with its own ADR before Phase 6. This is that ADR.

Three facts shape it:

- **Versions are signed by their author's device key, which the thief holds.** Anything the
  stolen device signs can say whatever the thief likes, including its vector counters and
  modification time. A rule that keeps "the versions it made before it was stolen" cannot rest on
  numbers it signs: the thief can sign new versions with old numbers.
- **Counters are per entry.** A version's vector counts changes to that entry, not a device's
  changes overall, so there is no single number to cut a device off at.
- **Validity must not depend on arrival order.** If replicas that applied a version before the
  revocation keep it while the others refuse it, they disagree for good. ADR 0024's rule for
  removed members ("versions received later are refused; versions already applied stay") has
  exactly this flaw.

## Decision

### Revocations

- A revocation is `Signed<DeviceRevocation>` with kind `identity/1/device-revocation`:
  `user: UserId, device: DeviceId, revoked_ms: i64`, signed by the user key. It is permanent: a
  device that is found again re-pairs, with a new device key.
- A device holding the user key revokes: `tidefell device revoke <device>`, with the API and the
  web peer's admin pages. The machine holding the user key is the one that pairs devices
  (ADR 0015), so it knows the user's devices by name.
- Every device keeps every revocation it has verified, in its runtime settings. The set only
  grows, so copies merge by union and an old copy cannot undo a revocation.
- Revocations spread to every peer, whatever folders they share: after the `sync/1` handshake
  each side sends the revocations the other has not seen (a new message pair, `Revocations`).
  A device that shares no folder with the revoking machine still hears of it through the
  devices it does share one with.

### Connections

- A revoked device's certificate fails the handshake on every peer that knows of the revocation:
  it is not served indexes, manifests or chunks, and its pairing approvals are refused.
- A device that learns its own certificate is revoked stops syncing and says so; the app asks
  the user to pair again.

### Versions: takeover (recommended)

- **A version authored by a revoked device is invalid, everywhere.** Validity depends only on the
  version and the revocation set, never on what arrived first, so replicas cannot end up
  disagreeing.
- **Before dropping them, each replica takes over the revoked device's current versions.** For
  every entry whose current version, or open conflict copy, is by the revoked device, the replica
  makes a change of its own with the same path and content, building on that version. Several
  replicas taking over the same entry make concurrent versions with the same content, which
  already need no decision (they are settled). The files stay where they are, now attributed to
  the replicas that kept them.
- **What the thief changed before the revocation reached a replica stays** where that replica had
  applied it, as with any member's change, and earlier versions are in the recycle bin and
  history. What it sends afterwards is refused.
- A version by the revoked device that no replica holds a current copy of is simply refused.

Takeover is recommended over the alternatives:

- **Refuse what arrives later, keep what was applied** (ADR 0024's rule for removed members):
  simplest, but replicas that applied a version and replicas that refused it disagree forever,
  and the simulation's convergence check (ADR 0036) fails as soon as revocation is in it.
- **A list of the versions to keep** (their IDs, in the revocation): exact and unforgeable, but
  only the machine with the user key can sign it, and it lists only versions of folders that
  machine replicates; a folder it does not hold would lose every file the lost device made.

### Order of work

- Refusing a revoked device's connections needs only revocations and the handshake check, and
  ships first: it is most of the protection, since a thief with a revoked phone can then reach no
  replica that has heard of it.
- Takeover and refusing relayed versions sit in the same check as author vouching (ADR 0024,
  Phase 5c step 6), which is not built yet, and ship with it.
- ADR 0024's rule for removed members changes the same way: their devices' versions are judged by
  the current member list, not by when they arrived, and a removed member's current versions are
  taken over like a revoked device's.

### Out of scope

- **A lost user key.** Revocation needs the user key; whoever has it can certify and revoke
  devices. Recovering from a lost or stolen user key means rotating it, which is a separate
  decision.
- **Member lists that sync.** Removing a member still has to be done on every replica.

## Consequences

- A lost device can be cut off from one machine, and the cut reaches every replica that talks to
  any other.
- Files the lost device made stay, kept by the replicas that hold them, and the folder converges.
- Revoking adds versions: one per entry whose current version was by the revoked device, on each
  replica that holds it. A phone that made many files makes many takeovers; they carry no content
  and settle without conflicts.
- Changes the thief made before the revocation spread are kept like any member's change; the
  recycle bin and history are the way back.
