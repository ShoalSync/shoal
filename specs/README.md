# Protocol specifications

One file per ALPN; bump the ALPN string on any breaking change.

| ALPN | Spec |
| --- | --- |
| `tidefell/sync/1` | [sync-v1.md](sync-v1.md) |
| `tidefell/blobs/1` | [blobs-v1.md](blobs-v1.md) |
| `tidefell/pair/2` | [pair-v2.md](pair-v2.md) |
| `tidefell/sync/2` | [sync-v2.md](sync-v2.md): encrypted folders, alongside sync/1 |
| `tidefell/blobs/2` | [blobs-v2.md](blobs-v2.md): encrypted folders, alongside blobs/1 |
| `tidefell/storage/1` | [storage-v1.md](storage-v1.md): what a storage peer keeps of an encrypted folder |

Formats that are not protocols of their own:

| Format | Spec |
| --- | --- |
| Folder keys, sealing and grants | [encryption-v1.md](encryption-v1.md) |

The vectors of `sync/1`, `blobs/1` and `pair/2` are made by `crates/proto`'s tests. Those of the
encrypted folders are made by a second implementation written from the specs, in
[`vectors/gen/`](vectors/gen/), which `nix flake check` runs (`spec-vectors`); the Rust code is
checked against them.
