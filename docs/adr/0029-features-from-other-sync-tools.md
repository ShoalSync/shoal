# 0029: Features from other sync tools

- Status: accepted
- Date: 2026-09-27

## Context

Tidefell covers what the plan set out: peer-to-peer sync, full and partial replicas with
placeholders, a recycle bin with per-device retention, conflict copies, confirmed copies, a web
peer with share links, and phone folders. Compared with Syncthing, Resilio Sync, Dropbox,
OneDrive and Nextcloud, it lacks features that those users expect and that fit a design with
no central server:

- **Everything in a folder syncs.** Editor lock files, `.DS_Store`, `Thumbs.db`, partial
  downloads and build output reach every device, and phone folders back them up.
- **Edits are not recoverable.** The recycle bin keeps deleted files, but a file replaced by
  another device's edit is gone. ADR 0024 names removing a misbehaving member as the only
  remedy; the damage they did stays.
- **Scans poll.** `tidefelld` rescans every folder every 10 seconds. At the 100,000-entry target
  that is constant disk and CPU work, and a change still waits up to 10 seconds.
- **Devices on one network find each other through the internet.** Without internet access
  they do not find each other at all, and traffic between them may go through a relay.
- **No control over bandwidth.** Only the Android app limits when it syncs. A laptop on a
  metered link, or a home connection's upload, has no limit and no pause.
- **Partial replicas choose by recent use only.** There is no way to keep a directory on a
  device for offline use.
- **Share links only give files out.** There is no way to collect files from someone without
  an account.
- **Conflicts are counted but cannot be resolved** except by finding and deleting conflict
  copies by hand.
- **Every member can change everything.** There is no way to share a folder with someone who
  should only receive it.

## Decision

Tidefell adds the features below, in the order of the Phase 5e breakdown in the plan. Each is a
separate pull request; details not decided here are decided in that pull request and recorded
by amending this ADR.

### Ignore patterns

- A `.tidefellignore` file at a folder's root lists paths this device neither sends nor writes.
  The syntax is gitignore's, matched with the `ignore` crate: `*`, `**`, `?`, character
  classes, `/` to anchor at the root, `!` to include again, and `#` comments.
- **Per device.** The file is not synced, as in Syncthing: a phone and a laptop often want
  different patterns. A line `#include <path>` reads patterns from a file inside the folder,
  which does sync, so a folder can share a common list.
- **Built-in defaults** cover operating-system files and temporary files: `.DS_Store`,
  `Thumbs.db`, `desktop.ini`, `~$*`, `.~lock.*#`, `*.part`, `*.crdownload`, `*.swp` and the
  like. They apply before the file's own patterns, so `!` can include one of them again.
- An ignored path is left alone. A local file there is not scanned. A version from another
  device is listed as on other devices, like a file dropped from this device (ADR 0025), and
  not written. A path that becomes ignored after it synced keeps its entry: this device stops
  sending changes to it, and nothing is deleted.
- Step 3 decided:
  - The patterns are read again at every scan. `#include` works one level deep: an included
    file's own `#include` lines are comments. `.tidefellignore` is never synced, even with a `!`
    pattern.
  - The built-in defaults also cover `._*`, `.Spotlight-V100/`, `.Trashes/`, `.fseventsd/`,
    `ehthumbs.db`, `$RECYCLE.BIN/` and `*.swo`.
  - Other devices' deletes of an ignored path are not applied either.
  - Patterns govern scans and what the engine writes from other devices. A file saved through
    the engine (a mounted folder, a web upload, the documents provider) is an explicit save and
    is recorded even at an ignored path. A mount shows only recorded files, so otherwise a
    download saved as `.part` and then renamed would fail there.
  - A replica with ignore patterns still answers as a full replica: copies are confirmed per
    file from the chunks it holds (ADR 0022), so it never confirms a file it ignores.

### Watching for changes

- `tidefelld` watches each folder with the `notify` crate: inotify on Linux, and the platform
  equivalents when Windows and macOS clients come. Changes settle for one second, then the
  engine scans only the directories they touched.
