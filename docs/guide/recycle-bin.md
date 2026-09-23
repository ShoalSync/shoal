# The recycle bin

Deleting a file on any device deletes it everywhere, but its last version is kept in each
full replica's recycle bin until the folder's retention period expires. Its content stays in
the block store until then.

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
are released. A retention of 0 keeps deleted files forever.
