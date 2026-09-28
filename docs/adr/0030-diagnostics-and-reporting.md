# 0030: Diagnostics and reporting

- Status: accepted
- Date: 2026-09-27

## Context

When a user reports a problem, Tidefell has little to show for it:

- **Logs.** `tidefelld` writes plain text to standard error, at the level `RUST_LOG` sets. Under
  systemd the journal keeps it; elsewhere, as on many NAS systems, nothing does. The bindings can
  only log to standard error, which Android discards, and the Android app never turns logging
  on. An Android report comes with no engine logs at all.
- **Crashes.** Nothing records a panic or an uncaught exception. A panic in a background task,
  such as a folder's scan loop, ends that task for good: the folder silently stops syncing and
  only standard error hears about it. An Android crash leaves the system dialog and nothing else.
- **Status.** Folders report pending files and conflicts; peers report a name and shared
  folders. Nothing says whether a peer is connected, directly or through a relay, when it was
  last seen, why a file is not syncing, or how fast data moves. A file that fails to write is
  only a warning in the log.
- **History.** Events are live only, so "where did my file go?" has no answer after the fact.
- **Monitoring.** An always-on peer (ADR 0018) has nothing to monitor it with.

Whatever is added must fit a system with no central service (ADR 0018) and users who chose it
partly for privacy: nothing leaves a device unless its user sends it.

## Decision

### Logs

- `tidefelld` and the bindings share one logging setup. Standard error stays human-readable. A
  rotating file in `<data_dir>/logs/` keeps JSON lines, five files of 5 MiB. Paths, folders,
  peers and IDs go in structured fields, never only in the message text, so a diagnostics bundle
  can redact them reliably. Under systemd (`JOURNAL_STREAM` is set) the journal has the logs and
  no file is written unless `log.file = true`.
- The level is `info` by default, set with `log.level` in the configuration, and can be changed
  at runtime through the API, the CLI (`tidefell log-level`) and the bindings.
- On Android the engine also logs to logcat. The bindings expose `log(level, tag, message)`, so
  the app's own messages land in the same file and form one timeline.
- The last 500 warnings and errors are kept in memory and available through the API, the CLI
  (`tidefell logs --problems`) and the bindings.

### Crashes

- `tidefelld` and the bindings install a panic hook. It writes a crash file to
  `<data_dir>/crashes/` with the version, platform, thread, message, location and a captured
  backtrace, then runs the previous hook. The ten most recent crash files are kept.
- **A panic in an engine task is fatal.** Locks it held are poisoned and state in memory may be
  inconsistent, so the task is not restarted. `tidefelld` exits with an error so its service
  manager restarts it from its saved state. In the bindings the node stops and reports that it
  crashed, and the Android app offers to restart syncing.
- The Android app installs an uncaught-exception handler that writes to the same directory
  before passing the exception on.
- On the next start, a crash file not yet seen is reported in `tidefell status`, the web and the
  app ("Tidefell stopped unexpectedly"), with the option to report it.
- Step 2 decided:
  - **Log file.** It is `<data_dir>/logs/tidefell.log`, rotated by size to `tidefell.1.log`
    and so on (`tracing-appender` only rotates by time). The level filter applies to the outputs
    only, so the recent-problems buffer always gets warnings and errors.
  - **Logcat.** Android logs to logcat with `paranoid-android` under the tag `tidefell`.
    `tracing-logcat` does not build alongside `serde_json`.
  - **Bindings.** `init_diagnostics(data_dir, level)` starts logging and installs the panic
    hook in one call. `record_crash` lets the app write its own crash files, in the same
    format.
  - **API and CLI.** `GET` and `PUT /v1/log` read and set the level, `GET /v1/log/problems`
    lists recent problems, and `GET /v1/crashes` and `POST /v1/crashes/dismiss` handle
    crashes. `/v1/status` carries the newest unseen crash. The CLI has `tidefell logs
    [--problems]`, `tidefell log-level [LEVEL]` and `tidefell crashes [--dismiss]`.
  - **One crash file per process.** Later panics, such as those on locks the first one
    poisoned, are added to the first panic's file, so a cascade cannot push the cause out of
    the ten kept.
  - **Supervision.** Engine tasks are polled in place under `catch_unwind`, not spawned, so
    shutting the engine down still waits for them. Panics in the fetch driver's requests and in
    blocking scans and compaction are passed on rather than swallowed.
  - **Web notice.** The crash notice in the web comes with step 3's status views.

### Status

- **Peers:** connected or not, the path (direct, relay or none), round-trip time, last seen, and
  the last connection error.
- **Folders:** a state (idle, scanning, syncing, paused (ADR 0029) or error), pending files and
  bytes, the time and duration of the last scan, and for each peer when the folder last finished
  exchanging changes with it.
- **Problems.** The engine keeps, per folder, the paths it failed to scan, fetch or write, with a
  kind (permission denied, disk full, content unavailable, name not allowed on this system,
  changed while writing), a message, since when, and how many attempts. A problem clears when
  the path succeeds.
- **Transfer:** current rates and totals since start, sent and received.
- All of it is in the API, the sdk, the bindings, `tidefell status` and `tidefell problems`, the
  web and the app.
