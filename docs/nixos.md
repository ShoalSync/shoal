# Running shoald on NixOS

The flake exposes a NixOS module, `nixosModules.shoald`.

```nix
{
  inputs.shoal.url = "github:ShoalSync/shoal/v0.3.0";

  outputs = { nixpkgs, shoal, ... }: {
    nixosConfigurations.nas = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        shoal.nixosModules.shoald
        {
          services.shoald = {
            enable = true;
            openFirewall = true;
            certificateFile = "/run/secrets/shoal-nas.cert";
            settings = {
              network.bind = "0.0.0.0:7777";
              folders = [{
                id = "<folder id>";
                path = "/srv/shoal/photos";
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
  It is checked with `shoald config check` when the system is built, so an invalid
  configuration fails `nixos-rebuild` rather than the service.
- `data_dir` defaults to `services.shoald.dataDir` (`/var/lib/shoal`).
- `metadata_dir`, if set, is writable by the service like `data_dir`; put it on fast storage
  when `dataDir` and the folders are on slow disks.
- `settings.web.bind` enables the web peer (see the example configuration). Create accounts
  on the host with `sudo -u shoal shoald web account add --data-dir /var/lib/shoal
  --username <name>`, and put a TLS reverse proxy in front of it.
- `relayTokenFile` is the token for self-hosted relays that admit only holders of it, with
  `settings.network.relay.mode = "custom"`. It is given to the service as a systemd
  credential, so the file may be readable by root alone; give a path to a secret (agenix,
  sops-nix), not a Nix path literal, which would copy it into the store.
- The service runs as the `shoal` system user with a hardened systemd sandbox: no
  capabilities, no devices, a system call filter and no writable executable memory. It can
  write only its data directory and the configured folder paths, so those paths must be
  writable by `shoal`. FUSE mounts (`mount` on a folder) are not available under it.

## First start and certification

1. Deploy with the service enabled. On first start it creates a device key and logs the
   device ID: `journalctl -u shoald | grep device:`. The service then stops, because the
   device is not certified yet.
2. Where your user key lives, certify that ID:
   `shoald certify --user-key user.key --device <id> --name nas --out nas.cert`.
3. Put `nas.cert` on the machine (for example with agenix or sops-nix) and set
   `certificateFile`, or run once:
   `sudo -u shoal shoald init --data-dir /var/lib/shoal --name nas --certificate nas.cert`.
