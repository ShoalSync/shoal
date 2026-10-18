# Pairing protocol, version 2

ALPN: `tidefell/pair/2`

Status: draft. Messages are implemented in `crates/proto` (`pair` module) and the key
derivations in `crates/identity` (`pairing` module); golden encodings are in
[`vectors/pair-v2.json`](vectors/pair-v2.json). See ADR 0015.

Version 2 replaces version 1, before 1.0 and so without keeping it (ADR 0026): a folder offer
says whether the folder is encrypted and who its storage peers are (ADR 0023), so the new
device joins it as such. The derivation context `"tidefell pair/1 secret"` is unchanged: it is
a label, not the protocol's version.

## Purpose

Give a new device a device certificate from a device that holds the user key (the
**approver**), without copying the user key or a certificate file by hand.

## Pairing code

The new device, which has a device key but no certificate yet, serves only this ALPN and shows a
pairing code:

| Field | Type | Meaning |
| --- | --- | --- |
| `device` | `DeviceId` | The new device's ID (its iroh endpoint ID). |
| `secret` | 16 bytes | Random, generated for this code. |
| `relay` | `Option<String>` | Relay URL the new device is reachable through. |
| `addrs` | `Vec<SocketAddr>` | Its direct addresses. |

The text form is `TFP1-` followed by the postcard encoding of these fields in unpadded
RFC 4648 base32, upper case. Readers ignore case, surrounding whitespace, and spaces or dashes
after the prefix. A QR code holds the text form.

A code is valid for ten minutes and for one successful pairing. The new device refuses
connections with an invalid proof, and stops serving the code after too many of them.

## Keys and codes

With `A` the approver's device ID and `N` the new device's (32 bytes each):

```text
key          = BLAKE3-derive-key("tidefell pair/1 secret", secret)
proof        = BLAKE3-keyed(key, "proof"   || A || N)
confirmation = LE-u32(BLAKE3-keyed(key, "confirm" || A || N)[0..4]) mod 1 000 000
```

The confirmation code is shown as six decimal digits, zero-padded.

## Transport and encoding

The approver dials `device` with ALPN `tidefell/pair/2` and opens one bidirectional stream.
iroh's TLS authenticates the new device's key, so the approver knows it reached the device in the
code; the proof shows the new device that the approver saw the code. Framing is the same as
`tidefell/sync/1`: varint length prefix, postcard body.

## Messages

| Index | Message | Fields |
| --- | --- | --- |
| 0 | `Request` | `approver: Signed<DeviceCertificate>`, `proof: [u8; 32]` |
| 1 | `Accept` | `name: String` |
| 2 | `Complete` | `certificate: Signed<DeviceCertificate>`, `relay: Option<String>`, `addrs: Vec<SocketAddr>`, `folders: Vec<FolderOffer>` |
| 3 | `Reject` | `code: PairErrorCode` |

`FolderOffer` is `id: FolderId`, `name: String`, `members: Vec<UserId>`,
`retention_days: u32` (0 keeps deleted files forever), `encryption: Option<OfferedEncryption>`.

`OfferedEncryption` is `storage_peers: Vec<DeviceId>`: present for an encrypted folder, which the
new device joins as encrypted and syncs over `sync/2` and `blobs/2`, with the approver's storage
peers for it. The new device gets the folder's keys by grant once it has joined (`sync-v2.md`).

`PairErrorCode` variants: `0 BadProof`, `1 Expired`, `2 Declined`, `3 BadCertificate`,
`4 ProtocolViolation`.

## Exchange

1. Approver → `Request`. `approver` is the approver's own certificate; its `device` must equal
   the connection's peer ID and it must verify. The new device checks `proof`; if it does not
   match, it replies `Reject(BadProof)`. If the code expired or was used, `Reject(Expired)`.
2. New device → `Accept`, with the name it suggests for itself. Both sides now show the
   confirmation code; the new device also shows the approver's name and user.
3. The approver asks the person to check that the codes match and to confirm the device's name.
   If they decline, it sends `Reject(Declined)`. Otherwise it sends `Complete` with a certificate
   for `N` signed by the approver's user, its own addresses, and the folders on the approver
   whose members include that user.
4. The new device verifies that the certificate is signed by its `user`, is for `N`, and names the
   same user as `approver`; if not, it replies `Reject(BadCertificate)`. It stores the
   certificate and closes the stream. The code is used up.

The new device then starts as a normal peer and keeps the approver connected. Joining the
offered folders is up to the person using it.

## Open questions

- Pairing with a user key held on a phone (the phone as approver).
- Revoking a device certificate.