- With a working watch, the full scan interval defaults to one hour, as a safety net for missed
  events. If a watch cannot be set up, for example because the inotify watch limit is reached,
  the folder falls back to its polling interval and `tidefell status` says why.
- The Android app keeps its own triggers (ADR 0025) for now.
- Step 4 decided:
  - `tidefelld`'s `watch` setting turns watching on, and it is on by default. The engine takes a
    `watch` flag. The bindings leave it off.
  - Each directory a scan walks gets its own non-recursive watch. So `.tidefell/`, ignored
    directories and symlinks are never watched, and a directory created or moved in is watched
    when the scan of its parent walks it.
  - A change rescans its directory. A new directory is walked in full. A path that is gone
    rescans everything recorded under it.
  - A scan records a delete only for a file that no longer exists on disk.
  - Changes wait at most five seconds to settle, so a file written continuously is still picked
    up. The following need a full scan: more than 4096 changed paths, a change to the ignore
    file or a file it includes, and a lost event queue.
  - A folder on a network or FUSE filesystem is not watched, because other machines' changes are
    not reported there. The same goes for a folder with such a filesystem mounted on a
    directory inside it, which is found by its device ID. It is scanned on its interval, and
    `watch_error` in the API and `tidefell status` says so.
  - That list is Linux's. There, inotify reports only changes made through this machine: SMB's
    `CHANGE_NOTIFY`, NFSv4.1 directory notifications and the FUSE protocol's invalidations do
    not reach it. Other platforms differ. Windows's `ReadDirectoryChangesW` works on SMB
    shares, because it uses `CHANGE_NOTIFY`, so the Windows client can watch them. The macOS
    and Windows clients decide their own lists.
  - A file saved through the engine rescans only its own directory.

### Local network discovery

- The engine adds iroh's mDNS address lookup (`iroh-mdns-address-lookup`) next to the address
  lookup it already configures. Devices on the same network then connect directly, including
  with no internet access.
- Discovery only finds addresses for devices the engine already knows; it never adds a peer.
  It announces this device's endpoint ID on the local network, so it can be turned off:
  `local_discovery` in the configuration, the API and the bindings, on by default. On Android
  the app holds a multicast lock while it syncs.
- Step 2 decided: devices announce the mDNS service `tidefell`, apart from other iroh
  applications. The setting is read at start: `tidefelld`'s `network.local_discovery` and the
  bindings' `Network.local_discovery` set it, and `GET /v1/status` and `tidefell status` report
  it. On a network without multicast the device logs a warning and syncs without local
  discovery. `tidefelld pair --no-relay` turns it off with the relay and address lookup.

### Version history

- When the engine replaces or deletes a file on disk, the replaced content is kept on this
  device as a previous version of the entry. It is moved into the folder's reserved directory,
  as ADR 0027 does for deleted files, so keeping it costs no copy.
- **What each device keeps.** A device keeps the versions the engine replaced there. A file
  edited in place on this device leaves nothing behind, because the editor overwrote it; the
  other devices keep that version when the engine replaces their copies. As with deletes by
  hand (ADR 0027), a folder whose history must be complete needs a second replica.
- **Retention.** The folder's retention on this device (ADR 0021) applies to versions as to the
  recycle bin. Within it, every version from the last 24 hours is kept, then one per day. The
  recycle bin's size shown in the app and web includes versions.
- **Restoring** a version writes its content as a new version of the entry, which syncs to
  every device like any edit. Nothing is rewound on other devices.
- Versions are listed per file in the API, CLI, web and Android app. History stays on its
  device and is not synced. Engine state format 7 records it.
