# Repository map

Where does a change belong? Start here. See [plan.md](plan.md) for how the repositories connect.

| If you are changing... | It belongs in |
| --- | --- |
| Rust code, protocol behaviour, wire formats, on-disk formats | `tidefell/tidefell` |
| Protocol specifications and test vectors (`specs/`) | `tidefell/tidefell` |
| User, admin and self-hosting documentation | `tidefell/tidefell` (`docs/`) |
| Glossary, ADRs, cross-repo conventions | `tidefell/tidefell` (`docs/`) |
| FFI surface exposed to Kotlin | `tidefell/tidefell` (`crates/ffi`) |
| Android UI, services, `DocumentsProvider`, Gradle build | `tidefell/android` |
| Relay, DNS, always-on peer role, NixOS deployment | `tidefell/infra` |
| Daemon packaging and the NixOS module for `tidefelld` | `tidefell/tidefell` (`flake.nix`) |
| Website design, landing pages, blog posts | `tidefell/site` |

## Pins

Each downstream repository pins exactly one monorepo release.

| Repository | Where the pin lives |
| --- | --- |
| `tidefell/android` | `gradle/libs.versions.toml` (`tidefell = "X.Y.Z"`), checked out by `scripts/monorepo.sh` |
| `tidefell/infra` | `flake.nix` input `tidefell` and `flake.lock` |
| `tidefell/site` | `flake.nix` input `tidefell` and `flake.lock` |

Each takes the pinned release's source from Forgejo over SSH (ADR 0041).

## Change order

1. Change and review in the monorepo.
2. Release (a pre-release tag such as `v0.2.0-rc.1` is fine).
3. Bump the pin in each affected downstream repository.

Never the reverse. A downstream repository never defines a type, message or config key that
the monorepo does not.
