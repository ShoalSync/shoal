# 0012: Server-rendered web UI with htmx

- Status: accepted
- Date: 2026-09-23

## Context

The web peer embeds a browser UI in the binary. Options were a JavaScript single-page app, a
Rust/WASM app, or server-rendered HTML.

## Decision

HTML is rendered on the server with compile-time templates (askama). Interactivity uses htmx,
vendored as one static file. CSS is a single hand-written stylesheet. All assets are embedded
in the binary.

## Consequences

- No Node or WASM toolchain: the web peer builds with cargo and Nix alone.
- Every page works without JavaScript for reading; htmx enhances forms and live updates.
- Richer client-side interactions later would need a new ADR.
