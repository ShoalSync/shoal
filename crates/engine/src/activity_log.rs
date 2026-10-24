//! Each folder's activity log (ADR 0030): what this device applied, kept for 30 days or 10,000
//! entries, whichever is less. Not synced: every device keeps its own.

use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use shoal_meta::{Change, ChangeKind};
use shoal_proto::DeviceId;

/// Entries kept.
const ENTRIES: usize = 10_000;
/// How long entries are kept.
const DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;
/// Lines at which the log is rewritten, so it stays bounded while the folder is open.
const COMPACT_AT: usize = 2 * ENTRIES;

/// The log's file in a folder's state directory: one JSON line per entry, oldest first.
const FILE: &str = "activity";

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Line {
    pub(crate) time_ms: i64,
    pub(crate) kind: String,
    pub(crate) entry: String,
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) old_path: Option<String>,
    pub(crate) device: String,
}

pub(crate) fn kind_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Changed => "changed",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
        ChangeKind::Restored => "restored",
        ChangeKind::Conflict => "conflict",
    }
}

pub(crate) fn kind_of(name: &str) -> Option<ChangeKind> {
    Some(match name {
        "added" => ChangeKind::Added,
        "changed" => ChangeKind::Changed,
        "deleted" => ChangeKind::Deleted,
        "renamed" => ChangeKind::Renamed,
        "restored" => ChangeKind::Restored,
        "conflict" => ChangeKind::Conflict,
        _ => return None,
    })
}

impl Line {
    pub(crate) fn device(&self) -> Option<DeviceId> {
        self.device.parse().ok()
    }
}

/// Appends `changes` to the log in `dir`, applied at `now_ms`, and rewrites it once it passes
/// [`COMPACT_AT`] lines. `lines` counts the file's lines.
pub(crate) fn record(
    dir: &Path,
    lines: &Cell<usize>,
    changes: &[Change],
    now_ms: i64,
) -> std::io::Result<()> {
    lines.set(lines.get() + append(dir, changes, now_ms)?);
    if lines.get() > COMPACT_AT {
        lines.set(compact(dir, now_ms)?);
    }
    Ok(())
}

/// Appends `changes` to the log in `dir`, applied at `now_ms`, and returns how many lines it
/// wrote. The time is this device's clock when it applied them, not the files' modification
/// times, which can be long past.
fn append(dir: &Path, changes: &[Change], now_ms: i64) -> std::io::Result<usize> {
    if changes.is_empty() {
        return Ok(0);
    }
    let mut text = String::new();
    for c in changes {
        let line = Line {
            time_ms: now_ms,
            kind: kind_name(c.kind).to_owned(),
            entry: c.entry.to_string(),
            path: c.path.clone(),
            old_path: c.old_path.clone(),
            device: c.author.to_string(),
        };
        text.push_str(&serde_json::to_string(&line).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(FILE))?;
    file.write_all(text.as_bytes())?;
    Ok(changes.len())
}

/// The log's entries, oldest first, without those past their time or beyond the count.
pub(crate) fn read(dir: &Path, now_ms: i64) -> Vec<Line> {
    let Ok(text) = fs::read_to_string(dir.join(FILE)) else {
        return Vec::new();
    };
    let oldest = now_ms - DAYS * DAY_MS;
    let lines: Vec<Line> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Line>(l).ok())
        .filter(|l| l.time_ms >= oldest)
        .collect();
    let skip = lines.len().saturating_sub(ENTRIES);
    lines.into_iter().skip(skip).collect()
}

/// Rewrites the log without what `read` leaves out, so it does not grow for good, and returns how
/// many lines it kept. Done when the folder is opened and by [`record`].
pub(crate) fn compact(dir: &Path, now_ms: i64) -> std::io::Result<usize> {
    let path = dir.join(FILE);
    if !path.exists() {
        return Ok(0);
    }
    let lines = read(dir, now_ms);
    let kept = lines.len();
    let mut text = String::new();
    for line in lines {
        text.push_str(&serde_json::to_string(&line).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    let tmp = dir.join("activity.tmp");
    fs::write(&tmp, text)?;
    crate::rename_durably(tmp, path)?;
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use shoal_proto::EntryId;

    use super::*;

    fn change(n: u8) -> Change {
        Change {
            entry: EntryId([n; 16]),
            kind: ChangeKind::Changed,
            path: format!("file {n}"),
            old_path: None,
            author: DeviceId([n; 32]),
        }
    }

    #[test]
    fn kept_for_thirty_days_and_ten_thousand_entries() {
        let dir = tempfile::tempdir().unwrap();
        let now = 100 * DAY_MS;
        append(dir.path(), &[change(1)], now - 31 * DAY_MS).unwrap();
        append(dir.path(), &[change(2)], now - DAY_MS).unwrap();
        let lines = read(dir.path(), now);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].path, "file 2");
        assert_eq!(lines[0].device(), Some(DeviceId([2; 32])));

        let many: Vec<Change> = (0..ENTRIES + 5).map(|_| change(3)).collect();
        append(dir.path(), &many, now).unwrap();
        assert_eq!(read(dir.path(), now).len(), ENTRIES);
        assert_eq!(compact(dir.path(), now).unwrap(), ENTRIES);
        let text = fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert_eq!(text.lines().count(), ENTRIES);
    }

    #[test]
    fn rewritten_while_open_once_it_grows() {
        let dir = tempfile::tempdir().unwrap();
        let lines = Cell::new(0);
        let count = || {
            fs::read_to_string(dir.path().join(FILE))
                .unwrap()
                .lines()
                .count()
        };
        let batch: Vec<Change> = (0..ENTRIES).map(|_| change(4)).collect();
        record(dir.path(), &lines, &batch, 0).unwrap();
        record(dir.path(), &lines, &batch, 0).unwrap();
        assert_eq!(count(), COMPACT_AT);
        record(dir.path(), &lines, &batch[..1], 0).unwrap();
        assert_eq!(count(), ENTRIES);
        assert_eq!(lines.get(), ENTRIES);
    }
}
