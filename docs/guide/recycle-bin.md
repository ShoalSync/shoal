# The recycle bin

Deleting a file on any device deletes it everywhere, but its last version is kept in each
full replica's recycle bin until the folder's retention period expires. The other devices keep
it by moving their copy into the folder's `.tidefell/deleted/` directory, so the recycle bin
takes no more space than the file did.

The device where a file was deleted by hand, outside Tidefell, has no copy left to keep: it
still lists the file, and restoring it fetches the content from the other devices. A folder
that exists on only one device cannot recover files deleted there by hand; add a second
device, such as an [always-on peer](always-on.md), if that matters.

## From the command line

```sh
tidefell recycle list <folder id>
```

lists deleted files, newest first, with their size, when they were deleted and when they will
be purged. Restore by path
(the most recent deletion of that path) or by entry ID from the listing:

```sh
tidefell recycle restore <folder id> holiday.jpg
```

A restored file is a new version: it reappears on every device that syncs the folder.

## From the browser

On the [web peer](web.md), open a folder and choose **Recycle bin**, then **Restore** next to
the file.

## Retention

Retention is set per folder when it is created or joined (`--retention-days`, or
`retention_days` in the configuration). Entries older than that are purged, and their chunks
are released. A retention of 0 keeps deleted files forever. Age counts from when this device
recorded the delete, by its own clock, so a device whose clock is wrong cannot make the others
purge its deletes early.

Retention belongs to each device, not to the folder: a phone can keep deleted files for a day
while a NAS keeps them for a year. Change it for a folder added with the CLI or API with:

```sh
tidefell folder retention <folder id> 90
tidefell folder retention <folder id> forever
```

Shortening retention does not purge anything immediately: files past the new limit are purged
at the next hourly purge, and the command says how many first and asks before going ahead.
Folders from the configuration file keep the retention set there.

`tidefell status` and `tidefell folder list` show how many files each recycle bin holds, their
total size and when the next one will be purged.

Files removed from one device only with `tidefell remove-here` wait in that device's recycle
bin too, listed as `removed here`. Other devices still have them. Restoring one puts it back on
this device only, and emptying the bin frees the space it took here.

## Previous versions

When a file is replaced by another device's edit, this device keeps the version it had. The
same goes for a file you replace by saving through Tidefell: in a mounted folder, by uploading
in the browser, or from the Android file picker. Previous versions stay on the device that kept
them and are not synced.

```sh
tidefell history <folder id> notes/todo.txt                 # the current version, then earlier ones
tidefell history <folder id> notes/todo.txt --restore previous:1a2b…
```

Restoring makes that version the file's content again on every device, and the version it
replaces is kept in turn. In the browser, each file has a **history** link.

Previous versions share the folder's retention with the recycle bin, and their size counts
toward it. Every version from the last day is kept, then one per day. With a retention of 0,
which keeps deleted files forever, one version a day of every edited file is kept for good.

A file edited in place on this device, by an editor, leaves nothing behind here. The other
devices keep that version when your edit replaces their copies.

## Emptying the recycle bin

To free space on one device without waiting for retention, delete files from its recycle bin
permanently:

```sh
tidefell recycle empty <folder id> holiday.jpg   # one file, by path or entry ID
tidefell recycle empty <folder id>               # everything
```

Both ask for confirmation unless given `--yes`. This only affects the device you run it on:
other devices keep their own copies in their recycle bins, and can still restore the file.
