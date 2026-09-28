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
One file that fails does not hold up the others. A file in a directory that cannot be read is
not taken for deleted.

## Checking a device

```sh
tidefell doctor
```

checks, and says what to do about anything that is not right:

- that the device's clock agrees with its connected devices': conflicting edits are decided by
  modification time, so a clock more than two minutes off is a warning;
- free space where the data directory and each folder live;
- that each folder can be read and written, and is watched for changes;
- the connection to the relay, and whether direct connections can work (UDP over IPv4 and IPv6,
  the kind of NAT, a sign-in page blocking the network).

Administrators find the same under **Check** on the web peer's status page.

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
tidefell crashes --dismiss       # stop reporting them in status
```

The ten most recent crash files are kept. On Android, the app tells you syncing stopped and offers
to restart it.
