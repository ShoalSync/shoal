# Setting up devices

Every device has its own key, created on first use. Devices are grouped by *user*: a user key
signs a *device certificate* for each of that person's devices, and other peers accept a
device because of that certificate. Folders are shared with users, not with individual
devices.

## The first device

Create the device key, a user key and the first certificate in one step:

```sh
tidefelld init --data-dir ~/.local/share/tidefell --name laptop
```

This writes `user.key` into the data directory unless you pass `--user-key <file>`.
**Back up the user key.** It is what proves which devices are yours; anyone who has it can
add devices to your account.

`tidefelld id --data-dir ~/.local/share/tidefell` prints the device ID and the user ID.

## More devices

On the new device, print its ID. This creates its device key:

```sh
tidefelld id --data-dir /var/lib/tidefell
```

Where the user key lives, certify that ID and copy the certificate to the new device:

```sh
tidefelld certify --user-key ~/.local/share/tidefell/user.key \
    --device <new device id> --name nas --out nas.cert
```

On the new device, finish setup with the certificate:

```sh
tidefelld init --data-dir /var/lib/tidefell --name nas --certificate nas.cert
```

The user key never has to leave the machine it was created on.

## Running the daemon

`tidefelld run --config <file>` starts the daemon. A minimal configuration only needs the data
directory; folders can be added later with `tidefell`:

```toml
data_dir = "/var/lib/tidefell"
```

See [Configuration](configuration.md) for every setting, and [NixOS](../nixos.md) to run it
as a service.

## Connecting peers

Peers find each other through address lookup and the public iroh relays by default. To keep a
peer connected, or to reach one on the local network directly:

```sh
tidefell peer add <device id> --addr 192.168.1.20:7777
tidefell peer list
```

A peer only syncs folders that both sides have and that the other device's user is a member
of.