- Step 3 decided (the app's screens come with the next Android pin):
  - **Peers** are the ones the engine keeps connected plus any that connected since it
    started. A record per device holds when its session started, when it ended and the last
    error, including failed dials, which were only logged before. The path and round-trip time
    are read from the session's selected path when asked. The device's name comes from its last
    session.
  - **Transfer** counts file content where the rate limiter sees it, so local peers count too.
    Index exchanges are not counted. Rates are averaged over the last ten seconds.
  - **Folder state** is the first of: error (unavailable, or the last scan failed), paused,
    scanning, syncing (files pending and a connected peer shares the folder), idle. Pending
    files and bytes come from the same plan materialising follows. A peer is up to date once an
    exchange finished in its current session; later changes are pushed as they happen.
  - **Problems** are kept by path and operation (scan, fetch, write, delete). A scan replaces
    the scan problems of the paths it covered with what it found, so a path read, gone or now
    ignored clears. A materialise pass replaces the fetch, write and delete problems with its
    own. A directory's problems are keyed with a trailing `/` and cleared only by a scan that
    read that directory. Content unavailable is recorded only when a connected peer was asked.
    An unavailable folder or a crash stops the pass and shows in the folder's state instead of
    once per file. One path that fails no longer stops a scan or a materialise pass.
  - **Only a path known to be gone is deleted.** A scan used to take any path it could not
    look at for deleted, so the files in a directory this device could list but not enter, or
    not read at all, were deleted on every device. Now only "not found" is a delete; other
    errors are problems.
  - None of it is persisted: it describes the current run.
  - `GET /v1/status` gains `peers` and `transfer`; folders gain `state`, `error`,
    `pending_files`, `pending_bytes`, `last_scan_ms`, `last_scan_duration_ms`, `peers` and a
    `problems` count; `GET /v1/folders/{id}/problems` lists them. The CLI shows them in
    `tidefell status` and `tidefell problems [folder]`, and the bindings have `peerStates`,
    `transfer` and `pathProblems`. The web shows each folder's state on the folder list and its
    problems on its page, and an administrators' **Status** page lists the peers, the transfer
    totals and a crash not yet dismissed, which step 2 left for here.

### Diagnostics bundle

- `tidefell diagnostics`, `POST /v1/diagnostics`, the bindings, a "Share diagnostics" action in
  the app (the share sheet) and a download in the web produce one zip file:
  - a summary: version, platform, and on Android the API level and device model;
  - the configuration, with secrets removed;
  - the status above, and each folder's mode, sync options, retention, file count, total size
    and recycle-bin size;
  - problems, recent logs, crash files and the doctor report below.
- **Redacted by default.** Every path and folder name becomes a hash plus its extension, keyed
  with a random key for that bundle, so names are consistent within a bundle but cannot be
  looked up. Device, user and folder IDs are shortened to 8 characters. Peer IP addresses are
  replaced by their kind (local or public, IPv4 or IPv6). `--include-paths`, or a switch in the
  app and the web, keeps real paths.
- **Never included:** file contents, keys, share-link tokens, account passwords and the engine's
  state files.
- The user sees what the bundle contains before sharing it, and attaches it to a report
  themselves. Nothing is uploaded.

### Doctor

`tidefell doctor`, and a "Check" action in the app and the web, report pass, warning or failure
with advice for:

- the clock difference with each connected peer, over two minutes a warning, since conflicts
  are decided by modification time. sync/1's `Hello` gains the sender's clock for this;
- free space on the folder and data volumes;
- folder and reserved directories readable and writable;
- file-watch limits once watching lands (ADR 0029);
- relay reachability and direct connectivity (UDP, IPv4 and IPv6, NAT), from iroh's network
  report;
- on Android: All files access, the battery-optimisation exemption, background restrictions and
  notification permission;
- engine state loading cleanly.

### Activity history

- Each file lists the device that made its current version and when.
- Each device keeps a per-folder activity log of what it applied: files added, changed, deleted,
  renamed, restored or in conflict, which device made each change, and whether it came from a
  peer or from this device. It keeps 30 days or 10,000 entries, whichever is less, and is not
  synced. `tidefell activity`, the API, the web and the app show it.

### Metrics

- `tidefelld` can serve Prometheus metrics: `[metrics] listen = "127.0.0.1:9477"`, off by
  default. They include bytes sent and received (local and remote peers), chunks served, fetch
  failures by reason, connections by path, scan duration, pending files, recycle-bin size and
  problems per folder, and iroh's own metrics. Folders are labelled by the first 8 characters of
  their ID.
- `tidefell/infra` gains an option to scrape it.

### Reporting a crash

- There is no collecting service. When a crash file is new, the CLI, the web and the app offer
  "Report this crash". It opens a new issue on the project's tracker, pre-filled with the
  version, platform, message, location and backtrace, with home directories stripped, which the
  user reviews and submits, and can attach a diagnostics bundle to.
- The tracker URL is `report_url` in the configuration and the app's settings. It defaults to
  the public tracker once the repositories are public; until then the report is shown for the
  user to copy.

### Usage telemetry

None. Anything that would send usage data needs its own ADR, and would be off by default, asked
once, show exactly what it sends, and go to a collector the user can run themselves.

## Consequences

- An issue report can carry logs, crashes, status and checks from the device that had the
  problem, including Android, without any service run by the project.
- The data directory grows by at most about 25 MiB of logs, ten crash files and each folder's
  activity log.
- Redaction depends on log records keeping paths and IDs in fields; a review item for new log
  lines.
- A panic now restarts `tidefelld` or stops the Android node, instead of leaving a folder
  silently stalled.
- sync/1's `Hello` gains a clock field. Nothing has been released publicly, so nothing
  migrates (ADR 0026).
- New dependencies, such as `tracing-appender`, a logcat writer, a zip writer and a Prometheus
  encoder, must pass `cargo-deny`'s licence and advisory checks.
