# The control API

`tidefelld` serves a REST/JSON API on the Unix socket `<data_dir>/control.sock`, readable only
by the daemon's user (mode 0600). `tidefell` uses nothing else, so anything it does can be
scripted against the API.

- Every route is under `/v1`. `GET /v1/openapi.json` returns the OpenAPI document, which is
  also printed by `tidefelld api schema` and attached to each release as
  `tidefell-api-X.Y.Z.openapi.json`.
- `GET /v1/events` is a stream of server-sent events: folders added, removed or changed, and
  files written to or removed from disk.
- Errors are JSON objects with a `kind` (`bad_request`, `not_found`, `conflict` or `internal`)
  and a `message`.

With curl:

```sh
curl --unix-socket /var/lib/tidefell/control.sock http://localhost/v1/status
curl --unix-socket /var/lib/tidefell/control.sock http://localhost/v1/folders
```

From Rust, the `tidefell-sdk` crate (MIT OR Apache-2.0) has the request and response types and a
typed client:

```rust,ignore
let client = tidefell_sdk::Client::unix("/var/lib/tidefell/control.sock");
for folder in client.folders().await? {
    println!("{} {}", folder.id, folder.path);
}
```
