Steps that once found a bug, run by `regressions_pass` in `../seeds.rs`. Each is the JSON list
of steps a failing seed was shortened to.

- `deleted-file-given-way.json`: a file deleted on one device before its scan, while another
  device's file took its path, was never recorded as deleted.
- `deleted-then-edited-elsewhere.json`: a deleted version brought back by an edit made apart
  from the delete was dropped from the device that kept it, not kept as a previous version.
- `older-version-deleted.json`: a delete applied to a file holding an older version than the
  one deleted lost that version.
- `restarted-device-serves.json`: an engine stopped while a task held its network node kept
  answering peers, so they never fetched from the engine started after it.
- `deleted-twice-around-an-edit.json`: as above, with the entry deleted again before its
  deleted version was let go.
- `metadata-lost-after-an-unsent-edit.json`: losing metadata right after an edit only that
  device had is held to what can be kept (see `LoseMetadata`).
- `renamed-over-a-twin.json`: a file moved over another of the same size and modification
  time, to the millisecond, was taken for the one it replaced, so the change was never synced;
  and the old path of a file the engine moved aside was tidied after another entry's file had
  taken it.
