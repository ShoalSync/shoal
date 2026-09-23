# 0006: postcard wire encoding and Ed25519 signed envelopes

- Status: accepted
- Date: 2026-09-23

## Context

Peers exchange metadata messages that must be compact, deterministic (so signatures cover a
canonical byte string) and implementable outside Rust.

## Decision

- Messages are serialised with [postcard](https://github.com/jamesmunns/postcard) 1.x, whose wire
  format is specified and stable.
- Every message on a sync stream is framed as a varint length followed by the postcard bytes.
- Messages that must be attributable are wrapped in a signed envelope: the Ed25519 signature
  covers a domain-separation tag, the message kind and the postcard-encoded body.
- Golden encodings live in `specs/vectors/` and are checked by tests.

## Consequences

- Changing field order or types in a wire struct is a breaking change: new ALPN version.
- Non-Rust implementations must implement postcard's format, which is short and documented.
