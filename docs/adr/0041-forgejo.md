# 0041: The repositories on Forgejo, consumed from source

- Status: accepted
- Date: 2026-10-18

## Context

The four repositories moved from GitHub to the project's own Forgejo instance (Forgejo 16) on
2026-10-18, code only: branches and tags. GitHub's issues, pull requests and releases stay there
as an archive, and nothing mirrors to GitHub. CI ran on self-hosted GitHub runners on one
machine; it now needs Forgejo's.

How the repositories reached the private monorepo was shaped by GitHub: the organisation's GitHub
App minted tokens that could read releases, so `tidefell/site` downloaded the release's built
book (ADR 0013), `tidefell/android` the release's AAR (ADR 0014) and `tidefelld` tarball, and only
`tidefell/infra` took the source, as a flake input. Release assets were a way to reach a private
repository, not a choice of what to consume. Forgejo's SSH access reads a repository at a tag
directly, with a deploy key.

## Decision

- **Hosting.** The repositories are `tidefell/{tidefell,android,infra,site}` on Forgejo, cloned
  and fetched over SSH. Pull requests are Forgejo pull requests, squash-merged once every commit
  status of their head passed.
- **CI.** Workflows are in `.forgejo/workflows/`, which Forgejo reads instead of
  `.github/workflows/`. They run on Forgejo runners on the same machine, with the host executor
  and the labels `self-hosted` and, for releases only, `release`. CI jobs run as a user of their
  own with no sudo and no access to developers' keys; the release runner keeps its own user
  (docs/project/release-runner.md). Each runner slot has its own `CARGO_HOME` and `RUSTUP_HOME`.
  `actions/upload-artifact` and `download-artifact` are pinned at v3, the newest Forgejo supports.
- **Reading the monorepo.** Downstream repositories consume the pinned release's source, not its
  assets. CI reads it with `TIDEFELL_READ_KEY`, an organisation secret holding a deploy key that can
  only read `tidefell/tidefell`, against Forgejo's pinned SSH host key; developers use their own
  keys.
  - `tidefell/infra`: the `tidefell` flake input, at
    `git+ssh://git@github.com/ShoalSync/shoal?ref=refs/tags/vX.Y.Z`.
  - `tidefell/site`: a `tidefell` input of the same form with `flake = false`; the site's flake
    builds the book from it with mdbook, so the book no longer needs to be a release asset.
    This replaces the download in ADR 0013.
  - `tidefell/android`: `scripts/monorepo.sh` checks out the tag pinned in
    `gradle/libs.versions.toml`, and Gradle builds `tidefell-ffi` from it as an included build,
    which `-PtidefellFfiLocalPath` already did for FFI work. CI builds the emulator's ABI only and
    keeps a checkout per runner slot, so cargo rebuilds only what changed. The exit-criterion
    test's `tidefelld` is the release's flake package. This supersedes ADR 0014.
- **Releases.** `release.yml` is in `.forgejo/workflows/` and publishes Forgejo releases through
  the API with the job's token, after checking that the tag is on `main` and that its commit's CI
  and Nix statuses all succeeded. The assets are the binaries and the specs, vectors and schemas,
  each with a SHA-256 file. The AAR and the book are no longer assets: android and site build
  them from the pinned release's source. Sigstore's keyless signing used GitHub's OIDC identity,
  which Forgejo does not provide, so assets are signed with cosign and a release key
  (2026-10-19): the private key is a repository secret, the public key is `cosign.pub` in the
  repository, and the workflow checks every signature against it before publishing. Signatures
  are not recorded in Sigstore's public log, which would publish the private repository's asset
  hashes.

## Consequences

- No GitHub App and no tokens with an expiry in CI; one read-only key reaches the monorepo.
- A downstream change to the pin is a tag in a URL or a version, as before, and a source build
  checks the release builds, not only that an asset exists.
- android CI compiles Rust for one ABI per run instead of downloading an AAR: slower when the
  pin changes, incremental otherwise. A Nix package of the AAR would make it cacheable; that is
  later work.
- Forgejo is reachable from the project's own network only, so the source is not public while
  the repositories are private, and links in the documentation to GitHub point at the archive.
- Renovate runs against Forgejo from the same server and keeps opening its pull requests there.
