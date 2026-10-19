# An always-on peer

Tidefell syncs directly between your devices, so two devices only sync while both are online.
A phone and a laptop that are rarely awake at the same time can go a long while without
syncing. An *always-on peer* fixes that: a full replica that is always online, which each
device syncs with whenever it is awake.

A NAS at home does this already. Without one, run an always-on peer on a small server you
control, such as a VPS. It is your machine and holds your files in plaintext, like any other
replica, so choose a provider you trust with them
([ADR 0018](../adr/0018-always-on-peers.md)). For a machine you do not trust with them, use a
[storage peer](storage-peer.md) instead: it keeps an encrypted folder without being able to read
it. The Tidefell project runs no storage service.

## What you need

- A small Linux server with a public IP address. A folder's full size in disk space, plus a
  little for metadata; memory and CPU needs are modest.
- A DNS name pointing at it, e.g. `tidefell.example.org`, for the web peer's HTTPS
  certificate.
- Your user key, on a device where `tidefelld init` created it.

## On NixOS

The [infrastructure flake](https://github.com/tidefell/infra) has a role for this. It runs
`tidefelld` as a full replica, opens UDP 7777 for sync, and serves the web peer over HTTPS
through Caddy, which obtains the certificate:

```nix
{
  inputs.infra.url = "github:tidefell/infra";

  outputs = { infra, ... }: {
    nixosConfigurations.vps = infra.inputs.nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./hardware-configuration.nix
        infra.nixosModules.role-always-on
        {
          tidefell.domain = "tidefell.example.org";
          tidefell.alwaysOn = {
            acmeEmail = "me@example.org";
            # Optional: a relay at relay.tidefell.example.org for your other devices.
            relay.enable = false;
          };
        }
      ];
    };
  };
}
```

The role pins `tidefelld` to the release in the infra flake's lock file. `web.enable = false`
leaves out the web peer, and `web.shareLinks = true` turns on
[share links](web.md#share-links).

## Set it up

After the first deployment, pair the server with a device that holds your user key. On the
server:

```sh
sudo systemctl stop tidefelld
sudo -u tidefell tidefelld pair --data-dir /var/lib/tidefell --name vps
```

Approve the code it prints on your laptop with `tidefell pair TFP1-…`, check that the
confirmation codes match, then start the service again with `sudo systemctl start tidefelld`.
See [Setting up devices](setup.md) for certificate files instead of pairing.

Join your folders on the server. Folders added this way must live under the data directory,
because the service may only write there:

```sh
sudo -u tidefell tidefell --data-dir /var/lib/tidefell \
    folder join <folder id> /var/lib/tidefell/folders/photos
```

To declare folders in the host configuration instead, list them in
`services.tidefelld.settings.folders`; see [NixOS](../nixos.md).

Add a web account to use the [web peer](web.md) at `https://tidefell.example.org`:

```sh
sudo -u tidefell tidefelld web account add --data-dir /var/lib/tidefell --username me --admin
```

## Point your devices at it

Your devices find the server through address lookup like any other peer. To connect directly
without lookup, add it as a peer on each device, with its device ID from `tidefelld id`:

```sh
tidefell peer add <server device id> --addr 203.0.113.10:7777
```

From then on, whatever one device changes reaches the server while that device is online, and
every other device picks it up from the server the next time it is online, whether or not the
first device still is.

## Without NixOS

Run `tidefelld run --config <file>` under your service manager with a configuration like:

```toml
data_dir = "/var/lib/tidefell"

[network]
bind = "0.0.0.0:7777"

[web]
bind = "127.0.0.1:8080"
```

Open UDP 7777 in the firewall, and put a TLS-terminating reverse proxy such as Caddy or nginx
in front of the web peer. Set up identity, folders and accounts as above.
