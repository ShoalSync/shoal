# 0045: GitHub hosting and hosted CI

- Status: accepted
- Date: 2026-10-24
- Supersedes: [0041](0041-forgejo.md)'s releases and runners, and the runner parts of
  [0042](0042-windows-client.md)

## Context

The four repositories went public under the name Shoal (ADR 0043, 0044) in the `ShoalSync`
organisation on GitHub: `ShoalSync/shoal`, `ShoalSync/android`, `ShoalSync/infra` and
`ShoalSync/site`. ADR 0041 put them on a self-run Forgejo with CI on the project's own runners: a
few container runners on one machine, a Windows VM with a desktop session, and a release runner
that ran nothing else. Public repositories get GitHub's hosted runners free, Linux, Windows and
ARM, so the project's own machines are no longer needed for CI.

A spike on the hosted Windows images (Windows Server 2025 on x86-64, Windows 11 Enterprise on
ARM64) found that both run jobs in an interactive session with Explorer and the cloud files
filter running: sync roots, placeholders, the tray's window and the MSI all work. One test fails
on both: after `fs-cfapi` pins a placeholder, Windows never hydrates it, which Windows 11 Pro
does by itself and the provider relied on.

## Decision

- **The repositories are on GitHub**, `main` protected by a ruleset: changes only by pull
  request, no force-push, and every CI job a required check. The monorepo is public, so the
  other repositories fetch its pinned release over HTTPS, with no deploy key.
- **CI runs on GitHub's hosted runners**, workflows in `.github/workflows`:
  - Linux jobs on `ubuntu-24.04`, with Nix from `cachix/install-nix-action` (KVM on, for the
    NixOS VM tests) and the Nix store kept in the Actions cache by
    `nix-community/cache-nix-action`, no binary cache service. One job, `nix flake check`, builds
    and checks everything and saves the store, on `main` only; the jobs after it get the builds
    they run from it as an artifact. The cache key is the lock files', so a change to the code
    reuses the compiled dependencies.
  - Windows jobs on `windows-2025` and `windows-11-arm`, each with its compiled dependencies kept
    by `Swatinem/rust-cache`, saved on `main` only.
  - Nightly work is split where one 4-core runner would take too long: the 10,000 simulation
    seeds run as ten shards of 1,000.
  - Every pull request runs every job, documentation-only ones too: a workflow a path filter
    skips never reports, and a required check that never reports blocks the merge.
- **`fs-cfapi`'s pinning test is ignored** until the provider hydrates a pinned placeholder
  itself, the first Shoal change after the move, which also covers users' machines that behave
  like the hosted images.
- **The exit criterion runs on two hosted machines** with only the internet between them: they
  pass pairing codes as commit statuses through GitHub's REST API, and the test asks that the
  two connect, directly or through a relay, rather than directly over a LAN.
- **Releases are GitHub releases**, made by `.github/workflows/release.yml` on hosted runners,
  never with a cache: Linux on `ubuntu-24.04` (x86-64) and `ubuntu-24.04-arm` (aarch64, built
  natively), Windows on `windows-2025`. The publishing job runs in the protected `release`
  environment, which only tags and `main` may deploy to.
- **Release assets are signed without a key** (cosign keyless): each asset's Sigstore bundle
  holds a short-lived certificate for the release workflow's identity, from GitHub's OIDC token,
  and is recorded in Sigstore's public log. Users check the identity, as the install guide shows.
  `cosign.pub` stays in the repository for the releases signed with the key, 0.22.0 to 0.23.1.
- **Each release carries `THIRD-PARTY-LICENSES.html`**, every dependency's licence and notice,
  made by `cargo about` from `about.toml`; CI checks it can be made.

## Consequences

- No machine of the project's runs CI or releases, so the release runner and its separate user
  are gone, and with them the threat model's gaps of a release runner beside pull-request
  runners and of Windows releases built on the CI runner.
- No private key signs releases. An attacker would need to run the release workflow on a tag
  or on `main`, which takes a reviewed change on `main`.
- Hosted runners have 4 cores and start cold: a cache miss rebuilds the dependencies, and the
  first run after a lock-file change is the slow one. The Actions cache holds 10 GB per
  repository, shared by the Nix store and the Windows caches.
- The memory measurements run on shared machines, so the numbers vary more from night to night.
