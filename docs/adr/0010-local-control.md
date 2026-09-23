# 0010: Local control over a Unix socket; web peer inside tidefelld

- Status: accepted
- Date: 2026-09-23

## Context

The `tidefell` CLI controls the local daemon. The web peer needs the same engine. Running the
web peer as a separate process would need its own engine or an inter-process protocol.

## Decision

- `tidefelld` serves the REST API on a Unix domain socket, `<data_dir>/control.sock`, created
  with mode 0600. Access control is file ownership: whoever can open the socket may control the
  daemon. The CLI connects there by default.
- The web peer is part of `tidefelld`, enabled by a `[web]` section in the configuration. It
  listens on TCP, requires a logged-in account for every request, and talks to the engine
  in-process.
- Remote administration goes through the web peer, never the control socket.

## Consequences

- No network-exposed unauthenticated control surface.
- One process per device; the web peer shares the engine and block store of the daemon.
