# 0009: REST/JSON API with generated OpenAPI; no gRPC for now

- Status: accepted
- Date: 2026-09-23

## Context

The plan called for REST and gRPC in the `api` crate. The CLI and the web peer are the only
consumers in Phase 3. Android embeds the engine through FFI (ADR 0004) rather than calling an
API. gRPC would add `protoc` and a second API surface to CI, Nix and releases.

## Decision

- `api` serves REST with JSON bodies under `/v1`, built on axum. Live changes are streamed as
  server-sent events.
- Request and response types live in `sdk` (MIT OR Apache-2.0) so third-party clients can use
  them; `api` depends on `sdk`, never the reverse.
- The OpenAPI document is generated from those types with `utoipa`, served at
  `/v1/openapi.json`, and published with each release. It is never written by hand.
- gRPC is deferred until a consumer needs it; it would be added in a new ADR.

## Consequences

- One API surface, with a machine-readable schema that CI keeps in step with the code.
- Streaming uses SSE rather than gRPC streams.
