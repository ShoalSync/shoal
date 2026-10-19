# Installing

Tidefell ships two programs:

- `tidefelld`, the daemon. It holds the device's keys, syncs folders with other peers, serves
  the control API and, optionally, the web peer.
- `tidefell`, the command-line client. It talks to a running `tidefelld` on the same machine.

## Release binaries

Each [GitHub release](https://github.com/tidefell/tidefell/releases) has archives for Linux
(x86-64 and ARM64), each with a `.sha256` file and, from 0.20, a `.sigstore.json` signature. On
macOS, build from source (below).

```sh
version=X.Y.Z
target=x86_64-unknown-linux-gnu
curl -LO https://github.com/tidefell/tidefell/releases/download/v$version/tidefell-$version-$target.tar.gz
curl -LO https://github.com/tidefell/tidefell/releases/download/v$version/tidefell-$version-$target.tar.gz.sha256
sha256sum -c tidefell-$version-$target.tar.gz.sha256
tar -xzf tidefell-$version-$target.tar.gz
sudo install tidefell-$version-$target/tidefell tidefell-$version-$target/tidefelld /usr/local/bin/
```

### Checking who built it

Every asset is signed when it is released, with the project's release key: its public half is
[`cosign.pub`](https://github.com/tidefell/tidefell/blob/main/cosign.pub) in the repository.
[cosign](https://docs.sigstore.dev/cosign/system_config/installation/) checks that an asset
was signed with it:

```sh
curl -LO https://github.com/tidefell/tidefell/releases/download/v$version/tidefell-$version-$target.tar.gz.sigstore.json
cosign verify-blob tidefell-$version-$target.tar.gz --key cosign.pub \
  --bundle tidefell-$version-$target.tar.gz.sigstore.json --insecure-ignore-tlog
```

The signatures are not recorded in Sigstore's public log, so cosign warns that it skipped the
log; the key is what it checks.

Releases 0.20 and 0.21 were signed without a key, by GitHub's release workflow, through Sigstore,
and are checked by the workflow that signed them:

```sh
cosign verify-blob tidefell-$version-$target.tar.gz \
  --bundle tidefell-$version-$target.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/Tidefell/tidefell/\.github/workflows/release\.yml@refs/(tags/v.+|heads/main)$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

A checksum shows a download arrived whole; this shows it is what the release built.

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
- Conflicting versions are not written to disk until you resolve them; see
  [Conflicts](folders.md#conflicts).
- Placeholders of a partial replica are fetched with `tidefell fetch` or the web peer; there is
  no desktop filesystem integration yet.
- Device certificates cannot be revoked yet.
