# 0033: File names across filesystems

- Status: accepted
- Date: 2026-09-27

## Context

sync/1 requires paths to be NFC-normalised, but nothing enforces it: `is_valid_path` notes that
"NFC normalisation is the author's responsibility and is not checked here", and the scanner
records names as it reads them. Nothing accounts for filesystems that treat different paths as
the same file, or that cannot store a name at all:

- **Case-insensitive filesystems.** Android's shared storage, where phone folders live
  (ADR 0025), matches names without regard to case, and so do the exFAT and FAT32 SD cards it
  supports. So do macOS and Windows by default, the Phase 7 and 8 clients.
- **Normalisation.** macOS and iOS may hand back names in a different Unicode form than they
  were created with, so one name can arrive as two different paths.
- **Names a filesystem refuses.** Windows and FAT refuse `< > : " | ? *`, control characters,
  trailing dots and spaces, and names such as `CON` and `NUL`. Most filesystems cap a name at
  255 bytes or 255 UTF-16 units.

A Linux device can sync `Photo.jpg` and `photo.jpg` as two files. On a phone folder they are
one file, so the phone writes both to the same place: one overwrites the other, or the two
change back and forth on every scan. A name the phone cannot store fails to write on every pass.

## Decision

- **Paths on the wire.** Paths stay case-preserving and NFC-normalised. The scanner converts
  every name it reads to NFC before recording it, and remembers the name as it is on disk.
  sync/1 validation rejects paths that are not NFC. Nothing has been released publicly, so this
  needs no migration (ADR 0026).
- **Each folder knows its filesystem.** When a folder opens, the engine probes its directory
  with test files in `.tidefell/`: whether names match without regard to case, whether they
  match across Unicode forms, which characters and names are refused, and the longest name
  allowed. It records the result and re-probes if the folder moves. `tidefell doctor`
  (ADR 0030) reports it.
- **Collisions follow ADR 0024.** On each device, entries whose paths are equal after that
  device's folding (case, Unicode form) count as the same path. The entry with the lower
  `EntryId` keeps the name, and the others are written as `<stem> (conflict <id>)<extension>`,
  exactly as for entries whose paths are identical. This only changes where the device writes
  them: their recorded paths and versions are unchanged, so the other devices still see
  `Photo.jpg` and `photo.jpg`. Directories whose paths differ only in case are one directory on
  such a device, spelled as the lowest `EntryId` beneath them spells it.
- **Names a device cannot store** are not written. The file is listed as on other devices, like
  a dropped file (ADR 0025), with a problem (ADR 0030) that names the reason. The app, the web
  and the CLI offer to rename it everywhere. Names are not escaped or replaced, because an
  escaped name that came back as a local change would rename the file on every device.
- **Local changes** keep the case and characters the user typed. A rename that only changes
  case, which case-insensitive filesystems allow, is recorded as a rename.

## Consequences

- A folder shared between Linux and a phone, a Mac or a PC never overwrites one file with
  another because their names fold together; the device that cannot tell them apart keeps both
  under distinct names.
- A user with names Windows or FAT cannot store sees them listed and can fix them from any
  device.
- The probe costs a few file operations when a folder opens.
- The `unicode-normalization` crate, or an equivalent, becomes a dependency of `proto` and must
  pass `cargo-deny`.