- Step 5 decided:
  - **What is kept.** A previous version is kept when the engine writes another device's
    version over a file, when a moved entry leaves an old path with different content, and when
    a file is saved through the engine (a mount, a web upload, the documents provider) over a
    recorded one. The file moves to `.tidefell/history/`.
  - **Never missing.** The new file takes the old one's place with no moment where the path is
    missing, which a scan would record as a delete. On Linux this is an atomic exchange. Where
    that is not supported, the old file is hard-linked aside first. On filesystems with
    neither, such as FAT and Android's shared storage, a marker lets the next start put the old
    file back before the first scan. ADR 0027's move aside uses the same exchange.
  - **Checked.** A file changed since it was recorded is chunked again, so a version is always
    kept under its real content.
  - **Retention.** History ages by this device's clock, from when the version was kept. Within
    the folder's retention, every version from the last day is kept, then the newest one per UTC
    day. With retention forever, one version a day of each edited file is kept for good.
  - **Not a copy for anyone else.** Previous versions never count as holding a file for other
    devices (blobs/1 availability, ADR 0022): history is thinned, so it must not stand in for a
    confirmed copy. A thinned version whose content a conflict copy, a recycle-bin item or
    another entry still needs is handed to the kept files instead of deleted.
  - **Restoring** copies the version and saves it through the engine, so the file it replaces
    becomes a previous version in turn.
  - **Where it shows.** `versions(folder, path)` lists the current version and the previous
    ones. Their IDs are `current` and `previous:<id>`, leaving room for the conflict versions of
    step 9. They are in the API (`POST /v1/folders/{id}/versions` and `.../versions/restore`),
    `tidefell history`, a history page per file in the web, and the bindings. The Android
    screen comes with the `v0.12.0` pin.

### Bandwidth limits and pause

- A device can limit its total upload and download rates (`[limits]` in the configuration, the
  API and the bindings). Traffic to peers on the local network is not limited unless
  `limit_local = true`.
- Syncing can be paused for the whole device or for one folder, from the CLI, the web and the
  app, and stays paused across restarts. A paused folder still scans, so local changes are
  recorded, but it transfers nothing.
- Time-of-day schedules are left for later.
- Step 6 decided, for limits:
  - **What is limited.** File content only: blobs/1 chunks. Indexes and manifests are small
    next to content and are not held back.
  - **One bucket each way for the whole device.** Every connection shares it. A sender waits
    before each chunk. A receiver counts each chunk after it arrives and waits before reading
    the next, so QUIC flow control slows the sender, and the wait never counts against the
    request's deadline. The lowest limit is 64 KiB/s, at which a 1 MiB chunk still arrives
    well within its deadline. Lower values are raised to it.
  - **Busy.** ADR 0024 bounds every chunk by a deadline, and several requests at once could
    queue more than a throttled sender can send in time. blobs/1 gains `Busy
    { retry_after_ms }`: a sender whose upload backlog is over 10 seconds stops serving the
    request with it. The backlog is checked and reserved under one lock, so concurrent requests
    cannot together overshoot it, and it is checked before a chunk is read from disk. The delay
    is until the backlog has drained to half, and at least a second, so requesters do not storm
    a busy sender. The requester keeps what arrived, and asks for the rest from another peer or
    from this one after the delay. A peer that is busy is not counted as failing. Changing the
    limits starts both directions afresh.
  - **Local peers.** A connection counts as local when its selected path is a direct private,
    link-local, loopback or unique-local address. Carrier-grade NAT addresses (100.64.0.0/10),
    as overlay networks such as Tailscale use, can be anywhere and are limited.
  - **Where they are set.** `tidefelld`'s `[limits]` section (`upload_kib_per_sec`,
    `download_kib_per_sec`, `limit_local`) sets them. `PUT /v1/limits`, `tidefell limits` and
    the bindings' `setLimits` change them until restart, as the log level does. The Android app
    keeps its own setting.

### Keeping files on a partial device

- A partial replica can pin a file or a directory. Pinned files are fetched in the background,
  kept up to date and never evicted. They do not count against the folder's budget, which
  applies to everything else; the app and web show their total size.
- Pins are per device and set from the CLI, the web and the Android app.

### Upload-only share links

- A share link to a directory can be upload-only (ADR 0019). Visitors see an upload form and
  none of the directory's contents. Uploads land in that directory, and a name already taken
  gets a numbered suffix, so nothing is overwritten.
- These links have the existing expiry and password settings, plus limits on total bytes and
  number of files. They need `[web] share_links = true`, like other links, and every upload is
  logged with the link ID.

### Resolving conflicts

