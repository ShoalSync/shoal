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
| `network.relay` | `{ mode = "default" }` (public iroh relays), `{ mode = "custom", urls = [...] }` or `{ mode = "disabled" }`. |
| `network.address_lookup` | Publish and resolve device addresses via DNS. |
| `network.bind` | UDP address to listen on. |
| `network.peers` | Peers to keep connected, each with a `device` ID, optional direct `addrs` and an optional `relay` URL they are reachable through. |
| `folders` | Folders managed by the configuration: `id`, `path`, `mode` (`full` or `partial`, which needs `budget_bytes`; see [partial replicas](folders.md#partial-replicas)), `members`, `retention_days`, `scan_interval_secs`, and on Linux `mount` (see [mounting a folder](mounting.md)). |
| `web` | Enables the [web peer](web.md): `bind`, `secure_cookies`, and `share_links` (off by default; see [share links](web.md#share-links)). |

Folders in the configuration are read-only to `tidefell`; change them here and restart. Folders
added with `tidefell folder create` or `join` are kept in the data directory instead.
