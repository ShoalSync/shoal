# Repository map

Where does a change belong? Start here. See [plan.md](plan.md) for how the repositories connect.

| If you are changing... | It belongs in |
| --- | --- |
| Rust code, protocol behaviour, wire formats, on-disk formats | `ShoalSync/shoal` |
| Protocol specifications and test vectors (`specs/`) | `ShoalSync/shoal` |
| User, admin and self-hosting documentation | `ShoalSync/shoal` (`docs/`) |
| Glossary, ADRs, cross-repo conventions | `ShoalSync/shoal` (`docs/`) |
| FFI surface exposed to Kotlin | `ShoalSync/shoal` (`crates/ffi`) |
| Android UI, services, `DocumentsProvider`, Gradle build | `ShoalSync/android` |
| Relay, DNS, always-on peer role, NixOS deployment | `ShoalSync/infra` |
| Daemon packaging and the NixOS module for `shoald` | `ShoalSync/shoal` (`flake.nix`) |
| Website design, landing pages, blog posts | `ShoalSync/site` |

## Pins

Each downstream repository pins exactly one monorepo release.

| Repository | Where the pin lives |
| --- | --- |
| `ShoalSync/android` | `gradle/libs.versions.toml` (`shoal = "X.Y.Z"`), checked out by `scripts/monorepo.sh` |
| `ShoalSync/infra` | `flake.nix` input `shoal` and `flake.lock` |
| `ShoalSync/site` | `flake.nix` input `shoal` and `flake.lock` |

Each takes the pinned release's source from GitHub over HTTPS (ADR 0045).

## Change order

1. Change and review in the monorepo.
2. Release (a pre-release tag such as `v0.2.0-rc.1` is fine).
3. Bump the pin in each affected downstream repository.

Never the reverse. A downstream repository never defines a type, message or config key that
the monorepo does not.
