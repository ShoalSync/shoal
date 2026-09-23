# Summary

[Introduction](README.md)

# User guide

- [Installing](guide/install.md)
- [Setting up devices](guide/setup.md)
- [Folders](guide/folders.md)
- [The recycle bin](guide/recycle-bin.md)
- [The web peer](guide/web.md)
- [Configuration](guide/configuration.md)

# Self-hosting

- [NixOS](nixos.md)
- [The control API](admin/api.md)

# Reference

- [Glossary](glossary.md)
- [Protocols](reference/protocols.md)
  - [sync/1](reference/sync-v1.md)
  - [blobs/1](reference/blobs-v1.md)

# Project

- [Implementation plan](plan.md)
- [Repository map](repos.md)
- [Contributing](project/contributing.md)
- [Architecture decisions](adr/README.md)
  - [0001: CONVENTIONS.md in every repository](adr/0001-conventions-file.md)
  - [0002: `org.tidefell` namespace](adr/0002-namespace.md)
  - [0003: Nix for infrastructure and packaging](adr/0003-nix-infrastructure.md)
  - [0004: Android runs the full peer via FFI](adr/0004-android-ffi.md)
  - [0005: One version for all monorepo artifacts](adr/0005-single-version.md)
  - [0006: postcard wire encoding and Ed25519 signed envelopes](adr/0006-wire-encoding.md)
  - [0007: Minimum supported Rust version 1.91](adr/0007-msrv-1-91.md)
  - [0008: Own chunk transfer protocol on the Tidefell block store](adr/0008-blob-transfer.md)
  - [0009: REST/JSON API with generated OpenAPI; no gRPC for now](adr/0009-rest-api.md)
  - [0010: Local control over a Unix socket; web peer inside tidefelld](adr/0010-local-control.md)
  - [0011: Web accounts are local logins mapped to Tidefell users](adr/0011-web-accounts.md)
  - [0012: Server-rendered web UI with htmx](adr/0012-web-ui.md)
  - [0013: Zola for the website, mdBook for documentation](adr/0013-site-generator.md)
