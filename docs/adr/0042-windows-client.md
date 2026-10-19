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
  - **Where a sync root can be.** A partial folder must be on a local NTFS volume and not
    inside another provider's sync root (such as Documents redirected into OneDrive); otherwise
    it is refused when added, with the reason.
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
