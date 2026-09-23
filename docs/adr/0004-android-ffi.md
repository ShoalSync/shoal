# 0004: Android runs the full peer via FFI

- Status: accepted
- Date: 2026-09-23

## Context

The Android app could either embed the Rust engine or talk to a separate daemon over the
`api` crate's gRPC/REST surface.

## Decision

The Android app embeds the engine through `tidefell-ffi` (uniffi, Kotlin bindings), running in a
foreground service. It does not use the network API to reach its own engine.

## Consequences

- The phone is a real peer that can serve chunks to others.
- One code path for sync logic on every platform.
- `ffi` depends only on `sdk` and `engine`; its surface is generated, never hand-written.
