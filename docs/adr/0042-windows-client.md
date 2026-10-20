# 0042: The Windows client

- Status: accepted
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
