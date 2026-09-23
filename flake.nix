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
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
