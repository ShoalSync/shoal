# Summary

[Introduction](README.md)

# User guide

- [Installing](guide/install.md)
- [Setting up devices](guide/setup.md)
- [Folders](guide/folders.md)
- [Mounting a folder](guide/mounting.md)
- [Shoal on Windows](guide/windows.md)
- [The recycle bin](guide/recycle-bin.md)
- [The web peer](guide/web.md)
- [Configuration](guide/configuration.md)
- [When something goes wrong](guide/troubleshooting.md)

# Self-hosting

- [NixOS](nixos.md)
- [An always-on peer](guide/always-on.md)
- [A storage peer](guide/storage-peer.md)
- [The control API](admin/api.md)

# Reference

- [Glossary](glossary.md)
- [Protocols](reference/protocols.md)
  - [sync/1](reference/sync-v1.md)
  - [blobs/1](reference/blobs-v1.md)
  - [pair/2](reference/pair-v2.md)

# Security

- [Threat model](security/threat-model.md)

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
  - [0014: The ffi AAR is a release asset, not a Maven package](adr/0014-aar-release-asset.md)
  - [0015: Devices join a user by pairing over the network](adr/0015-pairing.md)
  - [0016: Partial replicas keep placeholders and fetch on demand](adr/0016-partial-replicas-on-demand.md)
  - [0017: tidefelld mounts folders read-write with FUSE](adr/0017-fuse-mounts.md)
  - [0018: Always-on peers without a central server](adr/0018-always-on-peers.md)
  - [0019: The web peer uploads files and shares links](adr/0019-web-uploads-and-share-links.md)
  - [0020: Camera-roll backup on Android](adr/0020-camera-roll-backup.md)
  - [0021: Recycle-bin retention is visible and editable](adr/0021-retention-settings.md)
  - [0022: Confirmed copies](adr/0022-confirmed-copies.md)
  - [0023: Encrypted folders and storage peers](adr/0023-encrypted-storage-peers.md)
  - [0024: Sync consistency and trust](adr/0024-sync-consistency-and-trust.md)
  - [0025: Phone folders on Android](adr/0025-phone-folders.md)
  - [0026: Compatibility from 1.0](adr/0026-compatibility-from-1-0.md)
  - [0027: Content is kept once per device](adr/0027-content-kept-once.md)
  - [0028: Other devices' files in phone folders](adr/0028-other-devices-files-on-the-phone.md)
  - [0029: Features from other sync tools](adr/0029-features-from-other-sync-tools.md)
  - [0030: Diagnostics and reporting](adr/0030-diagnostics-and-reporting.md)
  - [0031: Static analysis and fuzzing](adr/0031-static-analysis-and-fuzzing.md)
  - [0032: Transfer efficiency](adr/0032-transfer-efficiency.md)
  - [0033: File names across filesystems](adr/0033-file-names-across-filesystems.md)
  - [0034: Metadata storage and recovery](adr/0034-metadata-storage-and-recovery.md)
  - [0035: Memory at scale](adr/0035-memory-at-scale.md)
  - [0036: Simulation testing](adr/0036-simulation-testing.md)
  - [0037: Disk space and I/O errors](adr/0037-disk-space-and-io-errors.md)
  - [0038: Threat model](adr/0038-threat-model.md)
  - [0039: Android battery](adr/0039-android-battery.md)
  - [0040: Device revocation](adr/0040-device-revocation.md)
  - [0041: The repositories on Forgejo, consumed from source](adr/0041-forgejo.md)
  - [0042: The Windows client](adr/0042-windows-client.md)
  - [0043: Rename to Shoal](adr/0043-rename-to-shoal.md)
  - [0044: Licence: MIT OR Apache-2.0](adr/0044-licence-mit-or-apache.md)
  - [0045: GitHub hosting and hosted CI](adr/0045-github-hosting.md)
