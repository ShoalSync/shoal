# notify 8.2.0, patched

A copy of [`notify`](https://crates.io/crates/notify) 8.2.0 as published, with one change, used
through `[patch.crates-io]` until notify 9 is released (ADR 0042's file-semantics amendment).

## Windows: an overflow is a rescan, and the watch is kept

`ReadDirectoryChangesW` reports that it discarded changes, as when a burst overflows its
buffer, either as a successful completion of zero bytes or as `ERROR_NOTIFY_ENUM_DIR`. notify
8.2.0 read the zero-byte completion's empty buffer and reported nothing, and treated
`ERROR_NOTIFY_ENUM_DIR` as an unknown error, dropping the watch without an event: the folder's
changes then waited for its next full scan.

In `src/windows.rs`, `handle_event` now keeps watching on `ERROR_NOTIFY_ENUM_DIR`, and on
either form emits an event flagged `Flag::Rescan` instead of reading the buffer, which the
engine already answers with a scan. This is what notify 9 does (`completion_rescan_event` in
9.0.0-rc.6), so the copy goes when the engine moves to notify 9.
