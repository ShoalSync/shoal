# Shoal

Shoal Sync, formerly Tidefell.

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

Shoal is built in Rust on [iroh](https://github.com/n0-computer/iroh) (QUIC with hole
punching and relay fallback), with content-defined chunks transferred and verified by their
BLAKE3 hashes.

> **Status:** early development. Devices sync folders and can be managed with the `shoal`
> command or a browser, but there is no stability promise yet. Read the
> [documentation](docs/README.md).

## Crate map

| Crate | Role |
| --- | --- |
| `proto` | Wire types, ALPN constants, signed message formats |
| `chunker` | FastCDC content-defined chunking, BLAKE3 hashing, chunk index |
| `store` | Block store, availability bitmaps, eviction, on-disk layout |
| `meta` | File tree, versions, vector clocks, tombstones, recycle bin |
| `identity` | Users, devices, folder membership, signing, key formats |
| `crypto` | Encrypted folders: folder keys, sealed chunks and metadata, HPKE key grants |
| `net` | iroh endpoint, relay config, address lookup, sync/1 protocol |
| `fetch` | Multi-peer scheduler, rarest-first, retry |
| `engine` | Replica policies; wires the crates above into the daemon core |
| `api` | REST surface for CLI, web and apps |
| `daemon` | Binary `shoald`: headless peer for Linux servers and NAS |
| `cli` | Binary `shoal` |
| `web` | Web peer: accounts, sessions, embedded browser UI |
| `fs-fuse` | Linux placeholder filesystem |
| `sdk` | Public Rust client API for third parties |
| `ffi` | uniffi bindings to Kotlin (Android), later Swift |
| `sim` | Simulation testing: several engines on an in-memory network, driven by seeds |

Each crate lives in `crates/<name>` and is published as `shoal-<name>`, under MIT OR Apache-2.0.

Dependency rules:

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

With Nix, `nix build` produces `shoal` and `shoald`, and `nix develop` opens a shell with
the toolchain and `cargo-deny`.

## Conventions

Read [CONVENTIONS.md](CONVENTIONS.md) before making changes.

## Trying it out

Two machines (or two data directories) owned by the same user:

```sh
# On the first device: create a device key, a user key and a certificate.
shoald init --data-dir ~/.local/share/shoal --name laptop
shoald new-folder-id            # share this ID with your other devices

# On a second device: print a pairing code and wait.
shoald pair --data-dir /var/lib/shoal --name nas

# On the first device, with its daemon running: approve it after comparing the
# confirmation code both devices show.
shoal pair TFP1-…
```

Start each daemon with a configuration (see
[`crates/daemon/shoald.example.toml`](crates/daemon/shoald.example.toml); `folders` may be
empty): `shoald run --config <file>`. Then manage it with the `shoal` command, which talks
to the local daemon over its control socket:

```sh
shoal folder create ~/Photos              # prints the folder ID
shoal folder join <folder id> /srv/photos # on the other device
shoal peer add <laptop device id> --addr 192.168.1.10:7777
shoal files <folder id>
shoal recycle list <folder id>
shoal recycle restore <folder id> holiday.jpg
```

Everything `shoal` does goes through the `/v1` REST API; `shoald api schema` prints its
OpenAPI document and `shoald config schema` the configuration's JSON Schema.

### Web peer

Add a `[web]` section to a daemon's configuration and create an account:

```sh
shoald web account add --data-dir /var/lib/shoal --username alice --admin
```

People sign in from a browser and see the folders their user is a member of: browse, download
and restore deleted files. Serve it behind a TLS reverse proxy.

Current limitations: empty directories and symlinks are not synced, conflict copies are kept
in metadata but not written to disk, placeholders of partial replicas are fetched with
`shoal fetch` (there is no desktop filesystem integration yet), and device certificates
cannot be revoked yet.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Shoal is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

All earlier versions of this code, including commits made under the name Tidefell, are also
available under MIT OR Apache-2.0.

The name "Shoal Sync" and the logo identify this project: see [TRADEMARKS.md](TRADEMARKS.md).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
