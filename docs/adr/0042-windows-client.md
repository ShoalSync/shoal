# 0042: The Windows client

- Status: accepted; its runners superseded by [0045](0045-github-hosting.md)
- Date: 2026-10-19

## Context

Phase 8 brings Tidefell to Windows: a partial replica whose folder shows in Explorer with
placeholders that open on demand (the plan's exit criterion). Everything below the daemon is
already portable or close to it (proto, chunker, crypto, net, fetch, meta), but the daemon and
its clients assume Unix: the control API is a Unix socket (ADR 0010), key files and the data
directory are private by file mode, the scanner reads the executable bit and inode numbers from
Unix metadata, durable renames and free space use Unix calls, the watcher puts one watch on each
directory as inotify needs, logs go to journald, and placeholders are a FUSE mount (ADR 0017).
Phase 7 (iOS) is set aside; a Windows VM runs CI.

Windows shows cloud files through the Cloud Files API (cfapi): a provider registers a directory
as a *sync root*, files in it can be placeholders (real NTFS files whose data is not on disk),
and the system calls the provider back when a placeholder's data is first read. A sync root
belongs to a user: it is registered, connected and called back in that user's session, and
Explorer shows its state (online only, on this device, always keep) with no extra code.

## Decision

- **tidefelld runs per user, not as a service.** Placeholders, Explorer and the user's files
  live in the user's session, so `tidefelld run` runs there too, started at sign-in by the tray
  app and stopped with it. A Windows service (session 0, a machine account) can neither connect
  a user's sync root nor read a profile without impersonation, and an always-on Windows peer is
  not this phase's aim. `tidefelld run` from a console still works, for development and tests.
  The plan's "tidefelld as a Windows service" is replaced by this.
- **Data under the user's profile, private by ACL.** The data directory defaults to
  `%LOCALAPPDATA%\Tidefell`; the CLI and the tray find it the same way (`TIDEFELL_DATA_DIR`
  still overrides). What a Unix build makes private by file mode (the data directory, key files,
  web accounts and shares) is made private on Windows by a DACL granting only the user and
  SYSTEM, set when the file or directory is created.
- **The control API over a named pipe.** On Windows the API (ADR 0010) is served on
  `\\.\pipe\tidefell-<hash of the data directory>`, in place of `control.sock`, with a DACL
  for the user only. The hash is of the directory's canonical path with case folded, so the CLI
  and the daemon agree however it is spelled. The SDK gains a pipe target beside the Unix one;
  the CLI's `--socket` takes a pipe name there. The HTTP API itself is unchanged.
- **Partial replicas are sync roots; full replicas are plain folders.** A partial folder's
  `path` is registered and connected as a sync root by a new crate, `fs-cfapi`, beside `fs-fuse`
  on the same engine support (ADR 0016). There is no separate mount point: the folder is where
  the user sees it.
  - Every live file not on disk is a placeholder with its size and modification time. Reading
    one calls back the provider, which runs the engine's `open` and streams the fetched content;
    a file no connected peer can serve fails the read with the cloud provider's offline error.
  - Eviction dehydrates a file instead of deleting it, so it stays visible as online-only.
    Explorer's "Always keep on this device" maps to the engine's pins (ADR 0029), and "Free up
    space" to eviction of that file.
  - Deletes and renames in the sync root arrive as cfapi notifications and become the engine's
    `delete` and `rename` (ADR 0017); new and changed files are found by the watcher and scans
    as in any folder.
  - **`fs-cfapi` owns every write inside a sync root.** A placeholder is already on disk where
    its file goes, so the engine's usual way of placing a file, a temporary file renamed into
    place, would replace it with an ordinary file and lose what Explorer shows. Inside a sync
    root the engine instead asks `fs-cfapi` to create a placeholder for a new file, update or
    dehydrate one when a peer changes it, and fill one in place when it is read: `open` gains a
    form that fetches content without placing the file. The generic placing path never writes
    in a sync root.
  - **A sync root holds only the user's files.** Its reserved directory (history, kept and
    deleted files, staging) lives in the data directory instead of in `.tidefell` inside the
    folder, and the folder's identity check (ADR 0037) uses the sync-root registration in place
    of the marker file.
  - **Where a sync root can be.** A partial folder must be on a local NTFS volume, not inside
    another provider's sync root (such as Documents redirected into OneDrive), and not inside a
    hidden or system folder (see the spike's amendment); otherwise it is refused when added, with
    the reason.
  - **The scanner never reads a placeholder's data**, which would download it: a file whose
    attributes say its data is not on disk is a placeholder, not a change. Another tool's
    online-only files (OneDrive's) inside a full replica's folder are reported as problems
    (ADR 0030), not read and not silently skipped.
- **File semantics.**
  - Windows has no executable bit, so a Windows file's bit is *unknown*, not false: every
    comparison with a recorded version ignores it (otherwise each scan would see every
    executable file as changed), and a local change keeps the bit its entry's current version
    has, so editing a script on Windows does not strip it for Linux peers. New files are not
    executable.
  - A file's identity is its NTFS file ID, in place of the inode; free space comes
    from `GetDiskFreeSpaceExW` (ADR 0037), the volume from its serial number, and a folder on a
    network drive is detected by its drive type, as Linux detects one by filesystem type.
  - Renames use the handle-based rename with POSIX semantics where the volume supports it, so
    a file another program holds open can still be replaced; where it cannot, the write waits
    and is retried, and a file held open too long is a problem (ADR 0030), never lost. An atomic
    "rename if absent" is a rename that does not replace, not a hard link, which exFAT and ReFS
    lack.
  - The reserved `.tidefell` directory and the root marker are hidden. Paths are long-path
    aware (the programs' manifests opt in), and names Windows refuses are already problems
    (ADR 0033).
  - One recursive watch per folder, instead of one per directory, since every Windows watch
    holds its directory open.
- **Logs** go to the data directory's log files (ADR 0030), as off journald on Linux; the
  allocator settings the Unix build applies by re-executing itself (ADR 0035) are set in the
  process at start.
- **The tray app** (`tidefell-tray`, Rust, in the monorepo) starts and supervises the daemon,
  shows its state, opens folders, pauses and resumes, and pairs a device: it shows the pairing
  code and approves the other device over the control API's pairing calls. Settings beyond
  those stay in the CLI and the web peer.
- **Installer and release (provisional).** A per-user MSI (no administrator rights) installs
  `tidefelld`, `tidefell` and the tray app and starts the tray at sign-in. This holds only if a
  program installed without package identity can register a sync root that Explorer shows in
  full; the first step on the Windows runner checks that, and if it needs identity (an MSIX or
  a sparse package), the installer and its signing are decided again in an amendment.
  Authenticode signing, which SmartScreen trusts, needs a code-signing certificate and is
  decided before the first Windows release; until then the installer is unsigned. Every Windows
  asset is also signed with cosign (ADR 0041).
- **Windows releases are built on a runner that runs nothing else**, as Linux releases are
  (Phase 5c step 5): a second Windows machine or VM registered only with the label
  `release-windows`, never the CI runner that runs pull requests. Until one exists, releases have
  no Windows assets.
- **CI.** A Windows VM runs as a Forgejo runner with the label `windows`: it builds the
  workspace for Windows and runs its tests, and the sync-root tests run there, as the FUSE tests
  run where `/dev/fuse` is. Sync roots and Explorer need a user's desktop session, so the runner
  runs as an ordinary user in a session signed in at boot, not as a service; the VM has NTFS,
  the MSVC build tools and long paths enabled. Crates that cannot build on Windows (`fs-fuse`,
  the simulation's Linux-only parts) are gated by target, so `cargo build --workspace` works
  there.

## Amendment: privacy from Windows' defaults (2026-10-19)

Setting an explicit DACL takes Win32 calls that are `unsafe` in Rust, and the workspace forbids
unsafe code. Windows' defaults already give what the DACLs above were for, so no DACL is set:

- **The data directory** is under the user's profile (`%LOCALAPPDATA%`), and what is created
  in it inherits the profile's access control: the user, SYSTEM and administrators, as a Unix
  build's mode 0700 and 0600 allow the user and root. A data directory set outside the profile
  does not have that; `tidefell doctor` says so.
- **The control pipe** keeps a named pipe's default security: full access for its creator,
  SYSTEM and administrators, and read access for everyone else, which cannot send a request.
  It refuses remote clients.

If a tighter access list is ever needed, it comes from a maintained crate that wraps those calls
safely, not from unsafe code here.

## Amendment: the spike's result (2026-10-20)

On the Windows runner (Windows 11 25H2, an ordinary user, no package identity), a test program
registered a sync root through `StorageProviderSyncRootManager`, Windows listed it among its
active sync roots (as Explorer's navigation pane does), a placeholder made there had no data on
disk, and reading it filled it through the provider's callback. So the installer stays a per-user
MSI without package identity, and is no longer provisional.

One limit came out of it: the shell does not show a sync root inside a hidden folder, such as
`AppData` (where temporary directories are): its registration is written but the folder cannot
be resolved. "Where a sync root can be" therefore also refuses a partial folder inside a hidden
or system folder. The spike stays as `fs-cfapi`'s test `tests/spike.rs`.

## Amendment: file semantics as built (2026-10-20)

Step 3 of the plan, tested on the Windows runner, settled how each Unix behaviour above is
done without unsafe code:

- **File IDs.** A file's NTFS file ID and its metadata come from one handle opened only to read
  attributes, so a file another program holds without sharing is still seen; a file that
  cannot be opened at all has no ID, as on a filesystem without inodes (ADR 0036).
- **Renames.** The standard library's rename already uses the handle-based rename with POSIX
  semantics, so a file another program has open, shared as programs usually share, is
  replaced. A program holding a file without letting it be deleted makes the rename fail as
  access denied, as missing permission does; opening the file for deletion tells the two apart.
  Such a file is busy: its replacement is tried again on each pass, a problem meanwhile, and
  what was fetched for it is removed rather than left in the reserved directory.
- **Rename if absent** stays a hard link where the volume has them, as NTFS does, then the
  old name removed, and elsewhere a check then a rename, as on Linux: no safe call renames
  without replacing.
- **Network drives.** A folder's path is resolved first, which turns a mapped drive letter
  into its share's UNC path; a UNC path is a network drive.
- **Hidden reserved directory.** `.tidefell` is hidden when a folder is created or joined, by
  `attrib +H`, which keeps its other attributes; the root marker is inside it. A directory that
  cannot be hidden still works.
- **Long paths.** The standard library already passes long paths in their `\\?\` form; the
  programs' manifests (embedded by `tidefelld`'s and `tidefell`'s build scripts) make the rest
  of Windows accept them too.
- **One recursive watch** holds up under a burst of 3,000 new files (a test). One limit
  remains in `notify` 8: a watch whose buffer overflows with `ERROR_NOTIFY_ENUM_DIR` is
  dropped without an event, so changes would wait for the hourly full scan. `notify` 9 turns
  that into a rescan; until it is released, step 6 adds a check that the watch is still alive.
  (As built in step 6: `notify` 8 is patched in `third_party/notify` with notify 9's
  handling, so an overflow in either form is a rescan and the watch is kept.)
- **Logs** need nothing: no journal is detected, so they go to the data directory's files.
- **Allocator settings.** The allocator reads them as the process loads, before any code of
  ours runs, and changing them in the process takes an unsafe call. So the tray app (step 7)
  starts the daemon with them in its environment; a daemon started by hand runs without them,
  which costs only memory, as on Unix when it cannot start itself again.
- **Doctor.** `tidefell doctor` warns of a data directory outside the user's profile.

## Amendment: the second spike, the seam's primitives (2026-10-20)

`fs-cfapi/tests/primitives.rs` tried each operation the engine's seam relies on, on the
Windows runner. The test is ignored by default; CI runs it so it prints its findings.

- **The provider's own changes raise no callbacks.** Deletes and renames made by the daemon's
  own process in its sync root raise none. Other processes' changes carry their process IDs.
- **Placeholders deleted while the provider is not connected go without a callback.** Windows
  allows the delete and reports nothing when the provider connects again. The provider
  therefore keeps a durable record of the placeholders it made. When it connects, one that is
  gone counts as a delete by the user, with the mass-delete hold (ADR 0037), as a file deleted
  while the daemon was stopped does on Linux. Decided by the project's owner.
- **Pins hydrate by themselves.** "Always keep on this device" makes Windows fetch the file
  through the provider's callback.
- **Dehydrating and converting work in place.**
  - Dehydrating keeps the file's size, modification time and file ID.
  - A file written in the usual way can be made an in-sync placeholder afterwards.
  - Both need a handle opened for writing, exclusively.
- **Only metadata that does not follow a placeholder shows its data is elsewhere.**
  `symlink_metadata` does, and so does a handle opened on the reparse point. The engine reads
  only those (step 5).
- **Renaming a file over a dehydrated placeholder first downloads the old content.** A
  placeholder in the way is therefore removed before a file takes its place.
- **Fetches are answered from threads of their own.** The callback returns at once.
  - Windows runs about two of one process's fetches at a time.
  - It cancels a fetch after 60 s with no data, then retries it once.
  - Reporting progress keeps a fetch alive, as progressive hydration does, so a large file
    reports progress as its chunks arrive.
  - A fetch that cannot be served fails at once with "network unavailable". Sending nothing
    would leave the program reading the file waiting two minutes.
- **`cloud-filter` is patched.** As published, two of its bugs aborted the process: a
  callback without process information, and a failure the platform refused. Failing a fetch was
  refused, and other arguments were misused. The fixes are in a copy at `third_party/cloud-filter`
  (its `PATCHES.md`), until a release upstream carries them.

## Amendment: the provider's design (2026-10-20)

Two more questions for the spike settled the rest. The design below replaces the earlier text
where they differ.

- **The reserved directory stays in the folder.** `.tidefell` stays in the sync root as a hidden,
  plain directory, not in the data directory as planned above. This replaces the "sync root
  holds only the user's files" text:
  - A placeholder moved into it becomes "not in sync", so Windows refuses to free it. History,
    the recycle bin and kept copies therefore stay whole as long as nothing under `.tidefell` is
    ever marked in sync.
  - The engine's moves stay on one volume.
  - The root marker and its identity check are unchanged.
  - The provider's own moves into it raise no callbacks.
- **Folders in a sync root are placeholders.** Another program renaming a plain folder of
  placeholders raised no callback at all. Renaming a placeholder folder raises one rename for
  the folder, which becomes the engine's directory rename. So the provider converts every
  folder it makes or finds into an in-sync placeholder.
- **The engine records the placeholders.** For each placeholder, the engine durably records the
  entry and the version vector it showed, beside its records of files on disk.
  - **Deletes:** a deleted placeholder builds on that vector, so a newer remote change survives
    as a conflict.
  - **Missing at a scan:** a recorded placeholder that is gone at a scan is a delete, under the
    same mass-delete hold as files (ADR 0037). This includes one deleted while the daemon was
    off, as decided above.
  - **The record comes second:** the provider records a placeholder only after creating it, so
    a crash between the two never reads as a delete.
- **The provider reconciles.** On the engine's events, as the FUSE mount refreshes on them, it
  makes the folder match the engine:
  - **Not on disk:** a live file not on disk gets a dehydrated, in-sync placeholder. Its
    identity blob names the entry and content.
  - **Changed elsewhere:** a placeholder of a file changed elsewhere is updated, and it stays
    dehydrated.
  - **No longer live:** a placeholder of a file no longer live is removed.
  - **Placed by the engine:** a file the engine placed is made an in-sync placeholder. That
    happens only on an exclusive handle, and only while the file is still what the engine
    recorded, because in-sync tells Windows the data may be freed.
  - **`.tidefell`:** nothing under it is touched.
- **Fetches serve the version the placeholder shows.**
  - A fetch runs on a thread of its own and reports progress while the engine waits on peers.
  - It fails with "network unavailable" when the content cannot be had. It fails the same way
    when the placeholder's version is no longer current; the reconciler then updates the
    placeholder.
  - A file once filled is recorded by a scan of its directory.
- **Callbacks from other programs:**
  - **Deletes and renames** become the engine's `delete` and `rename`.
  - **State changes** carry pins from Explorer to the engine. They also carry "Free up space":
    the scan finds the placeholder and drops the file's record.
- **Pins:** Windows fills a pinned placeholder through the provider itself. A pin made through
  the engine sets the placeholder's pin state, and the provider fills it, so one component owns
  each placeholder's data: the engine never writes where a recorded placeholder is, and replaces
  one only if it is unrecorded, as one made just before a crash is.

## Amendment: the provider as built (2026-10-21)

The provider (`fs-cfapi`), with the engine's placeholder records, passes its tests on the
Windows runner, three sync roots at once in one process. Its tests cover:
- filling on reading;
- other programs' deletes, renames and folder renames;
- a delete made while disconnected;
- a version changed before the first read;
- "Free up space";
- a pin made through the engine.

What the runs taught:

- **Fill only placeholders, never read one.** Windows asks for data only for a placeholder,
  so a file being filled that the engine still records as on disk was freed and not yet
  scanned. Its record is dropped before the fetch, and the fill never reads chunks from the
  file it fills. The engine had read its own placeholder, which waited on the fill that was
  waiting on it.
- **Callbacks hand their work to the provider's own thread**, and read placeholders' state
  through a plain handle, without an oplock.
- **Disconnecting is kept off the runtime.**
  - What was seen: with three roots in one process, one test disconnected its root on one of
    its runtime's threads, and every fetch in the process stood still. That lasted until
    Windows gave up on them, two minutes later.
  - What it was not: a disconnect alone, off any runtime, holds up no other root's fill (the
    spike's Q14).
  - What fixed it: disconnecting off the runtime.
  - So dropping a `SyncRoot` disconnects on a thread of its own, and `disconnect` and
    `unregister`, which block, are called off the runtime. A test drops a root on the
    runtime's thread while another root fills.
- **A fill opens no file.**
  - What was seen: with several roots in one process filling, now and then a fill stood still
    for two minutes, until Windows gave up on it. Every fetch of the process waited with it.
  - Where it stopped: step-by-step logs put it in the engine's read of a chunk from its block
    store, a plain file in the data directory. That read took the whole two minutes, holding
    the store's lock.
  - What the spike showed (Q19, nothing but `cloud-filter` and `std`): a fill that writes a
    file, then reads it back before answering, waits 60 s and more on the read. It is freed
    only as Windows cancels a fill pending, over and over. Writing the file took 2 ms. Opening
    a placeholder whose fill is pending also waits for the fill (Q18); reading its attributes
    does not.
  - Not told apart: whether the wait is Windows Defender scanning on open (on, on the
    runner), or the cloud files filter. It matters to neither the fix nor users, who have
    both.
  - The fix: a fill fetches its chunks from peers into memory, 16 MiB at a time in file
    order, and hands them over. (Since ADR 0032's amendment of 2026-10-22, the next 16 MiB is
    fetched while one is handed over: at most two windows are held.) It reads nothing from
    the block store or the folder, even what is there, so a chunk already here is fetched
    again. Under its locks, the engine looks at files only through their attributes.
  - Not yet avoided: an encrypted folder's fill still reads through the block store, where
    its content is opened (ADR 0023). Before such a folder is a sync root, its opened chunks
    must reach the fill in memory too. (Done in step 6: its sealed chunks are fetched a window
    at a time, opened in memory and handed over, never stored or read back.)
  - What came first, and stays: a process-wide lock keeps fills' writes to Windows apart from
    connecting, disconnecting and asking the sync root manager. Tried first, it was not the
    cause, but it keeps a safe order.
- **Not yet done.**
  - **Eviction:** the engine's own eviction still removes a file, and the reconciler then makes
    its placeholder again. Dehydrating in place is for step 6, with the daemon's wiring (done:
    see the daemon's amendment).
  - **Making a file in sync** compares its size and modification time with the engine's, not
    yet its file ID. (Done in step 6: the engine's files carry the file ID last scanned, and a
    file moved over the one recorded, with the same size and time, is left until scanned.)

## Amendment: the daemon's sync roots (2026-10-21)

- **One task owns every root.** `tidefelld` connects each partial folder as a sync root when it
  starts and as folders are added, and makes a root's placeholders match its folder on the
  engine's events, as a FUSE mount is refreshed. A folder removed is disconnected and its
  registration removed, so Explorer stops showing it; a folder whose root is unavailable is
  disconnected only. Folders are looked at again each minute too, for a root back after its
  drive was away or one refused before.
- **Stopping disconnects first.** Each root is disconnected, waiting for the callbacks in
  flight, before the engine shuts down; none is left to a dropped root's detached disconnect.
- **A folder that cannot be a sync root still syncs**, as a partial replica without
  placeholders, and the reason is logged once: one inside a hidden or system folder, on a
  network share, or inside or around another sync root, another provider's or another
  Tidefell folder's (the registration of the folder itself does not count). The volume's
  kind is not looked up: Windows refuses to register a root it cannot keep placeholders on,
  with its reason. Refusing such a folder when it is added is not done: the tray app adds no
  folders (they are added with the CLI and the web peer), so the daemon's log is where the
  reason shows.
- **Eviction dehydrates in place.** The engine takes a per-folder hook that frees a file in
  place; the daemon sets `fs-cfapi`'s for each root it connects and clears it before
  disconnecting. Eviction makes its usual checks under the folder's lock, then calls the hook
  outside it: the file, opened shared with no one, is dehydrated only if it is the in-sync,
  unpinned placeholder of the version the engine evicts, and the engine then records its
  placeholder, as for a file freed in Explorer. A file not freed (open elsewhere, not yet in
  sync) is removed as before, and the reconciler makes its placeholder.
- **Stopping without a signal.** Windows has no signal another program can send a console
  process, so the control API gains `POST /v1/shutdown`, which stops the daemon as a signal
  does; the tray app uses it, as the daemon's test does. Ctrl-C, Ctrl-Break and closing the
  console stop a daemon run by hand.

## Amendment: the tray app (2026-10-21)

- **Toolkit: `native-windows-gui`.** The tray needs a notification-area icon, a menu, message
  boxes and, to approve another device's pairing code, a window with a text box, all without
  unsafe code of ours. `native-windows-gui` has every one of them and its own message loop, and
  brings only `winapi` (already a dependency), `bitflags` 1 and `lazy_static`; its last release
  is from 2022, but it wraps a Win32 API that does not change. The alternatives were
  `tray-icon` and `muda` with `winit`'s event loop, which have no text box (a webview or
  `egui` would add one, at many times the size), and `slint`, a whole toolkit. A test on the
  Windows runner brings the icon up in the runner's session and quits.
- **The UI thread only shows.** A supervisor runs on a thread of its own, with the runtime that
  talks to the daemon, and wakes the UI with a notice. What the icon's tooltip and menu say
  comes from a platform-neutral function of the daemon's status and folders, and the
  supervisor is platform-neutral too: both are tested on Linux against a real `tidefelld`.
- **Supervising.** The daemon is `tidefelld.exe` beside the tray, run with the configuration
  `tidefelld.toml` in the data directory (`%LOCALAPPDATA%\Tidefell` unless
  `TIDEFELL_DATA_DIR` says otherwise, as for the CLI), with the allocator settings in its
  environment and no console window. A daemon already answering on the control pipe, as one
  started by hand, is used as it is. One that stops unasked is started again, waiting longer
  each time, until it has failed five times in two minutes; the tray then shows why, from what
  the daemon wrote to its standard error (kept in `tidefelld.stderr.log` in the data
  directory), and offers to start it again.
- **Quitting** stops the daemon with `POST /v1/shutdown` and waits for it, so it disconnects
  its sync roots first; the tray never kills it. A daemon started by hand that the tray is
  using stops too.
- **First, the menu:** what the daemon is doing, each folder (opened in Explorer), pausing and
  resuming, and quitting. Setting up a device (`init` or pairing as a new device) and
  approving another device follow.
- **Setting up.** A device with no configuration offers two ways. As the first of your
  devices: once the user has read that the new user key must be backed up, `tidefelld init`
  makes it in the data directory. Joining your other devices: `tidefelld pair` runs, and a
  window shows the pairing code to enter on a device with your user key, then the
  confirmation code to check against it; closing the window stops pairing. Either way the
  tray then writes a configuration of only the data directory, and the supervisor starts the
  daemon. Folders are added with the CLI.
- **Approving another device.** "Add a device…" opens a window for the pairing code the new
  device shows; the daemon's pairing calls (`POST /v1/pairings`, then approve or decline) run
  on a thread of their own, and the window shows the new device's name and the confirmation
  code to check before approving. Only a daemon that holds your user key can approve, and one
  that does not says so. Closing the window before deciding declines.
- **Windows' own events.** A menu or message box runs Windows' message loop while it shows,
  so the tray's notices can arrive then: nothing the UI holds borrowed across one is changed
  by them, and a window's own handler never unbinds itself, but asks for it on the next
  notice.

## Amendment: the installer and the release, for now (2026-10-21)

Decided by the project's owner: ship unsigned for now, and build without a Windows release
runner until one exists.

- **The installer** is a per-user MSI from `packaging/windows/tidefell.wxs`, built with `wixl`
  (msitools) on Linux rather than WiX on Windows, so it builds from Nix and on the release
  runner. It installs `tidefell-tray`, `tidefelld` and `tidefell` under
  `%LOCALAPPDATA%\Programs\Tidefell`, adds that to the user's `PATH`, starts the tray at
  sign-in (the user's `Run` key) and puts it in the Start menu; it needs no administrator
  rights. Every version has the same upgrade code, so installing one replaces another, and an
  older one does not replace a newer one. Uninstalling leaves the data directory and the
  folders. A zip of the same programs is released beside it.
- **Tested on the runner:** CI builds the MSI from placeholder programs, and the Windows job
  installs it, checks what it put where, and uninstalls it, checking everything went.
- **Not signed with Authenticode**, so SmartScreen warns before running the installer. Every
  Windows asset is still signed with cosign, as the others are.
- **Built on the CI runner.** A release's Windows programs are built on the Windows CI runner,
  from the tag, in a target directory made afresh, and packaged and signed on the release runner
  (see the release runner's page for what this does not protect against). This replaces "Windows
  releases are built on a runner that runs nothing else" until a second Windows machine exists.

## Amendment: the exit criterion, on real machines (2026-10-22)

- **Two runners, at once.** The `Exit criterion` workflow runs a Linux job, a real `tidefelld`
  on a Linux runner, and the Windows job together. The Linux runners' jobs run in containers no
  other machine can reach, so neither side listens for the other: they meet through iroh's
  public relays and then connect directly over the LAN, as two devices behind ordinary routers
  do. The test requires the direct path.
- **Pairing codes go between the jobs as commit statuses** on the run's commit (named
  `exit-criterion/<run>/…`), written with the run's token, as nothing else carries a value from
  one running job to another. The Linux side pairs as a new device, with a new code each time
  one expires while the Windows runner is busy; the Windows side, set up as the first device
  with a partial folder, approves it with the tray's approval code.
- **What the shell reports** was read on the runner, not taken from documentation: its listing
  leaves known extensions out of names, and gives an online-only file's status as "Available
  when online", and a fetched one's as "Available on this device".
- **Nightly, not a pull request's check:** it needs both runners free at once and the public
  relays.

## Consequences

- One codebase keeps working on Linux and Android unchanged: each Windows difference is a
  target-gated implementation behind the same function the Unix build uses.
- The SDK's public API gains a pipe target, a minor change before 1.0 (ADR 0026).
- The engine gains a seam for folders whose files another component places: `fs-cfapi` on
  Windows. Android and FUSE keep the engine's own placing.
- Windows releases need a second Windows machine, kept for releases only.
- A Windows device is a member and a partial or full replica like any other. It cannot be an
  always-on peer or a storage peer while it runs only when its user is signed in.
- Every Windows-specific behaviour needs the Windows runner to test; until it exists, Windows
  code is only type-checked by cross-compiling.
