# Mounting a folder (Linux)

On Linux, `tidefelld` can show a folder at a mount point with FUSE. With a partial replica,
this gives a directory that lists every file but stores only the ones opened recently. The
design is recorded in [ADR 0017](../adr/0017-fuse-mounts.md).

## Requirements

- The `fuse3` package, which provides `fusermount3`, and `/dev/fuse`. In a container, FUSE must
  be enabled for it; on Proxmox, set `features: fuse=1`.
- `tidefelld` running as the user who will use the files, for example as a systemd user
  service (below). The mount is private to that user.

## Setting it up

Add `mount` to a folder in the [configuration file](configuration.md). The folder's `path`
becomes backing storage: leave it alone and use the mount point instead.

```toml
[[folders]]
id = "…"
path = "/home/me/.local/share/tidefell/photos"
mode = "partial"
budget_bytes = 10000000000
mount = "/home/me/Photos"
members = ["…"]
retention_days = 30
```

The daemon mounts the folder when it starts and unmounts it when it stops. If the daemon was
killed without unmounting, it clears the stale mount on its next start.

## Using it

- **Listing** shows every file with its real size and date, whether or not it is stored here.
- **Opening** a file that is not stored here (a placeholder) fetches it from a connected device
  first. With no device online that has it, opening fails with an I/O error.
- **Saving** works as usual, including programs that save by writing a temporary file and
  renaming it. New content is synced when the file is closed.
- **Renaming** and **deleting** work on any file. Renaming a placeholder does not fetch it, and
  deleted files go to the other devices' [recycle bins](recycle-bin.md).
- **Empty directories** exist only until the daemon restarts, since empty directories are not
  synced.
- Symlinks, hard links, extended attributes and permission changes other than the executable
  bit are not supported.

Files not opened recently become placeholders again when the folder exceeds its budget. Open
files are never evicted.

## Running as a systemd user service

```ini
# ~/.config/systemd/user/tidefelld.service
[Unit]
Description=Tidefell daemon

[Service]
ExecStart=/usr/local/bin/tidefelld run --config %h/.config/tidefell/tidefelld.toml
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now tidefelld
```

The [NixOS module](../nixos.md) runs a sandboxed system service for servers and does not
support `mount`.
