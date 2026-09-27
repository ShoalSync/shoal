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
30; 0 keeps them forever; change it later with `tidefell folder retention`, see
[the recycle bin](recycle-bin.md#retention)) and `--member` to set the members explicitly. By
default the only member is your own user.

### Folders on removable or network drives

Creating or joining a folder creates its directory and writes `.tidefell/folder` in it, naming
the folder. After that, Tidefell never creates the directory again. If the directory or that file
is missing, as when the drive holding it is not mounted, the folder is **unavailable**: nothing is
scanned, written or deleted there, and `tidefell status` says why. Once the drive is back, the
folder syncs again by itself, within a minute. Without this, an empty mount point would look like
every file had been deleted, and the deletes would reach your other devices.

Folders created before this check was added are given the file the first time Tidefell starts, if
their directory holds any file Tidefell already knew about.

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

## Ignoring files

A `.tidefellignore` file at a folder's root lists paths this device neither sends nor writes.
It uses gitignore syntax:

```text
# Build output and logs
build/
*.log
!important.log
/scratch.txt
#include .shared-ignore
```

- The file applies to this device only and is never synced, so a phone and a laptop can ignore
  different things. An `#include <path>` line reads more patterns from a file inside the folder,
  which does sync, so devices can share a list.
- Operating-system and temporary files are ignored on every device: `.DS_Store`, `._*`,
  `Thumbs.db`, `desktop.ini`, `~$*`, `.~lock.*#`, `*.part`, `*.crdownload`, `*.swp` and the
  like. A `!` line includes one of them again.
- An ignored file on this device is not sent. Another device's file at an ignored path is
  listed, but not written here.
- Ignoring a file that already synced deletes nothing. This device stops sending changes to
  it, and deletes from other devices leave it alone.
- Patterns are read again at every scan.
- A file you save through Tidefell itself, in a [mounted folder](mounting.md), by uploading in
  the browser or from the Android file picker, is synced even at an ignored path.

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
device is only removed once a full replica has confirmed holding it (see below), so without a
full replica the folder can grow past its budget.

## Send-only folders and deletes that stay on one device

Two settings change how a folder syncs on one device. Other devices keep their own.

- `--direction send-only` sends this device's new and changed files, but never writes other
  devices' changes here: nothing is downloaded, and edits and deletes made elsewhere leave the
  files here alone.
- `--local-deletes this-device` makes a file deleted here, by any program, disappear from this
  device only. It stays on every other device, and it is not downloaded again, even when a new
  version arrives, until you fetch it.

Set them when creating or joining a folder, or change them later:

```sh
tidefell folder join <folder id> ~/Camera --direction send-only --local-deletes this-device
tidefell folder sync <folder id> --direction both
```

To delete a file everywhere from such a device, delete it with the app or the API rather than
by hand. A device with either setting does not count as a full replica (see below), because it
does not hold everything.

To free space on one device while keeping a file on the others, remove it from that device
only:

```sh
tidefell remove-here <folder id> 2024/beach.jpg
```

The file moves to this device's [recycle bin](recycle-bin.md), marked `removed here`, and is
deleted for good when retention expires or you empty the bin. Restoring it puts it back on
this device only.

## Is it safe elsewhere?

Every device keeps track of which full replicas, such as a NAS or an
[always-on peer](always-on.md), have confirmed holding the files it made. Until one has, the
device may hold the only copy:

- `tidefell files` marks such files `only on this device`;
- `tidefell status` counts them per folder;
- the web peer marks them `only on this server`.

Another partial replica holding a file does not count, because it may drop the file again. A
confirmation says the replica held the file at that moment; it does not protect against that
replica losing its disk later.

On Linux, a partial replica can also be [mounted](mounting.md), so its files open on demand
from any program.

## Leaving a folder

```sh
tidefell folder remove <folder id>
```

The device stops syncing the folder and forgets its metadata. Files on disk are kept.
