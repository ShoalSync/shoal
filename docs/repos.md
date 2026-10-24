# Repository map

Where does a change belong? Start here. See [plan.md](plan.md) for how the repositories connect.

| If you are changing... | It belongs in |
| --- | --- |
| Rust code, protocol behaviour, wire formats, on-disk formats | `shoal/shoal` |
| Protocol specifications and test vectors (`specs/`) | `shoal/shoal` |
| User, admin and self-hosting documentation | `shoal/shoal` (`docs/`) |
| Glossary, ADRs, cross-repo conventions | `shoal/shoal` (`docs/`) |
| FFI surface exposed to Kotlin | `shoal/shoal` (`crates/ffi`) |
| Android UI, services, `DocumentsProvider`, Gradle build | `shoal/android` |
| Relay, DNS, always-on peer role, NixOS deployment | `shoal/infra` |
| Daemon packaging and the NixOS module for `shoald` | `shoal/shoal` (`flake.nix`) |
| Website design, landing pages, blog posts | `shoal/site` |

## Pins

Each downstream repository pins exactly one monorepo release.

| Repository | Where the pin lives |
| --- | --- |
| `shoal/android` | `gradle/libs.versions.toml` (`shoal = "X.Y.Z"`), checked out by `scripts/monorepo.sh` |
| `shoal/infra` | `flake.nix` input `shoal` and `flake.lock` |
| `shoal/site` | `flake.nix` input `shoal` and `flake.lock` |

Each takes the pinned release's source from Forgejo over SSH (ADR 0041).

## Change order

1. Change and review in the monorepo.
2. Release (a pre-release tag such as `v0.2.0-rc.1` is fine).
3. Bump the pin in each affected downstream repository.

Never the reverse. A downstream repository never defines a type, message or config key that
the monorepo does not.
