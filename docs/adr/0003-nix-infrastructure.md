# 0003: Nix for infrastructure and packaging

- Status: accepted
- Date: 2026-09-23

## Context

Hosted services (relay, DNS, web peer, full-replica storage) need reproducible deployment.
The alternative considered was Terraform + Ansible.

## Decision

- The monorepo has a `flake.nix` exposing packages (`tidefelld`, `tidefell`) and, from Phase 2,
  a NixOS module for the daemon.
- `tidefell/infra` is a Nix flake that takes the monorepo as an input pinned to a release tag,
  and defines hosts as NixOS configurations.
- Upstream iroh relay and DNS servers are packaged in, or pinned by, the infra flake.
- Secrets are handled with an encrypted-secrets tool (agenix or sops-nix, chosen in Phase 2);
  plaintext secrets are never committed.

## Consequences

- One lock file (`flake.lock`) records exactly what each host runs.
- Contributors to `infra` need Nix. The monorepo builds without Nix; the flake is an extra
  entry point, not a requirement.
- Cloud resource provisioning, if needed, is revisited in a later ADR.
