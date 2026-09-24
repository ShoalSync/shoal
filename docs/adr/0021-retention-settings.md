# 0021: Recycle-bin retention is visible and editable

- Status: accepted
- Date: 2026-09-24

## Context

A folder's recycle-bin retention is set when it is created, through the configuration file or
`folder create`, and cannot be changed afterwards except by editing the configuration. People
cannot see how much space the recycle bin uses or when items will be purged, so "deletes are
recoverable" is hard to trust or manage.

## Decision

- **Change retention at runtime** for folders not managed by the configuration file: a new
  API endpoint, `tidefell folder retention <id> <days|forever>`, the web peer (for admin
  accounts) and the Android folder settings. Shortening retention purges expired items at
  the next purge, not immediately, and says so first.
- **Show the recycle bin's size and next expiry** in `tidefell status`, the folder page of
  the web peer and the Android app, alongside the existing per-item expiry.
- **Empty the recycle bin** on demand, for one item or all of them, with confirmation. This
  is a local purge on each replica that runs it; it is not propagated to other peers.
- Retention stays per replica, as today. Each replica keeps deleted content for as long as
  it chooses, so a phone can keep a day while the NAS keeps a year.

## Consequences

- Folders from the configuration file still report `Configured` for changes, as for other
  settings.
- The sdk and OpenAPI gain a retention update and recycle-bin summary fields.
