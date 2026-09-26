# 0028: Other devices' files in phone folders

- Status: accepted
- Date: 2026-09-26

## Context

A phone folder (ADR 0025) holds only some of the folder's files. Backup only never downloads,
and two-way sync keeps files deleted on the phone off it. The app lists the rest as on other
devices, but nothing can bring one to the phone, although ADR 0025 says a dropped file is
downloaded again when the user opens or fetches it. The documents provider (ADR 0016) lists only
folders in app storage, so other apps cannot reach these files at all.

Android offers no way to put an entry into shared storage, or into `MediaStore`, that downloads
when it is opened: a stub file with the real name is opened by other apps as a broken file, and
one with its own extension is not routed to Tidefell reliably. The documents provider is the
platform's mechanism for files that are not on the device.

## Decision

- **Download to this phone.** In the app, a file of a phone folder that is not on the phone has a
  "Download to this phone" action. It fetches the current version from other devices and writes
  it into the folder, where it is announced to `MediaStore` like any file the engine writes.
  - With two-way sync the file is no longer dropped: later versions reach the phone again.
  - With backup only and remove after backup it is a one-time copy: later versions from other
    devices do not replace it, as for every file of a send-only folder.
  - Remove after backup does not remove a downloaded file again, as for a restored one.
- **In the file picker.** Each phone folder is also a root of the documents provider, labelled
  "On your other devices". It lists only the folder's files that are not on the phone, since the
  others are already in the Files app where the folder lives. It is read-only: nothing is created,
  edited or deleted through it. Opening a file there downloads it as above and then serves it, so
  it leaves the listing and is in the phone folder afterwards.

## Consequences

- Amends ADR 0025: backup only downloads a file when the user asks for it.
- No engine change: `open` already fetches a file of any folder and clears its dropped mark.
- Opening from the picker needs a connected device holding the file; it fails otherwise, as for
  a placeholder in app storage.
- The phone's gallery still shows only files on the phone. Previews of other devices' files in
  the picker need thumbnails from other devices, which are not synced.
