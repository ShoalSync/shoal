# 0002: `org.tidefell` namespace

- Status: superseded by [0043](0043-rename-to-shoal.md) (`org.shoalsync`)
- Date: 2026-09-23

## Context

Kotlin packages, the Android application ID and Maven coordinates need a reverse-domain
namespace. Changing it after release breaks installs and dependency coordinates.

## Decision

Use `org.tidefell`:

- Maven group: `org.tidefell`, e.g. `org.tidefell:tidefell-ffi`.
- Kotlin package for generated bindings: `org.tidefell.ffi`.
- Android application ID: `org.tidefell.android`.

## Consequences

Fixed from the first published artifact. Requires control of the `tidefell.org` domain for
Maven Central publication later.
