# 0020: Camera-roll backup on Android

- Status: accepted
- Date: 2026-09-24

## Context

Backing up photos from a phone is the other everyday reason to use a sync service. ADR 0016
kept it separate from partial replicas: the phone does not scan shared storage, and
backing up means copying new photos out rather than keeping the camera folder in sync.

## Decision

- **Opt-in, per folder.** In the Android app the user picks a joined folder as the backup
  target, and chooses all photos and videos or selected albums.
- **Upload only.** New items from `MediaStore` are written with the engine's `write` under
  `Camera/<year>/<month>/<original name>`, with a suffix if the name is taken. Deleting or
  editing a photo on the phone never changes the folder, and deleting it from the folder
  never touches the phone.
- **Tracking.** The app records the `MediaStore` ID and content hash of each item it has
  backed up, so reinstalls and renames do not upload duplicates. The first run backs up
  everything that matches, oldest first.
- **Scheduling.** A `WorkManager` job runs when new media appears and periodically, by
  default only on unmetered networks and while charging. The foreground service shows
  progress while it runs.
- **Photos stay on the phone.** Backing up never removes a photo or video from the phone's
  camera roll by default. The folder's copy on the phone is a second copy, so a partial
  replica may evict it to a placeholder like any other file (ADR 0016). The original in
  `MediaStore` is untouched.
- **Removing originals is a setting, off by default.** "Remove from phone after backup"
  deletes an original only after a peer confirms it holds the content, and only through
  the system's delete confirmation. Turning it on asks for confirmation, and it never
  applies to items backed up before it was turned on.

## Consequences

- Needs `READ_MEDIA_IMAGES` and `READ_MEDIA_VIDEO` (or `READ_EXTERNAL_STORAGE` on older
  versions), requested only when backup is turned on.
- An always-on peer (ADR 0018) matters here too: without one, photos only leave the phone
  while another peer is online.
