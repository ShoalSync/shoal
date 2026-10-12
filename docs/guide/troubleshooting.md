# When something goes wrong

Everything Tidefell records about problems stays on the device. Nothing is sent anywhere unless
you attach it to a report yourself.

## Status and problems

`tidefell status` shows, besides this device:

- each device this one syncs with: connected directly or through a relay, the round-trip time
  and since when, or when it was last seen and why the last connection failed;
- file content sent and received since the daemon started, and the current rates;
- each folder's state (idle, scanning, syncing, paused or error), the files it still has to
  fetch, its last scan, and the devices it is not up to date with.

A file that cannot be read, fetched, written or deleted is a problem until it succeeds:

```sh
tidefell problems              # every folder
tidefell problems <folder id>  # one folder
```

Each lists the path, what failed (permission denied, disk full, content unavailable, name not
allowed on this system, changed while writing), the error, since when and how many attempts.
Problems that clear by themselves (a disk full, content no connected device has, a file that was
changing) are tried again on every pass. Those that do not (permission denied, a read-only drive,
a name the drive refuses, an I/O error the drive reports) are tried again only when the file changes, a scan looks at it again, or
you ask: `tidefell problems <folder id> --retry`.
One file that fails does not hold up the others. A file in a directory that cannot be read is
not taken for deleted.

When a drive gets close to full, Tidefell keeps some of it free (`min_free_space` in the
[configuration](configuration.md); 2% of the drive, up to 1 GiB, by default). Files that would
cross that wait, each listed as a disk-full problem, and the folder's status says which drive is
low and how much its recycle bin and earlier versions hold there. Emptying the recycle bin is the
quickest way to make room; downloads resume by themselves. A folder whose metadata cannot be
saved stops taking changes from other devices and is in the error state until it can, and then
catches up.

## What changed, and who changed it

```sh
tidefell activity <folder id>             # the last 50 changes
tidefell activity <folder id> --limit 500
```

lists what this device applied to the folder, newest first: files added, changed, deleted,
renamed, restored or in conflict, when, and which device made each change, or "this device".
Each device keeps its own log for 30 days or 10,000 changes; it is not synced. `tidefell
files` and the web also say which device made each file's current version.

## Checking a device

```sh
tidefell doctor
```

checks, and says what to do about anything that is not right:

- that the device's clock agrees with its connected devices': conflicting edits are decided by
  modification time, so a clock more than two minutes off is a warning;
- free space where the data directory and each folder live, against what is kept free there;
- that each folder can be read and written, and is watched for changes;
- what each folder's drive does with names: whether case matters, which characters it refuses
  and how long a name may be (see [names other drives cannot keep](folders.md#names-other-drives-cannot-keep));
- the connection to the relay, and whether direct connections can work (UDP over IPv4 and IPv6,
  the kind of NAT, a sign-in page blocking the network).

Administrators find the same under **Check** on the web peer's status page.

## Metrics

`tidefelld` can serve metrics for Prometheus. Turn them on in its configuration:

```toml
[metrics]
listen = "127.0.0.1:9477"
```

and scrape `http://127.0.0.1:9477/metrics`. There is no password, so keep the address on
loopback, or on a network only Prometheus can reach. The page has content sent and received,
split between the local network and the rest; chunks served; fetch failures by reason; peers
connected directly and through a relay; and, for each folder, labelled by the first 8
characters of its ID, the files and bytes still to fetch, how long the last scan took, the
recycle bin's size and the number of problems. iroh's own metrics, named `iroh_endpoint_...`,
follow. ADR 0030 lists every name.

## Reporting a problem

```sh
tidefell diagnostics                  # writes tidefell-diagnostics-<time>.zip and lists it
tidefell diagnostics --include-paths  # keeps real file and folder names
```

The file holds this device's version and platform, its configuration, status and problems,
the doctor's checks, recent logs and any crash files. Paths and folder names are replaced by
hashes that mean nothing outside the file, device and folder IDs are shortened, and addresses
say only whether they are local or public. File contents, keys, share links and passwords are
never included. Nothing is sent: look inside, then attach it to a report yourself.
Administrators can look at and download the same from the web peer's status page.

## Logs

`tidefelld` writes its log to standard error and, as JSON lines, to `<data_dir>/logs/`. The
file is rotated at 5 MiB, and five files are kept. Under systemd the journal keeps the logs
instead (`journalctl -u tidefelld`), unless `log.file = true` in the
[configuration](configuration.md).

```sh
tidefell logs                    # the level, and where the log file is
tidefell logs --problems         # the latest warnings and errors, oldest first
tidefell log-level debug         # more detail until the daemon restarts
tidefell log-level info,tidefell_net=debug
```

The daemon keeps its last 500 warnings and errors in memory, so `tidefell logs --problems` works
even when the log file is off.

On Android the app writes the same log to its own storage, and to logcat under the tag
`tidefell`.

## Crashes

If Tidefell hits an internal error, it writes a crash file to `<data_dir>/crashes/` and stops,
rather than carrying on in a state it cannot trust. A crash file holds:

- the version and platform;
- the error and where it happened;
- a backtrace.

It holds no file contents and no keys. Under systemd, `tidefelld` is restarted automatically and
picks up from its saved state.

The next `tidefell status` says that Tidefell stopped unexpectedly and names the crash file:

```sh
tidefell crashes                 # every crash recorded, newest last
tidefell crashes --report        # write the newest one up as an issue to file
tidefell crashes --report <id>   # or another one, by its ID from `tidefell crashes --json`
tidefell crashes --dismiss       # stop reporting them in status
```

The ten most recent crash files are kept. On Android, the app tells you syncing stopped and offers
to restart it.

### Reporting a crash

`tidefell crashes --report` prints an issue for the project's tracker: a title, and a body with
the crash file in it, with your home directory replaced by `~`. Nothing is sent. Read it, then
file it yourself, and attach a diagnostics bundle if you can (see above). With `report_url` set
in `tidefelld`'s configuration to the tracker's issues page, it also prints a link that opens a
new issue already filled in; a long report is cut to fit the link, so paste the rest from the
printed body. Without it, copy the title and body into a new issue. Administrators find the same
under **Report this crash** on the web peer's status page.

```toml
report_url = "https://github.com/Tidefell/tidefell/issues"
```

`report_url` goes at the top of the file, before any `[section]`. It works with trackers that
open `<report_url>/new?title=...&body=...` as a new issue, such as GitHub and Forgejo.
