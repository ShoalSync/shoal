# Tidefell

Open-source, peer-to-peer file sync with the feel of Dropbox, OneDrive or Nextcloud, but with
no central server.

- **Every device is a peer.** Some run as *full replicas* that keep everything (a NAS, a home
  server); others run as *partial replicas* that hold placeholders and fetch file contents on
  demand.
- **Swarm downloads.** Files download from many peers at once, verified chunk by chunk.
- **Always on, without a central server.** Run an always-on peer on a machine you control,
  such as a NAS or a small VPS, so devices that are rarely online together still sync. The
  project runs no storage service.
- **The web interface is just another peer**, one that adds user accounts and sessions. It
  uploads files and can share a file or directory through a link.
- **Deletes are recoverable.** Deleted files go to a recycle bin; each device chooses how long
  to keep them and can change it at any time.

Tidefell is built in Rust on [iroh](https://github.com/n0-computer/iroh) (QUIC with hole
punching and relay fallback), with content-defined chunks transferred and verified by their
BLAKE3 hashes.

> **Status:** early development. Devices sync folders and can be managed with the `tidefell`
> command or a browser, but there is no stability promise yet. Read the
> [documentation](docs/README.md).

## Crate map

| Crate | Role | License |
| --- | --- | --- |
| `proto` | Wire types, ALPN constants, signed message formats | MIT OR Apache-2.0 |
| `chunker` | FastCDC content-defined chunking, BLAKE3 hashing, chunk index | AGPL-3.0-only |
| `store` | Block store, availability bitmaps, eviction, on-disk layout | AGPL-3.0-only |
| `meta` | File tree, versions, vector clocks, tombstones, recycle bin | AGPL-3.0-only |
| `identity` | Users, devices, folder membership, signing, key formats | AGPL-3.0-only |
| `crypto` | Encrypted folders: folder keys, sealed chunks and metadata, HPKE key grants | AGPL-3.0-only |
| `net` | iroh endpoint, relay config, address lookup, sync/1 protocol | AGPL-3.0-only |
| `fetch` | Multi-peer scheduler, rarest-first, retry | AGPL-3.0-only |
| `engine` | Replica policies; wires the crates above into the daemon core | AGPL-3.0-only |
| `api` | REST surface for CLI, web and apps | AGPL-3.0-only |
| `daemon` | Binary `tidefelld`: headless peer for Linux servers and NAS | AGPL-3.0-only |
| `cli` | Binary `tidefell` | AGPL-3.0-only |
| `web` | Web peer: accounts, sessions, embedded browser UI | AGPL-3.0-only |
| `fs-fuse` | Linux placeholder filesystem | AGPL-3.0-only |
| `sdk` | Public Rust client API for third parties | MIT OR Apache-2.0 |
| `ffi` | uniffi bindings to Kotlin (Android), later Swift | AGPL-3.0-only |
| `sim` | Simulation testing: several engines on an in-memory network, driven by seeds | AGPL-3.0-only |

Each crate lives in `crates/<name>` and is published as `tidefell-<name>`.

Dependency rules:

- `proto` and `sdk` never depend on an AGPL crate.
- `engine` does not depend on `fs-fuse` or `web`.
- `ffi` depends only on `sdk` and `engine`.

Other directories:

- `specs/`: one protocol specification per ALPN.
- `tests/`: multi-peer integration tests.
- `docs/`: user guide, self-hosting and contributor docs (mdBook; `mdbook serve docs`).
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

# On a second device: print a pairing code and wait.
tidefelld pair --data-dir /var/lib/tidefell --name nas

# On the first device, with its daemon running: approve it after comparing the
# confirmation code both devices show.
tidefell pair TFP1-…
```

Start each daemon with a configuration (see
[`crates/daemon/tidefelld.example.toml`](crates/daemon/tidefelld.example.toml); `folders` may be
empty): `tidefelld run --config <file>`. Then manage it with the `tidefell` command, which talks
to the local daemon over its control socket:

```sh
tidefell folder create ~/Photos              # prints the folder ID
tidefell folder join <folder id> /srv/photos # on the other device
tidefell peer add <laptop device id> --addr 192.168.1.10:7777
tidefell files <folder id>
tidefell recycle list <folder id>
tidefell recycle restore <folder id> holiday.jpg
```

Everything `tidefell` does goes through the `/v1` REST API; `tidefelld api schema` prints its
OpenAPI document and `tidefelld config schema` the configuration's JSON Schema.

### Web peer

Add a `[web]` section to a daemon's configuration and create an account:

```sh
tidefelld web account add --data-dir /var/lib/tidefell --username alice --admin
```

People sign in from a browser and see the folders their user is a member of: browse, download
and restore deleted files. Serve it behind a TLS reverse proxy.

Current limitations: empty directories and symlinks are not synced, conflict copies are kept
in metadata but not written to disk, placeholders of partial replicas are fetched with
`tidefell fetch` (there is no desktop filesystem integration yet), and device certificates
cannot be revoked yet.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). A signed [CLA](CLA.md) is required.

## License

Tidefell is licensed under the [GNU Affero General Public License v3.0 only](LICENSE)
(`AGPL-3.0-only`).

The protocol crate (`crates/proto`) and client SDK (`crates/sdk`) are dual-licensed under
[MIT](crates/proto/LICENSE-MIT) or [Apache-2.0](crates/proto/LICENSE-APACHE), at your option,
so third-party clients can interoperate without AGPL obligations.
