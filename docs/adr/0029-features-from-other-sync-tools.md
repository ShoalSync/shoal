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

### Watching for changes

- `tidefelld` watches each folder with the `notify` crate: inotify on Linux, and the platform
  equivalents when Windows and macOS clients come. Changes settle for one second, then the
  engine scans only the directories they touched.
- With a working watch, the full scan interval defaults to one hour, as a safety net for missed
  events. If a watch cannot be set up, for example because the inotify watch limit is reached,
  the folder falls back to its polling interval and `tidefell status` says why.
- The Android app keeps its own triggers (ADR 0025) for now.

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

### Bandwidth limits and pause

- A device can limit its total upload and download rates (`[limits]` in the configuration, the
  API and the bindings). Traffic to peers on the local network is not limited unless
  `limit_local = true`.
- Syncing can be paused for the whole device or for one folder, from the CLI, the web and the
  app, and stays paused across restarts. A paused folder still scans, so local changes are
  recorded, but it transfers nothing.
- Time-of-day schedules are left for later.

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

- The engine gains `resolve(folder, path, keep)`. The version to keep, either the current one or
  a conflict copy, becomes the entry's current version with a vector that dominates every
  conflicting version. The other versions go to every replica's recycle bin, as a delete does.
- The CLI, the web and the Android app list a file's conflicting versions and resolve them.

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
- Engine state format 7 comes with version history. Nothing has been released publicly, so
  nothing migrates (ADR 0026).
