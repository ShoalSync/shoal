# 0026: Compatibility from 1.0

- Status: accepted
- Date: 2026-09-26

## Context

Tidefell has not been released publicly. Until now every release could change wire protocols,
the REST API, configuration and on-disk state without keeping older clients working, and it
has. After 1.0, people will run devices on different versions: a phone that has not updated
yet still has to sync with a NAS that has. Keeping every old version working forever would
make the protocols and the engine harder to change.

## Decision

- **Before 1.0, no compatibility.** Releases before 1.0 may break protocols, the API,
  configuration and on-disk state. Nothing migrates, and ADRs do not need a migration plan.
- **From 1.0, one version back.** When a release replaces a protocol version (a new ALPN such
  as `tidefell/sync/2`) or an API version (`/v2`), it keeps serving the previous one alongside
  it. Peers offer both ALPNs and use the newest they share.
- **Deprecation first.** The release that adds a new version marks the previous one
  deprecated. While it is deprecated:
  - a peer that connects with it is logged as a warning, and the device's status names the
    outdated peer and its version, so the user sees which device to update;
  - API responses on the old version carry `Deprecation` and `Sunset` headers, and the CLI and
    web peer show the warning;
  - the release notes and `docs/` say when it will be removed.
- **Then removal.** A deprecated version is removed no sooner than two minor releases, and no
  sooner than six months, after it was deprecated, whichever is later. A peer on a removed
  version is refused with an error that says to update.
- **State moves forward.** Configuration and on-disk state written by an older 1.x release are
  upgraded automatically on start. Downgrading is not supported.
- **Tested.** CI runs the current release against the previous minor release for every
  version still supported, over the real protocols.

## Consequences

- Each wire protocol and API version keeps its spec and golden vectors until it is removed.
- The engine carries adapters for the previous protocol version during the deprecation window.
- Phase 6 may replace sync/1 and blobs/1 with their v2 versions instead of serving both.
- ADR 0005 is unchanged: artifacts share one version, and protocol versions stay separate in
  their ALPNs.
