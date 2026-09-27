# 0038: Threat model

- Status: accepted
- Date: 2026-09-27

## Context

Security decisions are spread across ADRs: device certificates and pairing (ADR 0015), web
accounts and share links (ADR 0011, ADR 0019), trust between members (ADR 0024), encrypted
storage peers (ADR 0023). Phase 5c step 6 (hardening) and Phase 6 need one statement of what is
protected, from whom, and what is not.

What exists today includes: QUIC with TLS between endpoints whose IDs are device keys; device
certificates signed by the user key; per-folder member lists; signed versions; key files and the
control socket readable only by their owner; Android app data excluded from backups; argon2
passwords and 128-bit share-link tokens in the web peer; pairing codes that expire and stop
after repeated failures.

Gaps found while checking:

- **Device keys on Android** are files in app storage, not protected by the Android Keystore.
- **No way to revoke a device.** A lost phone keeps a valid certificate; certificate revocation
  is an open question in the sync/1 spec.
- **The relay** sees which devices talk to each other, when, and how much, though not what.
- **Removing a member** stops future changes but cannot take back what they already have
  (ADR 0024).

## Decision

- `docs/security/threat-model.md` states the assets (file contents, paths and metadata, user and
  device keys, web accounts), the adversaries (network attacker, relay operator, member,
  removed member, lost or stolen device, share-link holder, other local users, supply chain),
  what each can and cannot do, and the protection for each. Every ADR that touches these adds
  to it, and a new pull request template asks whether a change does.
- **Device revocation** gets its own ADR before Phase 6: a revocation list signed by the user key,
  synced with folders, which peers check before accepting a device's versions or serving it.
- **Android keys** are wrapped with a key held in the Android Keystore, so copying app storage
  does not yield the device key.
- **Relay metadata** is documented as visible to relay operators; users who mind run their own
  relay (infra already can).
- Phase 5c step 6 works from this document; its items are checked against it.

## Consequences

- One place to reason about security, reviewed as the system changes.
- A lost device can be cut off once revocation lands.
- Some gaps stay by design and are written down instead of implied.
