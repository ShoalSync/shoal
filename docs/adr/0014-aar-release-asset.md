# 0014: The ffi AAR is a release asset, not a Maven package

- Status: accepted
- Date: 2026-09-23

## Context

The Android app consumes `tidefell-ffi` as an AAR (ADR 0004). The plan named GitHub Packages
Maven as the channel. While the repositories are private, every consumer of GitHub Packages
Maven, including CI and developer machines, needs a personal access token with
`read:packages`; the organisation's GitHub App token cannot read that registry. Release assets
are already fetched with the App token by `tidefell/site` and `tidefell/infra`.

## Decision

- Each monorepo release attaches `tidefell-ffi-X.Y.Z.aar` and its `.sha256`. The AAR holds the
  uniffi-generated Kotlin (package `org.tidefell.ffi`) and `libtidefell_ffi.so` for
  `arm64-v8a`, `armeabi-v7a` and `x86_64`.
- The AAR is built from `bindings/android/` in the monorepo, a Gradle Android library project,
  using `cargo-ndk`.
- `tidefell/android` pins the version in `gradle/libs.versions.toml` (`tidefell = "X.Y.Z"`). Its
  build downloads the AAR with `gh release download`, checks the sha256, and depends on the file.
  The AAR has no POM, so the app declares the AAR's own dependencies (JNA) itself; the list is in
  the AAR's README.
- The `tidefellFfiLocalPath` Gradle property replaces the download with a composite build of
  `bindings/android/` from a local monorepo checkout.

## Consequences

- No new credentials: the App token that already reads releases is enough.
- Publishing to a Maven repository (GitHub Packages or Maven Central) is a later, additive step
  once the repositories are public; the Gradle coordinates `org.tidefell:tidefell-ffi` stay
  reserved for it.
