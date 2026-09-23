# Tidefell

Open-source, peer-to-peer file sync with the feel of Dropbox, OneDrive or Nextcloud, but with
no central server.

- **Every device is a peer.** Some run as *full replicas* that keep everything (a NAS, a home
  server); others run as *partial replicas* that hold placeholders and fetch file contents on
  demand.
- **Swarm downloads.** Files download from many peers at once, verified chunk by chunk.
- **The web interface is just another peer**, one that adds user accounts and sessions.
- **Deletes are recoverable.** Deleted files go to a recycle bin with a retention period chosen
  at first setup.

Tidefell is built in Rust on [iroh](https://github.com/n0-computer/iroh) (QUIC with hole
punching and relay fallback), with content-defined chunks transferred and verified by their
BLAKE3 hashes.

> **Status:** early development. Two `tidefelld` daemons can sync a folder, but there is no
> UI, no placeholders and no stability promise yet.

## Crate map

| Crate | Role | License |
| --- | --- | --- |
| `proto` | Wire types, ALPN constants, signed message formats | MIT OR Apache-2.0 |
| `chunker` | FastCDC content-defined chunking, BLAKE3 hashing, chunk index | AGPL-3.0-only |
| `store` | Block store, availability bitmaps, eviction, on-disk layout | AGPL-3.0-only |
| `meta` | File tree, versions, vector clocks, tombstones, recycle bin | AGPL-3.0-only |
| `identity` | Users, devices, folder membership, signing, key formats | AGPL-3.0-only |
| `net` | iroh endpoint, relay config, address lookup, sync/1 protocol | AGPL-3.0-only |
| `fetch` | Multi-peer scheduler, rarest-first, retry | AGPL-3.0-only |
| `engine` | Replica policies; wires the crates above into the daemon core | AGPL-3.0-only |
| `api` | REST + gRPC surface for CLI, web and apps | AGPL-3.0-only |
| `daemon` | Binary `tidefelld`: headless peer for Linux servers and NAS | AGPL-3.0-only |
| `cli` | Binary `tidefell` | AGPL-3.0-only |
| `web` | Web peer: accounts, sessions, embedded browser UI | AGPL-3.0-only |
| `fs-fuse` | Linux placeholder filesystem | AGPL-3.0-only |
| `sdk` | Public Rust client API for third parties | MIT OR Apache-2.0 |
| `ffi` | uniffi bindings to Kotlin (Android), later Swift | AGPL-3.0-only |

Each crate lives in `crates/<name>` and is published as `tidefell-<name>`.

Dependency rules:

- `proto` and `sdk` never depend on an AGPL crate.
- `engine` does not depend on `fs-fuse` or `web`.
- `ffi` depends only on `sdk` and `engine`.

Other directories:

- `specs/`: one protocol specification per ALPN.
- `tests/`: multi-peer integration tests.
- `docs/`: admin guide, self-hosting and contributor docs (mdBook).
- `xtask/`: repository automation (`cargo xtask`).

## Building

Requires stable Rust, 1.91 or newer (pinned via `rust-toolchain.toml`).

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

With Nix, `nix build` produces `tidefell` and `tidefelld`, and `nix develop` opens a shell with
the toolchain and `cargo-deny`.

## Conventions

Read [CONVENTIONS.md](CONVENTIONS.md) before making changes.

## Trying it out

Two machines (or two data directories) owned by the same user:

```sh
# On the first device: create a device key, a user key and a certificate.
tidefelld init --data-dir ~/.local/share/tidefell --name laptop
tidefelld new-folder-id            # share this ID with your other devices

# On a second device: create its key, then certify it with the user key from the first.
tidefelld init --data-dir /var/lib/tidefell --name nas --certificate nas.cert
#   where nas.cert came from, on the first device:
#   tidefelld certify --user-key ~/.local/share/tidefell/user.key \
#       --device <nas device id> --name nas --out nas.cert
```

Then write a configuration (see [`crates/daemon/tidefelld.example.toml`](crates/daemon/tidefelld.example.toml)),
check it with `tidefelld config check <file>`, and start the daemon with
`tidefelld run --config <file>`. `tidefelld config schema` prints the configuration's JSON Schema.

Current limitations: empty directories and symlinks are not synced, conflict copies are kept
in metadata but not written to disk, partial replicas hold metadata only, and device
certificates cannot be revoked yet.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). A signed [CLA](CLA.md) is required.

## License

Tidefell is licensed under the [GNU Affero General Public License v3.0 only](LICENSE)
(`AGPL-3.0-only`).

The protocol crate (`crates/proto`) and client SDK (`crates/sdk`) are dual-licensed under
[MIT](crates/proto/LICENSE-MIT) or [Apache-2.0](crates/proto/LICENSE-APACHE), at your option,
so third-party clients can interoperate without AGPL obligations.
