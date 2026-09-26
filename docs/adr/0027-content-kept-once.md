# 0027: Content is kept once per device

- Status: accepted
- Date: 2026-09-26

## Context

Every scan copied the chunks of every file into the block store (ADR 0008), and full replicas
kept them there. Each file therefore took twice its size on disk: a NAS with a terabyte of photos
used two, and a phone folder such as the camera folder (ADR 0025) doubled the space the photos
already took. The block store copy was what peers were served from, and it kept a deleted file's
content for the recycle bin.

## Decision

- **Files on disk are the content.** Scans compute manifests without copying chunks. A chunk is
  read from the block store if it is there, otherwise from a file on disk whose recorded content
  contains it, at the offset the manifest gives. Every chunk read from a file is checked against
  its hash, so a file changed since the last scan is never served; the peer asks elsewhere.
  Availability answers (blobs/1) count chunks on disk, so full replicas confirm copies (ADR 0022)
  of files they hold only as files.
- **The block store holds what is on disk nowhere.** Chunks being downloaded, and the content of
  recycle-bin items, conflict copies and pending versions that have no file. After each
  materialise pass, the engine removes every other chunk from the store. The first pass after an
  upgrade removes the copies earlier versions kept.
- **Deleted files are moved, not copied.** When the engine removes a file whose delete the
  recycle bin records (a delete from another device, `delete`, or a file replaced by a rename), it
  moves the file into the folder's reserved directory (`.tidefell/deleted/`) and reads the recycle
  bin's content from there. A file about to be overwritten that another entry still needs is moved
  aside the same way. Kept files are removed when their content is back in the folder or nothing
  references it, and when the recycle bin purges it.
- **Files deleted by hand.** When a file is deleted outside the engine on a device, that device has
  no copy left, since there is no second copy to keep. Its recycle bin still lists the file, and
  restoring it fetches the content from the other devices, which keep it in their recycle bins
  because they removed their copies through the engine.

## Consequences

- Each device stores roughly the size of its files, plus what its recycle bin keeps.
- A folder that exists on only one device loses the content of files deleted there by hand. Keep
  a second replica, such as an always-on peer (ADR 0018), for folders whose deletes must be
  recoverable.
- Serving a chunk from a file costs a seek and a hash check, and needs the folder's records to be
  current; a file edited in place is served again after the next scan.
- Engine state format 6 records the kept files. Older formats load with none.
