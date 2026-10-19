{
  description = "Tidefell: open-source peer-to-peer file sync";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Builds the dependencies once per Cargo.lock as their own derivation, so a change to
    # Tidefell's own code rebuilds only its crates.
    crane.url = "github:ipetkov/crane";
    # The MSRV toolchain for the MSRV check.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      workspace = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package;

      # The Rust builds of one system. Every check and the package share `cargoArtifacts`, the
      # compiled dependencies.
      rust =
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./.cargo
              ./crates
              ./xtask
              ./specs
            ];
          };
          # crane links each vendored crate from its own store path. askama (in uniffi_bindgen)
          # resolves template paths relative to a crate's real directory, which breaks through
          # the link, so uniffi_bindgen is copied instead.
          cargoVendorDir =
            let
              vendored = craneLib.vendorCargoDeps { inherit src; };
            in
            pkgs.runCommandLocal "vendor-cargo-deps" { } ''
              mkdir -p $out
              for registry in ${vendored}/*/; do
                name=$(basename "$registry")
                mkdir "$out/$name"
                for crate in "$registry"*; do
                  case "$(basename "$crate")" in
                    uniffi_bindgen-*) cp -rL "$crate" "$out/$name/" ;;
                    *) ln -s "$(readlink -f "$crate")" "$out/$name/" ;;
                  esac
                done
              done
              sed "s|${vendored}|$out|g" ${vendored}/config.toml > $out/config.toml
            '';
          commonArgs = {
            pname = "tidefell";
            inherit (workspace) version;
            # Everything a build or test reads.
            inherit src cargoVendorDir;
            strictDeps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          # Checks produce nothing but their success.
          check =
            lib: args:
            lib (
              commonArgs
              // {
                inherit cargoArtifacts;
                doInstallCargoArtifacts = false;
              }
              // args
            );

          msrvLib = (crane.mkLib pkgs).overrideToolchain (
            p: (p.extend rust-overlay.overlays.default).rust-bin.stable."${workspace.rust-version}.0".minimal
          );
          msrvArtifacts = msrvLib.buildDepsOnly commonArgs;

          # CI's host build, in the dev profile like a plain `cargo test`, so debug assertions hold:
          # the dependencies as their own derivation, then what CI's jobs run, fetched from the
          # binary cache when that source was built before instead of compiled in each job.
          devArgs = commonArgs // {
            CARGO_PROFILE = "dev";
          };
          devArtifacts = craneLib.buildDepsOnly devArgs;
          ciHost = craneLib.mkCargoDerivation (
            devArgs
            // {
              pnameSuffix = "-ci-host";
              cargoArtifacts = devArtifacts;
              doInstallCargoArtifacts = false;
              # The FUSE, mount and simulation tests (run outside the sandbox: they need /dev/fuse
              # or take long), tidefell-ffi with tidefelld and tidefell for the Kotlin tests, and
              # the Kotlin bindings generated from that library. Built workspace-wide, as the
              # dependencies were: one package alone would enable other features of them and
              # build them again.
              buildPhaseCargoCommand = ''
                cargo test --locked --profile dev --workspace --no-run \
                  --message-format json-render-diagnostics > tests.json
                cargo build --locked --profile dev --workspace
                target/debug/uniffi-bindgen generate --library target/debug/libtidefell_ffi.so \
                  --language kotlin --no-format --out-dir kotlin
              '';
              installPhaseCommand = ''
                mkdir -p $out/bin $out/lib $out/tests
                ${pkgs.jq}/bin/jq -r 'select(.reason == "compiler-artifact" and .profile.test
                  and (.target.name | IN("kernel", "mount", "seeds")))
                  | "\(.target.name) \(.executable)"' tests.json |
                  while read -r name executable; do cp "$executable" "$out/tests/$name"; done
                cp target/debug/tidefelld target/debug/tidefell $out/bin/
                cp target/debug/libtidefell_ffi.so $out/lib/
                cp -r kotlin $out/kotlin
              '';
            }
          );
        in
        {
          inherit ciHost;
          # `tidefelld` and `tidefell` binaries. The version is the workspace version (ADR 0005).
          package = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "--locked --package tidefell-daemon --package tidefell-cli";
              # The test check runs the tests; building the package does not repeat them.
              doCheck = false;
              meta = {
                description = "Open-source peer-to-peer file sync";
                homepage = "https://github.com/tidefell/tidefell";
                license = pkgs.lib.licenses.agpl3Only;
                mainProgram = "tidefell";
              };
            }
          );
          checks = {
            fmt = craneLib.cargoFmt { inherit (commonArgs) pname version src; };
            clippy = check craneLib.cargoClippy {
              cargoClippyExtraArgs = "--workspace --all-targets -- -D warnings";
            };
            # The sandbox has no network interface for multicast (ADR 0029) and no FUSE device;
            # the FUSE tests skip here and run in CI's FUSE job.
            test = check craneLib.cargoTest {
              cargoTestExtraArgs = "--workspace -- --skip devices_on_one_network_find_each_other";
            };
            crate-boundaries = check craneLib.mkCargoDerivation {
              pnameSuffix = "-crate-boundaries";
              # Release, like the shared dependencies.
              buildPhaseCargoCommand = "cargo run --release --locked --package xtask -- check-deps";
              installPhaseCommand = "mkdir -p $out";
            };
            msrv = msrvLib.cargoBuild (
              commonArgs
              // {
                cargoArtifacts = msrvArtifacts;
                cargoExtraArgs = "--locked --workspace --all-targets";
                doInstallCargoArtifacts = false;
                installPhaseCommand = "mkdir -p $out";
              }
            );
          };
        };

      # Shell, Nix and spelling checks over the source (ADR 0031).
      lintChecks =
        pkgs:
        let
          check =
            name: tools: script:
            pkgs.runCommand name { nativeBuildInputs = tools; } ''
              cd ${self}
              ${script}
              touch $out
            '';
        in
        {
          shellcheck = check "shellcheck" [ pkgs.shellcheck ] ''
            find . -name '*.sh' -print0 | xargs -0 --no-run-if-empty shellcheck
          '';
          statix = check "statix" [ pkgs.statix ] "statix check .";
          deadnix = check "deadnix" [ pkgs.deadnix ] "deadnix --fail .";
          typos = check "typos" [ pkgs.typos ] "typos";
          # The encrypted folders' golden vectors come from a second implementation, in Python;
          # this remakes them and checks it against RFC 9180 and sync/1's vectors.
          spec-vectors =
            check "spec-vectors"
              [ (pkgs.python3.withPackages (p: [ p.blake3 p.cryptography p.pynacl ])) ]
              ''
                work=$(mktemp -d)
                cp -r specs "$work/"
                cd "$work/specs/vectors/gen"
                export PYTHONDONTWRITEBYTECODE=1
                python3 hpke.py
                python3 check_wire.py
                for spec in encryption_v1 sync_v2 blobs_v2; do
                  python3 "$spec.py" > "$work/$spec.json"
                  diff -u "../''${spec//_/-}.json" "$work/$spec.json"
                done
              '';
        };

      # Evaluates the NixOS module and builds its systemd unit and checked configuration.
      nixosModuleCheck =
        let
          system = nixpkgs.lib.nixosSystem {
            system = "x86_64-linux";
            modules = [
              self.nixosModules.tidefelld
              {
                boot.isContainer = true;
                system.stateVersion = "25.11";
                networking.hostName = "check";
                services.tidefelld = {
                  enable = true;
                  openFirewall = true;
                  settings = {
                    network = {
                      bind = "0.0.0.0:7777";
                      relay.mode = "disabled";
                    };
                    folders = [
                      {
                        id = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
                        path = "/srv/tidefell/photos";
                        members = [ "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ];
                        retention_days = 30;
                      }
                    ];
                  };
                };
              }
            ];
          };
          pkgs = nixpkgs.legacyPackages.x86_64-linux;
        in
        assert system.config.networking.firewall.allowedUDPPorts == [ 7777 ];
        pkgs.runCommand "tidefelld-module-check" { } ''
          unit=${system.config.systemd.units."tidefelld.service".unit}/tidefelld.service
          grep -q 'tidefelld run --config' "$unit"
          grep -qx 'ReadWritePaths=/var/lib/tidefell' "$unit"
          grep -qx 'ReadWritePaths=/srv/tidefell/photos' "$unit"
          touch $out
        '';
    in
    {
      packages = forAllSystems (pkgs: rec {
        tidefell = (rust pkgs).package;
        default = tidefell;
        # What CI's FUSE, simulation and Kotlin jobs run, built once (see `ciHost`).
        ci-host = (rust pkgs).ciHost;
      });

      # `nix flake check` runs these: formatting, clippy, the tests, the crate boundaries, the MSRV
      # build and the package, all sharing the compiled dependencies, the NixOS module, and the
      # shell, Nix and spelling checks.
      checks = forAllSystems (
        pkgs:
        (rust pkgs).checks
        // lintChecks pkgs
        // {
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.tidefell;
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          nixos-module = nixosModuleCheck;
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            cargo-deny
            mdbook
          ];
        };
      });

      nixosModules = rec {
        tidefelld = import ./nix/module.nix {
          defaultPackage = pkgs: self.packages.${pkgs.stdenv.hostPlatform.system}.tidefell;
        };
        default = tidefelld;
      };

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