When two devices edit a file apart, every replica writes the same winner and keeps the other
version as a conflict copy, which is not written to disk (sync/1). Until now the user saw only a
count, could not open a conflict copy, and lost it without being asked: the winner is recorded
on disk with the merge of every version's vector, so the next edit to the file on any device
supersedes the conflict copy, which is neither recycled nor kept.

- **Conflicts last until they are resolved.** A version written to disk is recorded with its
  own vector, not the merge with its conflict copies, so later edits supersede only what the
  device actually had. A conflict copy stays until `resolve`, or until a version made from its
  own content supersedes it. sync/1 changes accordingly: a local change builds on the vector of
  the content on disk, and no longer on the conflict copies that existed when it was written.
- **Nothing to decide, nothing asked.** Concurrent versions with the same content do not
  conflict: the losing one is dropped, and content on disk that matches is recorded with both
  vectors merged, so the next edit supersedes both. A delete against an edit stays automatic, as
  today: the edit wins and the delete's device gets the file back.
- **The engine reports conflicts.** A new `ConflictDetected { folder, path }` event is emitted
  when an entry gains a conflict copy. `files` and the API list, for each conflicting version,
  the device that made it, its modification time and its size.
- **Opening a conflict copy.** `open_version(folder, path, version)` returns a read-only copy of
  a conflict copy's content, from this device or fetched from peers, so it can be compared before
  choosing.
- **Resolving.** `resolve(folder, path, keep)` takes one of:
  - *keep a version*, the current one or a conflict copy: it becomes the entry's current version
    with a vector that dominates every conflicting version, and the others go to every
    replica's recycle bin, as a delete does, so they stay restorable for the retention period;
  - *keep both*: every conflict copy becomes a new entry beside the file, named
    `<stem> (<device name>, <date>)<extension>`, with a number added if that name is taken, and
    syncs like any file. The file itself keeps its current version.
- **Prompting.** The Android app shows a notification for a new conflict ("notes.txt was
  changed on Laptop and on this phone") that opens the file's conflict screen, and marks the
  folder and the file. The web shows a banner on the folder with the same screen. The CLI has
  `tidefell conflicts [folder]` and `tidefell resolve <folder> <path> --keep <version>` or
  `--keep-both`, and `tidefell status` counts conflicts. The conflict screen lists each version
  with its device, time and size, opens it read-only, and offers keep this one, keep both and
  decide later. Deciding later leaves the mark and the count.
- **Not done:** conflict copies are not written to disk automatically, as Dropbox and Syncthing
  do: they would clutter every device, and apps could open the wrong file. Keep both gives the
  same result when it is wanted. The winner is still chosen by modification time while a
  conflict waits; a device with a wrong clock wins until the user decides.

### Read-only members

- A folder member can be read-only: replicas reject versions authored by that member's devices,
  and those devices list their local changes as not sent.
- This relies on author certificates being enforced (ADR 0024), so it is built in Phase 5c
  step 6 and specified there.

### Not adopted

- **Deferred:** rewinding a whole folder to a point in time (after version history), syncing
  symlinks and permissions (only if asked for), bandwidth schedules, and file-manager status
  icons (with the desktop clients, whose platform APIs show status).
- **Not a fit:** features that need a central service, such as project-hosted storage
  (ADR 0018), real-time co-editing, and server-side search or indexing over users' files. Photo
  timelines and face search are left to tools such as Immich pointed at a Tidefell folder.

## Consequences

- Each device stores its folders' files plus its recycle bin and version history, both bounded
  by its retention.
- History follows the same rule as deletes by hand: a device cannot keep what was overwritten
  outside the engine, so the other replicas keep it.
- Scanning costs little with a working watch, which makes the 100,000-entry target practical.
- Local discovery announces device IDs on the local network unless turned off.
- New dependencies are `ignore`, `notify` and `iroh-mdns-address-lookup`; each must pass
  `cargo-deny`'s licence and advisory checks before it is added.
- Conflict copies no longer disappear on the next edit; a folder can hold unresolved conflicts
  until someone decides, and the apps keep asking.
- Engine state format 7 comes with version history. Nothing has been released publicly, so
  nothing migrates (ADR 0026).
