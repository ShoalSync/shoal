# The release runner

Releases are built on one runner that runs nothing else (Phase 5c step 5): a Forgejo runner
(ADR 0041) registered to this repository alone, as a user of its own, with the label `release` and
none of the labels the CI runners take, so no pull-request job (`runs-on: self-hosted`) ever lands
on it. It keeps its own toolchains and caches. The release workflow refuses a tag that is not on
`main` or whose commit did not pass CI.

## Setting one up

On a Linux machine with `gcc`, `gcc-aarch64-linux-gnu`, `pkg-config`, `unzip`, `git`, `python3`
and internet access: the job's user has no sudo, so the cross compiler must be installed already.
Everything else the release needs is installed by the workflow: Rust into the runner's own home,
and cosign from Nix, which the runner's machine must have.

```sh
sudo useradd -m -s /bin/bash tidefell-release
# A registration token from the repository's Settings → Actions → Runners on Forgejo.
sudo -u tidefell-release forgejo-runner register --no-interactive \
  --instance https://github.com --token "$token" \
  --name "$(hostname)-release" --labels release:host
# Then run `forgejo-runner daemon` as tidefell-release, from a systemd unit.
```

The repository's runners list then shows it with the label `release` only.

## The release key

Assets are signed with cosign and the project's release key (ADR 0041). Made once, by a
maintainer, on their own machine:

```sh
cosign generate-key-pair      # asks for a password; writes cosign.key and cosign.pub
```

`cosign.pub` is committed at the repository's root, where the workflow checks every signature
against it. The contents of `cosign.key` and its password go in the repository's Actions secrets
as `COSIGN_PRIVATE_KEY` and `COSIGN_PASSWORD`, and `cosign.key` is then kept offline or deleted:
a new key means a new `cosign.pub`, and users are told in the release notes.

## What it does not protect against

The pull-request runners run as a user of their own with no sudo, so a hostile pull request
cannot read the release user's files directly; but a machine shared with them is no boundary
against one that escapes its user through the kernel or a shared service. The runner keeps a
release's builds and caches apart from everyone else's, which stops accidents and cache
poisoning. A release runner on a machine of its own is the boundary; the threat model lists
this as a gap until then.

## Windows

There is no Windows release runner yet. Until there is, a release's Windows programs are built
on the Windows CI runner (label `windows`), which also runs pull requests' jobs: from the
release tag, with the release's pinned toolchain, in a target directory made afresh for the
release rather than the one CI keeps. They are packaged (a zip, and the MSI that wixl builds from
`packaging/windows/tidefell.wxs`) and signed on the release runner, so the release key never
reaches the Windows machine. What this does not protect against: a pull request's job that
changed the Windows runner's toolchain, or anything else there that outlives a job, would reach
the release's programs. The installer is not signed with Authenticode either, so Windows'
SmartScreen warns before running it; its cosign signature is the one to check.

