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

- Step 4 decided, first part (the doctor):
  - **Clock.** `Hello` gains `clock_ms: Option<i64>`, the sender's clock, as its last field. A
    `Hello` whose frame ends before it is read with it absent, so devices from before it still
    connect both ways; every other message must be complete. The offset is measured when the peer's `Hello`
    arrives and kept with the peer's record.
  - **Checks.** Engine state loaded; the clock of each connected peer (a warning over two
    minutes); free space on the data volume and each folder's, once per volume (a failure
    under 100 MiB, a warning under 1 GiB or 5%); each folder's root read and checked writable
    without writing in it, where a scan could sync the file, and its reserved directory written
    with a probe file; an unavailable folder fails; a folder not watched warns
    with the system's watch limit; each relay connected or not; UDP over IPv4 and IPv6, a NAT
    that maps each destination differently, and a captive portal, from iroh's network report
    (its `unstable-net-report` feature), skipped with relays turned off, since the report
    probes them; and how many devices are connected directly or
    through a relay. The app adds its Android checks.
  - `GET /v1/doctor`, `tidefell doctor`, the bindings' `doctor`, and **Check** on the web's
    status page. Peers gain `clock_offset_ms`. The checks read the disks, so the API and web
    run them off the async runtime.
- Step 4 decided, second part (the bundle):
  - **Entries.** `summary.json` (version, platform, what the app passed, such as the Android
    API level, and whether there is a log file), `config.json` (the configuration as the daemon
    read it, or the app's settings), `status.json`, `problems.json`, `recent-problems.json`,
    `doctor.json`, the last 5 MiB of the current and previous log files, and the crash files.
    The builder writes only these; it never walks the data directory, so keys, account
    passwords, share-link tokens, `seen` markers and state files cannot get in.
  - **Redaction.** One random key per bundle. Every path component becomes the first 12 hex
    digits of its keyed BLAKE3 hash, plus its extension if that is 1 to 8 letters or digits
    after a dot that is not the first character. Known names (folder roots, the data
    directory, `$HOME`, every file and recycle-bin path, and paths in the configuration such as
    mount points) are replaced where they stand as a name, not inside a word; one followed by
    `/` starts a path that runs to `": "`, a quote or the line's end, spaces and all, and is
    hashed whole. Quoted paths are hashed whole too, and any other token with a `/`, except
    source locations in backtraces. Path fields of JSON log lines and the configuration are
    hashed whole. Hashing a hash leaves it alone, so a name reads the same everywhere in a
    bundle. If the names cannot be matched, no bundle is made. URLs keep only scheme and host.
    64-digit IDs keep 8 digits. IP addresses become `local-ipv4`, `public-ipv6` and so on; a
    bare IPv6 address needs four hex digits, so `engine::materialize` and timestamps stay.
    `include_paths` keeps names; IDs and addresses are shortened either way. Device names,
    such as "Graeme's phone", are kept: they say which device is which.
  - `POST /v1/diagnostics` returns the zip; `tidefell diagnostics [--include-paths] [-o file]`
    writes it and lists what it holds; the web's status page shows every entry's content
    before its download, which builds the bundle again with a new key; the bindings write it
    to a directory the app gives and return its entries for the app to show.

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
- Step 5 decided (the app's screen comes with the next Android pin):
  - **What is logged.** The folder's metadata notes what each version it stores does to its
    entry, compared with the entry's current version before it: added, changed (new content),
    deleted, renamed (a new path, perhaps with new content too), restored (live after a
    delete), or a conflict. A conflict is logged when a version made apart from the current one
    arrives and one of them waits for a decision, whichever wins. A version already known, or
    one that needs no decision, logs nothing. The device is the version's author, and the time
    is when this device stored it.
  - **Where.** One JSON line per change in `activity` in the folder's state directory, written
    after each save of the folder's metadata. A failure to write it is logged and does not stop
    the save. It is rewritten without entries past 30 days or beyond 10,000 when the folder is
    opened and whenever it reaches 20,000 lines.
  - Files gain `author_name`, the name this device knows for the author, or the start of its
    ID. `GET /v1/folders/{id}/activity?limit=` (100 by default) lists the log newest first,
    with each change's device, its name and whether it was made here; `tidefell activity
    <folder> [--limit]` prints it and the bindings have `activity`. The web shows who made
    each file's current version on the folder's page, and an **Activity** page per folder.

### Metrics

- `tidefelld` can serve Prometheus metrics: `[metrics] listen = "127.0.0.1:9477"`, off by
  default. They include bytes sent and received (local and remote peers), chunks served, fetch
  failures by reason, connections by path, scan duration, pending files, recycle-bin size and
  problems per folder, and iroh's own metrics. Folders are labelled by the first 8 characters of
  their ID.
- `tidefell/infra` gains an option to scrape it.
- Step 6 decided:
  - **Format.** One OpenMetrics page (`application/openmetrics-text; version=1.0.0`) at
    `GET /metrics`, ending with `# EOF`. It has no authentication; `listen` should be loopback
    or a network only the scraper reaches, and the NixOS module's `openFirewall` does not open
    it. A bind failure stops `tidefelld` at start, like the web peer's. Only `tidefelld` serves
    it; the app and the control API do not.
  - **Counters,** since the daemon started: `tidefell_content_sent_bytes_total` and
    `tidefell_content_received_bytes_total` with `network="local"` or `"remote"`, local as
    bandwidth limits see it (a direct path to a private, link-local or loopback address);
    `tidefell_chunks_served_total`, counted once a chunk is written to the stream; and
    `tidefell_fetch_failures_total` with `reason`: `request` (a request to a peer failed),
    `busy` (a peer at its upload limit asked for a retry later: back-pressure rather than an
    error, counted so a limit that is too low shows), `missing` (a peer that was not busy did
    not send a chunk it was asked for), `unavailable` (no connected peer had the content) and
    `storage` (a chunk could not be stored). A paused folder's cancelled fetches are not
    failures.
  - **Gauges,** read when the page is built: `tidefell_connections` with `path="direct"` or
    `"relay"`, both always present; and per folder, labelled `folder` with the first 8
    characters of its ID, `tidefell_folder_pending_files`, `tidefell_folder_pending_bytes`,
    `tidefell_folder_scan_duration_seconds` (the last scan, once there is one),
    `tidefell_folder_recycle_bin_items`, `tidefell_folder_recycle_bin_bytes` and
    `tidefell_folder_problems`. A removed folder's series go with it. Labels never hold paths,
    names or messages.
  - **iroh's metrics** follow, as iroh names them under `iroh_endpoint_`.
  - The page reads every folder's state, so it is built off the async runtime.
  - `tidefell/infra`'s scrape option needs a `tidefelld` that knows `[metrics]`, since the
    configuration rejects unknown keys, so it comes with the infra pin after the next release.

### Reporting a crash

- There is no collecting service. When a crash file is new, the CLI, the web and the app offer
  "Report this crash". It opens a new issue on the project's tracker, pre-filled with the
  version, platform, message, location and backtrace, with home directories stripped, which the
  user reviews and submits, and can attach a diagnostics bundle to.
- The tracker URL is `report_url` in the configuration and the app's settings. It defaults to
  the public tracker once the repositories are public; until then the report is shown for the
  user to copy.
- Step 7 decided (the app's screen comes with the next Android pin):
  - **`report_url`** is the tracker's issues page, over HTTP or HTTPS without a query, checked
    when the configuration is. A report links to `<report_url>/new?title=...&body=...`, which
    GitHub and Forgejo open as a new issue. The default is one constant, empty until the
    repositories are public. `tidefelld` reads it from the top of its configuration; the app
    passes its setting to the bindings.
  - **The report** is built from the crash file, whatever wrote it: the panic hook, with
    later panics appended, or the app's own. Its title is "Crash: " and the message, cut at
    100 characters; its body the message, the crash file in a code block fenced longer than
    any backticks in it, and how to attach a diagnostics bundle. `$HOME` and any `/home/<name>`,
    `/Users/<name>`, `/root` or `C:\Users\<name>` are replaced by `~` where they start a path,
    since backtraces name the machine that built Tidefell.
  - **The link** percent-encodes everything but unreserved characters and stays under 8,000
    bytes: if it would not, lines are dropped from the end of the crash file and a note says to
    paste the rest, and if even the message is too long, the link's body only asks for the
    report to be pasted. Every surface shows the whole title and body as well.
  - **Crashes** gain an `id`, their file's name. Only a name like `crash-<digits>-<source>.txt`
    that exists in `crashes/` is written up; anything else is not found.
  - `GET /v1/crashes/report?id=`; `tidefell crashes --report [id]`, for the newest crash not
    dismissed, else the newest, printing the report and the link or how to file it, and never
    opening a browser; **Report this crash** on the web's status page, for administrators; and
    `crash_report(data_dir, id, report_url)` in the bindings. Reporting does not dismiss the
    crash: dismissing stays its own step, once the user has filed it.

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
