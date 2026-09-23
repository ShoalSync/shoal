{
  description = "Tidefell: open-source peer-to-peer file sync";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      workspace = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package;
    in
    {
      packages = forAllSystems (pkgs: rec {
        # `tidefelld` and `tidefell` binaries. The version is the workspace version (ADR 0005).
        tidefell = pkgs.rustPlatform.buildRustPackage {
          pname = "tidefell";
          inherit (workspace) version;
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
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [
            "--package"
            "tidefell-daemon"
            "--package"
            "tidefell-cli"
          ];
          cargoTestFlags = [ "--workspace" ];
          meta = {
            description = "Open-source peer-to-peer file sync";
            homepage = "https://github.com/tidefell/tidefell";
            license = pkgs.lib.licenses.agpl3Only;
            mainProgram = "tidefell";
          };
        };
        default = tidefell;
      });

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

      # Evaluates the NixOS module and builds its systemd unit and checked configuration.
      checks.x86_64-linux.nixos-module =
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

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
