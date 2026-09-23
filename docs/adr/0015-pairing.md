# 0015: Devices join a user by pairing over the network

- Status: accepted
- Date: 2026-09-23

## Context

A new device needs a device certificate signed by its user's key (ADR 0006, glossary). On a
server this is done with `tidefelld certify` and a copied file. On a phone that is clumsy, and
copying the user key to the phone would put the key that can add devices on the device most
likely to be lost.

## Decision

A new ALPN, `tidefell/pair/1`, specified in `specs/pair-v1.md`:

1. The new device, not yet certified, starts a pairing-only endpoint and shows a **pairing code**:
   its device ID, a random 16-byte secret, and how to reach it (relay URL and direct addresses).
   On a phone it is a QR code; it can also be typed or pasted as text.
2. A device that holds the user key (the **approver**, e.g. `tidefell pair <code>` against a
   daemon whose data directory has `user.key`) connects to the device ID in the code. iroh's
   TLS authenticates the new device's key, so the approver knows it reached the device in the
   code.
3. The approver proves it has seen the code by sending a MAC over both device IDs keyed with the
   secret. The new device rejects the connection otherwise.
4. Both sides display a six-digit **confirmation code** derived from the secret and both device
   IDs, and the approver asks the person to confirm that they match and to name the device.
5. The approver signs a device certificate for the new device and sends it, with its own
   address and the folders the user is a member of on the approver. The new device stores the
   certificate, restarts as a normal peer, keeps the approver connected, and can join those
   folders.

The pairing code expires after ten minutes or one successful pairing.

## Consequences

- User keys stay on the machines where they were created; the phone never holds one.
- An approver must be online and reachable during pairing; relays make that the normal case.
- `tidefelld certify` and certificate files remain for headless servers.
