# Running tidefelld on NixOS

The flake exposes a NixOS module, `nixosModules.tidefelld`.

```nix
{
  inputs.tidefell.url = "github:tidefell/tidefell/v0.2.0";

  outputs = { nixpkgs, tidefell, ... }: {
    nixosConfigurations.nas = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        tidefell.nixosModules.tidefelld
        {
          services.tidefelld = {
            enable = true;
            openFirewall = true;
            certificateFile = "/run/secrets/tidefell-nas.cert";
            settings = {
              network.bind = "0.0.0.0:7777";
              folders = [{
                id = "<folder id>";
                path = "/srv/tidefell/photos";
                members = [ "<user id>" ];
                retention_days = 30;
              }];
            };
          };
        }
      ];
    };
  };
}
```

- `settings` is the [configuration file](guide/configuration.md) as Nix.
  It is checked with `tidefelld config check` when the system is built, so an invalid
  configuration fails `nixos-rebuild` rather than the service.
- `data_dir` defaults to `services.tidefelld.dataDir` (`/var/lib/tidefell`).
- `settings.web.bind` enables the web peer (see the example configuration). Create accounts
  on the host with `sudo -u tidefell tidefelld web account add --data-dir /var/lib/tidefell
  --username <name>`, and put a TLS reverse proxy in front of it.
- The service runs as the `tidefell` system user with a hardened systemd sandbox. It can
  write only its data directory and the configured folder paths, so those paths must be
  writable by `tidefell`.

## First start and certification

1. Deploy with the service enabled. On first start it creates a device key and logs the
   device ID: `journalctl -u tidefelld | grep device:`. The service then stops, because the
   device is not certified yet.
2. Where your user key lives, certify that ID:
   `tidefelld certify --user-key user.key --device <id> --name nas --out nas.cert`.
3. Put `nas.cert` on the machine (for example with agenix or sops-nix) and set
   `certificateFile`, or run once:
   `sudo -u tidefell tidefelld init --data-dir /var/lib/tidefell --name nas --certificate nas.cert`.
