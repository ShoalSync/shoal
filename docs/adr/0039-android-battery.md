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

## Implementation notes

- `scripts/battery-bench.sh` in `tidefell/android` pairs the app with a `tidefelld` on the host
  and measures three scenarios from a reset of the phone's battery statistics: idle with the
  foreground service running, receiving 1 GiB into a two-way phone folder, and backing up 1 GiB
  of photos. It reports the app's CPU time, wake-up alarms, jobs, partial wakelocks and network
  bytes, idle per hour and transfers per GiB, and fails a run over its budget.
- Energy is not reported from an emulator, whose power profile is a placeholder. The first
  figures are the emulator's; a phone's are still to come.
- Once opened, the app keeps its service running until Android stops it, which the shell cannot
  do without also cancelling the app's scheduled work, so the cost of that work alone is not
  measured.
- First figures, with the code released as `v0.18.0` on the emulator (`sdk_gphone64_x86_64`,
  Android 14), and the budgets
  set from them:

  | Scenario | CPU | Budget | Network |
  |---|---|---|---|
  | Idle, service running | 7.4 s per hour | 15 s | 1.9 MB per hour |
  | Receiving 1 GiB | 60 s | 120 s | 1.07 bytes per byte |
  | Backing up 1 GiB | 87 s | 180 s | 1.08 bytes per byte |

  No wake-up alarms, jobs or wakelocks while idle. Network is budgeted at 1.25 bytes per byte of
  content.
- No change to save battery is made yet: idle costs about 0.2% of one core, and nothing the
  emulator can measure points at one. The likely changes above wait for figures from a phone,
  which shows radio time and wake-ups as an emulator cannot.
- Backing up 1 GiB took 23 minutes against 1.5 minutes to receive it. Looked into, the time is
  not the phone's: it finds new photos within a second of them appearing, hashes 200 MiB in about
  a second and pushes its index at once, and the photos then move at about 10 MB/s. The phone's
  messages to the host wait in the QUIC connection, from 80 seconds to many minutes from one run
  to the next. On Linux, the same two daemons with the phone behind NAT back up 200 MiB in under
  5 seconds, directly or through the relay alone, so it is the emulator's network with iroh and
  not the topology. Whether a phone is affected needs a run on one; until then the emulator's
  backup figure is not one to hold the app to, and its budget only catches large regressions.
- The app counts its own runs, running time, scanning time and bytes moved, on the phone, and the
  diagnostics bundle includes them, per hour since counting started (`usage` in the app's part of
  the summary).
