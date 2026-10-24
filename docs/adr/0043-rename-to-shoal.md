# 0043: Rename to Shoal

- Status: accepted
- Date: 2026-10-24

## Context

The project was called Tidefell. Before it goes public, under a permissive licence and on
public hosting (ADR 0044), it takes the name it will keep: Shoal, at shoalsync.org. It has had no
public release and no users but its owner, so the rename can be a clean break.

SHOAL alone is a registered US trademark of another company, Safehinge Primera (Reg. 8258111,
classes 9 and 42). The brand is therefore "Shoal Sync", and the reverse-domain namespace follows
the domain, shoalsync.org.

## Decision

Rename everything at once, in one mechanical commit per repository:

| | Tidefell | Shoal |
|---|---|---|
| Prose | Tidefell | Shoal; the brand line is "Shoal Sync" |
| Programs | `tidefell`, `tidefelld`, `tidefell-tray` | `shoal`, `shoald`, `shoal-tray` |
| Crates | `tidefell-*`, `tidefell_*` | `shoal-*`, `shoal_*` |
| Namespace | `org.tidefell`; app ID `org.tidefell.android` | `org.shoalsync`; app ID `org.shoalsync.android` (ADR 0002's namespace) |
| Environment | `TIDEFELL_*` | `SHOAL_*` |
| Files | `.tidefell`, `.tidefellignore`, `.tidefellpriorities`, `tidefelld.toml`, `/var/lib/tidefell`, `tidefell.log`, `…\Programs\Tidefell` | `.shoal`, `.shoalignore`, `.shoalpriorities`, `shoald.toml`, `/var/lib/shoal`, `shoal.log`, `…\Programs\Shoal` |
| Wire | ALPNs `tidefell/…`, mDNS `tidefell` | ALPNs `shoal/…`, mDNS `shoal` |
| Crypto | `tidefell/sig/1\0`, `tidefell 2026-10-18 …`, `tidefell-device-key 1`, … | `shoal/sig/1\0`, `shoal 2026-10-24 …`, `shoal-device-key 1`, … |
| Windows installer | `tidefell.wxs`, `…\Programs\Tidefell` | `shoal.wxs`, `…\Programs\Shoal`, with the same UpgradeCode, so it replaces a Tidefell install |

**No compatibility with Tidefell.** Shoal devices do not talk to Tidefell peers: ALPNs and
signature and key-derivation contexts differ. They do not read Tidefell state, configuration or
folder markers either. Devices are paired and folders synced again. Before upgrading, copy out any
encrypted folder's data held only by a storage peer.

What stays as it was:
- historic ADRs (0001–0042) and the release notes of Tidefell's versions, which record what
  happened under the old name;
- names that belong to the current hosting, which a later move replaces: the Forgejo URLs and
  host keys, the `TIDEFELL_READ_KEY` secret and the `tidefell-release` runner user.

A CI check keeps the old name from coming back: `git grep -i tidefell` must find nothing outside
those places, this record, ADR 0044, and the README's "formerly Tidefell" line and licence grant.

## Consequences

- Every protocol vector and fuzz seed is made again, and the Android app installs as a new app
  beside the old one.
- The commit is mechanical, so blame can skip it (`.git-blame-ignore-revs`).
- Anyone with Tidefell data re-pairs and re-syncs; the owner's devices are the only ones.
