# 0025: Phone folders on Android

- Status: accepted
- Date: 2026-09-26

## Context

On Android, a joined folder lives in the app's private storage and is reached through the
documents provider (ADR 0016). Photos leave the phone through a separate, upload-only camera
backup (ADR 0020), which can offer to delete originals once a full replica holds them
(ADR 0022). The camera roll is therefore a special case with its own settings, and no other
folder on the phone can be synced.

Users want to pick any folder on the phone, the camera folder being just one of them, and to
choose for each folder how it syncs. Nothing on the phone is deleted unless the user chose it.

## Decision

### Where a folder lives on the phone

- **Any folder.** When creating or joining a folder, the user chooses where it lives on this
  phone: in app storage, as today (files on demand through the documents provider), or in a
  folder in shared storage such as `Pictures`, `Documents`, `Download`, `DCIM/Camera` or a
  folder on an SD card.
- **Choosing a folder.** The app lists folders in shared storage. The camera folder comes
  first, labelled "Camera", when the phone has one with photos in it: it is the folder most
  people will pick, and it is where the phone's own photos are. Other folders follow by name.
  Apart from its position in that list, the camera folder is treated like any other.
- **Access.** The app asks for "All files access" (`MANAGE_EXTERNAL_STORAGE`) the first time a
  folder in shared storage is chosen. The engine then reads and writes those folders with the
  same path-based code as on a desktop. App storage needs no permission. Publishing on Google
  Play needs the file-sync declaration for this permission.

### Sync options

Each phone folder has its own sync option, chosen when it is added and changeable later. The
app lists them in this order:

1. **Backup only.** Files added or changed on the phone are sent to other devices. Nothing is
   downloaded, and nothing on the phone is changed or deleted. Deleting a file on the phone
   does not delete it anywhere else.
2. **Two-way sync.** Files added or changed on the phone are sent to other devices, and files
   added or changed elsewhere are written into the folder on the phone. A file deleted on
   another device moves to this phone's recycle bin, as on every replica. A file deleted on
   the phone, by any app, is dropped from this phone only: it stays on every other device, and
   the app lists it as on other devices. It is not downloaded again, even when a new version
   arrives, unless the user opens or fetches it.
3. **Remove after backup.** As backup only, and in addition a file is removed from the phone
   once a full replica confirms it holds that version (ADR 0022). Choosing this option asks for
   confirmation once; removals do not ask again. It applies only to files added or changed
   after it was chosen. Removed files stay on every other device.

Backup only is the default. Folders in app storage always use two-way sync with files on
demand, as today.

- **Deleting everywhere is explicit.** Whatever the option, "Delete from all devices" is an
  action in the app. It sends the file to every replica's recycle bin, as a delete does today.
- **Clean up.** Folders on backup only or two-way sync have a "Clean up" action. It shows how
  many files, and how much space, a full replica has confirmed holding. If the user confirms,
  those files are removed from the phone.
- **Removal goes through the recycle bin.** Whenever the app removes a file from the phone,
  by remove after backup or by clean up, the file moves to this phone's recycle bin for the
  folder (ADR 0021) and is deleted for good only when the folder's retention expires or the
  user empties the bin. Restoring it puts it back in the phone folder. Space is freed when the
  bin is purged, and the app shows how much is waiting there.

### Engine

- A folder gains two per-device settings, which are not synced:
  - **direction**: `both` (today's behaviour) or `send-only`, which never downloads content
    and never applies other devices' changes to local files;
  - **local deletes**: `everywhere` (today's behaviour) or `this-device`, where a scan that
    finds a file missing records it as dropped on this device, like a placeholder of a partial
    replica (ADR 0016), instead of recording a delete.
- Backup only is `send-only` with `this-device`. Two-way sync is `both` with `this-device`.
  Remove after backup is backup only plus the app's removal of confirmed files.
- Desktops and servers keep `both` and `everywhere` by default. The settings are in the
  configuration, the API and the bindings.

### The camera roll

- **Like any folder.** The user picks the camera folder, a Tidefell folder for it, and a sync
  option. The upload-only backup into `Camera/<year>/<month>` (ADR 0020) is withdrawn. Photos
  keep the camera's own names and layout.
- **Galleries.** With two-way sync, photos from other devices are written into the camera
  folder and announced to `MediaStore`, so the phone's gallery shows them. A user who does not
  want that uses a Tidefell folder that only this phone's camera writes to, or backup only.

## Consequences

- Amends ADR 0016: the phone scans the shared-storage folders the user chooses. Supersedes
  ADR 0020, and on Android the notification-driven removal of originals in ADR 0022.
- A monorepo release with the two engine settings comes before the app work.
- Scanning large shared folders costs battery. Scans follow the folder's scan interval and run
  in the sync service.
- Remove after backup removes files without a per-item system prompt, because All files access
  allows it. The confirmation when choosing the option, the confirmed copy on a full replica
  and the recycle bin are the safeguards.
- Nothing has been released publicly, so the old camera backup and its settings are removed
  without a migration.
