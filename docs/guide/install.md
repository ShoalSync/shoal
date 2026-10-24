# Installing

Shoal ships two programs, and a third on Windows:

- `shoald`, the daemon. It holds the device's keys, syncs folders with other peers, serves
  the control API and, optionally, the web peer.
- `shoal`, the command-line client. It talks to a running `shoald` on the same machine.
- `shoal-tray`, on Windows: the notification-area app that runs the daemon for you. See
  [Shoal on Windows](windows.md).

## Release binaries

Each [GitHub release](https://github.com/ShoalSync/shoal/releases) has archives for Linux
(x86-64 and ARM64), each with a `.sha256` file and, from 0.20, a `.sigstore.json` signature. On
macOS, build from source (below).

```sh
version=X.Y.Z
target=x86_64-unknown-linux-gnu
curl -LO https://github.com/ShoalSync/shoal/releases/download/v$version/shoal-$version-$target.tar.gz
curl -LO https://github.com/ShoalSync/shoal/releases/download/v$version/shoal-$version-$target.tar.gz.sha256
sha256sum -c shoal-$version-$target.tar.gz.sha256
tar -xzf shoal-$version-$target.tar.gz
sudo install shoal-$version-$target/shoal shoal-$version-$target/shoald /usr/local/bin/
```

### Checking who built it

Every asset is signed when it is released, with the project's release key: its public half is
[`cosign.pub`](https://github.com/ShoalSync/shoal/blob/main/cosign.pub) in the repository.
[cosign](https://docs.sigstore.dev/cosign/system_config/installation/) checks that an asset
was signed with it:

```sh
curl -LO https://github.com/ShoalSync/shoal/releases/download/v$version/shoal-$version-$target.tar.gz.sigstore.json
cosign verify-blob shoal-$version-$target.tar.gz --key cosign.pub \
  --bundle shoal-$version-$target.tar.gz.sigstore.json --insecure-ignore-tlog
```

The signatures are not recorded in Sigstore's public log, so cosign warns that it skipped the
log; the key is what it checks.

Releases 0.20 and 0.21 were signed without a key, by GitHub's release workflow, through Sigstore,
and are checked by the workflow that signed them:

```sh
cosign verify-blob shoal-$version-$target.tar.gz \
  --bundle shoal-$version-$target.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/Shoal/shoal/\.github/workflows/release\.yml@refs/(tags/v.+|heads/main)$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

A checksum shows a download arrived whole; this shows it is what the release built.

## Windows

From 0.23, each release has a Windows installer, `shoal-X.Y.Z-x86_64.msi`, and a zip of the
same programs, each with a `.sha256` file and a `.sigstore.json` signature, on the
[releases page](https://github.com/ShoalSync/shoal/releases). The installer is for one
user and needs no administrator rights: it puts the programs in
`%LOCALAPPDATA%\Programs\Shoal`, adds that to your `PATH`, starts the tray app when you sign
in, and adds Shoal to the Start menu. Installing a newer version replaces the older one;
uninstalling (Settings → Apps) leaves your data directory and your folders.

The installer is not signed with a code-signing certificate yet, so Windows' SmartScreen warns
before running it ("Windows protected your PC": choose **More info**, then **Run anyway**). Check
it first: its checksum, in PowerShell,

```powershell
(Get-FileHash shoal-X.Y.Z-x86_64.msi).Hash.ToLower()
Get-Content shoal-X.Y.Z-x86_64.msi.sha256
```

and its signature, with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/)
for Windows, as below.

## Nix

```sh
nix profile install github:ShoalSync/shoal/vX.Y.Z
```

On NixOS, use the module instead; see [NixOS](../nixos.md).

## From source

Shoal needs stable Rust 1.91 or newer:

```sh
git clone https://github.com/ShoalSync/shoal
cd shoal
cargo build --release --package shoal-daemon --package shoal-cli
```

The binaries are `target/release/shoald` and `target/release/shoal`.

## Known limitations

- Empty directories and symlinks are not synced.
- Conflicting versions are not written to disk until you resolve them; see
  [Conflicts](folders.md#conflicts).
- Placeholders of a partial replica are fetched with `shoal fetch` or the web peer, through a
  [FUSE mount](mounting.md) on Linux, and on Windows by opening them in Explorer.
- Device certificates cannot be revoked yet.
