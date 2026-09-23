# Installing

Tidefell ships two programs:

- `tidefelld`, the daemon. It holds the device's keys, syncs folders with other peers, serves
  the control API and, optionally, the web peer.
- `tidefell`, the command-line client. It talks to a running `tidefelld` on the same machine.

## Release binaries

Each [GitHub release](https://github.com/tidefell/tidefell/releases) has archives for Linux
(x86-64 and ARM64) and macOS (Apple silicon), each with a `.sha256` file:

```sh
version=X.Y.Z
target=x86_64-unknown-linux-gnu
curl -LO https://github.com/tidefell/tidefell/releases/download/v$version/tidefell-$version-$target.tar.gz
curl -LO https://github.com/tidefell/tidefell/releases/download/v$version/tidefell-$version-$target.tar.gz.sha256
sha256sum -c tidefell-$version-$target.tar.gz.sha256
tar -xzf tidefell-$version-$target.tar.gz
sudo install tidefell-$version-$target/tidefell tidefell-$version-$target/tidefelld /usr/local/bin/
```

## Nix

```sh
nix profile install github:tidefell/tidefell/vX.Y.Z
```

On NixOS, use the module instead; see [NixOS](../nixos.md).

## From source

Tidefell needs stable Rust 1.91 or newer:

```sh
git clone https://github.com/tidefell/tidefell
cd tidefell
cargo build --release --package tidefell-daemon --package tidefell-cli
```

The binaries are `target/release/tidefelld` and `target/release/tidefell`.

## Known limitations

- Empty directories and symlinks are not synced.
- Conflict copies are kept in metadata but not written to disk.
- Placeholders of a partial replica are fetched with `tidefell fetch` or the web peer; there is
  no desktop filesystem integration yet.
- Device certificates cannot be revoked yet.
