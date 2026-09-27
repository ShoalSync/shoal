# 0039: Android battery

- Status: accepted
- Date: 2026-09-27

## Context

Nobody has measured how much battery Tidefell uses. The app keeps a foreground service
(`dataSync`) running while the peer is started, holds a connection to the relay while it runs,
scans phone folders after new media and on a schedule (ADR 0025), and hashes every new photo and
video. Each of these can wake the radio or the CPU. Android limits background work more with
each release, and users uninstall apps that show up in the battery screen.

## Decision

- **Measure first, locally.** A scripted benchmark on an emulator and one real device measures,
  from `dumpsys batterystats` and the app's own counters:
  - energy attributed to the app per hour idle and per GB synced;
  - wake-ups and radio-active time per hour while idle;
  - CPU time for hashing per GB of photos and video.
  It runs for each release, and its results go in the release notes. Budgets are set from the
  first results.
- **The app counts** its own wake-ups, connections, bytes and hashing time, keeps them on the
  device, and includes them in the diagnostics bundle (ADR 0030), so a user reporting battery
  drain can attach numbers.
- **Likely changes,** each confirmed by the benchmark before it ships:
  - drop the relay connection, and the foreground service, when there is nothing to sync, and
    reconnect on a schedule or a local change;
  - batch scans and uploads instead of acting on each new photo;
  - hash large videos only while charging, unless the user asks otherwise;
  - respect battery saver by deferring everything but user-opened files.
- **Usage telemetry later.** Energy and wake-up figures from real devices are the first
  candidates for opt-in usage telemetry. ADR 0030 rules that telemetry needs its own ADR, off by
  default, asked once, with a preview of what is sent and a collector users can run themselves;
  that ADR will start from these figures.

## Consequences

- Battery use becomes a measured number per release instead of a guess.
- Users can attach real figures to a battery complaint today, without any telemetry.
- Some sync becomes less immediate on battery, by design, and the settings say so.
