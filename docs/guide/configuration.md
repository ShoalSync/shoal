# Configuration

`tidefelld run --config <file>` reads a TOML file. Check a file without starting the daemon
with `tidefelld config check <file>`. `tidefelld config schema` prints its JSON Schema, which
is also attached to every release as `tidefelld-config-X.Y.Z.schema.json`.

This example shows every section:

```toml
{{#include ../../crates/daemon/tidefelld.example.toml}}
```

## Settings

| Key | Meaning |
| --- | --- |
| `data_dir` | Keys, certificates, metadata, the block store and the control socket. |
| `watch` | Watch folders for changes and scan only where something changed, with a full scan every hour. On by default. A folder that cannot be watched, such as one on or containing a network filesystem mount (NFS, SMB, FUSE), or past the system's watch limit, is scanned every `scan_interval_secs` instead, and `tidefell status` says why. |
| `network.relay` | `{ mode = "default" }` (public iroh relays), `{ mode = "custom", urls = [...] }` or `{ mode = "disabled" }`. |
| `network.address_lookup` | Publish and resolve device addresses via DNS. |
| `network.local_discovery` | Announce this device and find your other devices on the local network with mDNS, so they connect directly, even with no internet access. On by default; it only finds devices this one already knows. |
| `network.bind` | UDP address to listen on. |
| `network.peers` | Peers to keep connected, each with a `device` ID, optional direct `addrs` and an optional `relay` URL they are reachable through. |
| `folders` | Folders managed by the configuration: `id`, `path`, `mode` (`full` or `partial`, which needs `budget_bytes`; see [partial replicas](folders.md#partial-replicas)), `members`, `retention_days`, `scan_interval_secs`, `direction` (`both` or `send-only`) and `local_deletes` (`everywhere` or `this-device`; see [send-only folders](folders.md#send-only-folders-and-deletes-that-stay-on-one-device)), and on Linux `mount` (see [mounting a folder](mounting.md)). |
| `limits` | `upload_kib_per_sec` and `download_kib_per_sec`, unlimited if unset and at least 64: how fast file content is sent to and received from other devices. Traffic to devices on the local network is not limited unless `limit_local = true`. `tidefell limits` shows them, and changes them until the daemon restarts. |
| `log` | `level` (`info` by default, or per module such as `info,tidefell_net=debug`; `RUST_LOG` overrides it) and `file`: JSON lines in `<data_dir>/logs/`, rotated at 5 MiB with five files kept, written unless the system journal keeps the logs. `tidefell log-level` changes the level until the daemon restarts, and `tidefell logs --problems` shows the latest warnings and errors. |
| `web` | Enables the [web peer](web.md): `bind`, `secure_cookies`, and `share_links` (off by default; see [share links](web.md#share-links)). |

Folders in the configuration are read-only to `tidefell`; change them here and restart. Folders
added with `tidefell folder create` or `join` are kept in the data directory instead.
