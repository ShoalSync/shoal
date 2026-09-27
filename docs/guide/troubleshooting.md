# When something goes wrong

Everything Tidefell records about problems stays on the device. Nothing is sent anywhere unless
you attach it to a report yourself.

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
