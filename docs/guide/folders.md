# Folders

A *folder* is a directory tree synced between devices. It has a random ID and a list of member
users; every device of a member user may sync it.

`tidefell` talks to the daemon on the same machine through `<data_dir>/control.sock`. Pass
`--data-dir` (or set `TIDEFELL_DATA_DIR`) if the data directory is not
`~/.local/share/tidefell`, and `--json` for machine-readable output.

## Creating and joining

On the first device:

```sh
tidefell folder create ~/Photos
```

This prints the new folder's ID. On each of your other devices, join it at a local path:

```sh
tidefell folder join <folder id> /srv/photos
```

Both commands take `--retention-days` (how long deleted files stay in the recycle bin, default
30; 0 keeps them forever) and `--member` to set the members explicitly. By default the only
member is your own user.

## Sharing with other people

Add another user as a member, using the user ID they get from `tidefelld id`:

```sh
tidefell folder share <folder id> <their user id>
```

They then join the folder on their devices with `tidefell folder join`. `tidefell folder
unshare` removes a member; their devices stop receiving changes.

## Day to day

```sh
tidefell folder list                 # folders on this device
tidefell files <folder id>           # live files, with sizes and modification times
tidefell folder scan <folder id>     # pick up local changes now instead of at the next scan
tidefell events                      # follow changes as they happen
tidefell status                      # this device, its user and the daemon version
```

Folders listed in the configuration file are managed there and cannot be changed with
`tidefell`. Folders created or joined with `tidefell` are stored in the data directory and
survive restarts.

## Partial replicas

A device with little space can keep only some of a folder's files. Add `--partial <bytes>` when
creating or joining:

```sh
tidefell folder join <folder id> ~/Photos --partial 5000000000
```

Every file still appears in `tidefell files`, but files made on other devices are
*placeholders* until you fetch them:

```sh
tidefell fetch <folder id> 2024/beach.jpg    # prints where the file is on disk
```

Files you create or edit on the device are synced as usual. When the folder's files on disk
exceed the budget, the least recently used ones become placeholders again. A file made on this
device is only removed once another connected device has a copy.

On Linux, a partial replica can also be [mounted](mounting.md), so its files open on demand
from any program.

## Leaving a folder

```sh
tidefell folder remove <folder id>
```

The device stops syncing the folder and forgets its metadata. Files on disk are kept.
