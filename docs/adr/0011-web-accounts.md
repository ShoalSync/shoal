# 0011: Web accounts are local logins mapped to Tidefell users

- Status: accepted
- Date: 2026-09-23

## Context

The web peer lets people use Tidefell from a browser. It needs authentication and must decide
which folders each person can see. Tidefell membership is granted to users (ADR 0001 glossary),
identified by user keys that normally live on people's own devices.

## Decision

- The web peer is an ordinary certified device.
- An account is a username and password (argon2id hash) stored in the web peer's data
  directory, linked to one Tidefell `UserId`.
- An account sees and acts on exactly the folders that its user is a member of and that the web
  peer replicates. Actions (edits, deletes, restores) are made by the web peer's device.
- The server never stores user keys. Certifying new devices stays on devices that hold a user
  key.
- Sessions are random tokens in `HttpOnly`, `Secure`, `SameSite=Strict` cookies, stored
  server-side with an expiry. State-changing requests require a per-session CSRF token.
- The first account is created with the CLI on the web peer's host.

## Consequences

- Compromising the web peer exposes the folders it replicates, not users' keys or other devices.
- Actions made through the web appear as the web peer's device in version history.
