//! Human-readable output.

use std::fmt::Write as _;

use tidefell_sdk::types::{
    Crash, Direction, Event, File, Folder, KnownPeer, Limits, LocalDeletes, LogSettings, Mode,
    Peer, Problem, Recycled, Status, SyncOptions, Version, VersionKind,
};

pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn time(ms: i64) -> String {
    // UTC, to the minute, without pulling in a date library.
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn status(s: &Status, folders: &[Folder]) -> String {
    let mut out = format!(
        "device:  {}\nuser:    {}\nname:    {}\nversion: {}\nlocal discovery: {}",
        s.device,
        s.user,
        s.name,
        s.version,
        if s.local_discovery { "on" } else { "off" }
    );
    if s.paused {
        out.push_str("\nsyncing paused on this device: `tidefell resume` to continue");
    }
    if let Some(c) = &s.crash {
        let _ = write!(
            out,
            "\nTidefell stopped unexpectedly at {}: {}\n  crash file: {}\n  \
             attach it to a report, then run `tidefell crashes --dismiss`",
            time(c.time_ms),
            c.message,
            c.file
        );
    }
    if !folders.is_empty() {
        out.push_str("\nfolders:");
        for f in folders {
            let _ = write!(out, "\n  {}  {}\n    {}", f.id, f.path, recycle_bin(f));
            if let Some(note) = only_here(f) {
                let _ = write!(out, "\n    {note}");
            }
            if let Some(note) = pinned(f) {
                let _ = write!(out, "\n    {note}");
            }
            if let Some(note) = conflict_note(f) {
                let _ = write!(out, "\n    {note}");
            }
            if let Some(reason) = &f.unavailable {
                let _ = write!(out, "\n    unavailable, not syncing: {reason}");
            }
            if f.paused {
                let _ = write!(
                    out,
                    "\n    syncing paused: `tidefell resume {}` to continue",
                    f.id
                );
            }
            if let Some(reason) = &f.watch_error {
                let _ = write!(
                    out,
                    "\n    not watched for changes ({reason}): scanned on its interval"
                );
            }
        }
    }
    out
}

/// A warning when files made on this device have no confirmed copy on a full replica.
fn only_here(f: &Folder) -> Option<String> {
    if f.only_here == 0 {
        return None;
    }
    let mut note = format!(
        "{} file(s) only on this device: no full replica has confirmed them yet",
        f.only_here
    );
    if f.mode == Mode::Partial {
        note.push_str(" (they stay on disk, even over the budget, until one does)");
    }
    Some(note)
}

/// How a folder syncs on this device, in words.
pub(crate) fn sync(f: &Folder) -> String {
    let direction = match f.sync.direction {
        Direction::Both => "sends and receives changes",
        Direction::SendOnly => "only sends changes",
    };
    let deletes = match f.sync.local_deletes {
        LocalDeletes::Everywhere => "deleting a file here deletes it everywhere",
        LocalDeletes::ThisDevice => "deleting a file here keeps it on other devices",
    };
    format!("{direction}; {deletes}")
}

/// How long a folder keeps deleted files: "for 30 days" or "forever".
pub(crate) fn retention(f: &Folder) -> String {
    f.retention_days
        .map_or("forever".to_string(), |d| format!("for {d} days"))
}

fn conflict_note(f: &Folder) -> Option<String> {
    (f.conflicts > 0).then(|| {
        format!(
            "{} file(s) with conflicts waiting for a decision: tidefell conflicts {}",
            f.conflicts, f.id
        )
    })
}

/// A one-line summary of a folder's recycle bin on this device.
fn pinned(f: &Folder) -> Option<String> {
    (f.pinned_bytes > 0).then(|| {
        format!(
            "{} pinned, kept here outside the budget",
            size(f.pinned_bytes)
        )
    })
}

fn recycle_bin(f: &Folder) -> String {
    let bin = &f.recycle_bin;
    if bin.items == 0 {
        return format!("recycle bin empty, keeps deleted files {}", retention(f));
    }
    let next = bin.next_expiry_ms.map_or("kept forever".to_string(), |t| {
        format!("next purge {}", time(t))
    });
    format!(
        "recycle bin {} file(s), {}, {next}; keeps deleted files {}",
        bin.items,
        size(bin.bytes),
        retention(f)
    )
}

/// What `recycle empty` removed.
pub(crate) fn emptied(items: &[Recycled]) -> String {
    match items {
        [] => "the recycle bin is empty".into(),
        [one] => format!("permanently deleted {}", one.path),
        many => format!(
            "permanently deleted {} file(s), {}",
            many.len(),
            size(many.iter().map(|r| r.size).sum())
        ),
    }
}

pub(crate) fn folders(folders: &[Folder]) -> String {
    if folders.is_empty() {
        return "no folders; create one with `tidefell folder create <path>`".into();
    }
    let mut out = String::new();
    for f in folders {
        let source = if f.configured { ", from config" } else { "" };
        let _ = writeln!(out, "{}  {}", f.id, f.path);
        let _ = writeln!(out, "    {} member(s){source}", f.members.len());
        if f.sync != SyncOptions::default() {
            let _ = writeln!(out, "    {}", sync(f));
        }
        let _ = writeln!(out, "    {}", recycle_bin(f));
    }
    out.trim_end().to_string()
}

pub(crate) fn created(f: &Folder) -> String {
    format!(
        "created folder {}\nat {}\n\nOn your other devices, run:\n  tidefell folder join {} <path>",
        f.id, f.path, f.id
    )
}

pub(crate) fn joined(f: &Folder) -> String {
    format!("joined folder {}\nat {}", f.id, f.path)
}

pub(crate) fn files(files: &[File]) -> String {
    if files.is_empty() {
        return "no files".into();
    }
    let mut out = String::new();
    for f in files {
        let mut notes = Vec::new();
        if f.pinned {
            notes.push("pinned".to_string());
        }
        if !f.on_disk {
            notes.push("pending".to_string());
        }
        if f.conflicts > 0 {
            notes.push(format!("{} conflict(s)", f.conflicts));
        }
        if f.only_here {
            notes.push("only on this device".to_string());
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("  [{}]", notes.join(", "))
        };
        let _ = writeln!(
            out,
            "{:>10}  {}  {}{notes}",
            size(f.size),
            time(f.modified_ms),
            f.path
        );
    }
    out.trim_end().to_string()
}

pub(crate) fn recycled(items: &[Recycled]) -> String {
    if items.is_empty() {
        return "the recycle bin is empty".into();
    }
    let mut out = String::new();
    for r in items {
        let expires = r.expires_at_ms.map_or("kept forever".to_string(), |t| {
            format!("purged {}", time(t))
        });
        let what = if r.here_only {
            "removed here"
        } else {
            "deleted"
        };
        let _ = writeln!(
            out,
            "{}  {what} {}  {:>10}  {}  ({expires})",
            r.entry,
            time(r.deleted_at_ms),
            size(r.size),
            r.path
        );
    }
    out.trim_end().to_string()
}

pub(crate) fn peers(connected: &[Peer], known: &[KnownPeer]) -> String {
    let mut out = String::from("connected:\n");
    if connected.is_empty() {
        out.push_str("  none\n");
    }
    for p in connected {
        let _ = writeln!(
            out,
            "  {}  {} (user {}), {} shared folder(s)",
            p.device,
            p.name,
            p.user,
            p.shared_folders.len()
        );
    }
    out.push_str("kept connected:\n");
    if known.is_empty() {
        out.push_str("  none\n");
    }
    for p in known {
        let _ = writeln!(out, "  {}  {}", p.device, p.addrs.join(", "));
    }
    out.trim_end().to_string()
}

pub(crate) fn event(e: &Event) -> String {
    match e {
        Event::FolderAdded { folder } => format!("folder added      {folder}"),
        Event::FolderRemoved { folder } => format!("folder removed    {folder}"),
        Event::FolderChanged { folder } => format!("folder changed    {folder}"),
        Event::FileUpdated { folder, path } => format!("file updated      {folder}  {path}"),
        Event::FileRemoved { folder, path } => format!("file removed      {folder}  {path}"),
        Event::ConflictDetected { folder, path } => format!("conflict          {folder}  {path}"),
        Event::Crashed { message } => format!("engine stopped    {message}"),
    }
}

pub(crate) fn versions(versions: &[Version]) -> String {
    versions
        .iter()
        .map(|v| match v.kind {
            VersionKind::Current => format!(
                "current                    {}  {:>10}",
                time(v.modified_ms),
                size(v.size)
            ),
            VersionKind::Conflict => format!(
                "{}  {}  {:>10}  conflicts, from {}",
                v.id,
                time(v.modified_ms),
                size(v.size),
                v.author_name
                    .as_deref()
                    .or(v.author.as_deref())
                    .unwrap_or("another device")
            ),
            VersionKind::Previous => {
                let expires = v.expires_ms.map_or("kept for good".to_owned(), |ms| {
                    format!("until {}", time(ms))
                });
                format!(
                    "{}  {}  {:>10}  replaced {}, {expires}",
                    v.id,
                    time(v.modified_ms),
                    size(v.size),
                    time(v.kept_ms.unwrap_or_default())
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A file with versions that conflict, for `tidefell conflicts`.
#[derive(serde::Serialize)]
pub(crate) struct Conflict {
    pub(crate) folder: String,
    pub(crate) path: String,
    pub(crate) versions: Vec<Version>,
}

pub(crate) fn conflicts(conflicts: &[Conflict]) -> String {
    if conflicts.is_empty() {
        return "no conflicts".into();
    }
    conflicts
        .iter()
        .map(|c| {
            let waiting: Vec<Version> = c
                .versions
                .iter()
                .filter(|v| v.kind != VersionKind::Previous)
                .cloned()
                .collect();
            format!(
                "{} {}\n{}\n  decide with: tidefell resolve {} {:?} --keep <version> (or --keep-both)",
                c.folder,
                c.path,
                versions(&waiting)
                    .lines()
                    .map(|l| format!("  {l}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                c.folder,
                c.path
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn crashes(crashes: &[Crash]) -> String {
    if crashes.is_empty() {
        return "no crashes recorded".into();
    }
    crashes
        .iter()
        .map(|c| {
            let seen = if c.seen { "" } else { "  (new)" };
            format!("{}  {}{seen}\n  {}", time(c.time_ms), c.message, c.file)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn limits(l: &Limits) -> String {
    let rate = |r: Option<u64>| r.map_or("no limit".to_owned(), |k| format!("{k} KiB/s"));
    format!(
        "upload:   {}\ndownload: {}\nlocal network: {}",
        rate(l.upload_kib_per_sec),
        rate(l.download_kib_per_sec),
        if l.limit_local {
            "limited"
        } else {
            "not limited"
        }
    )
}

pub(crate) fn log_settings(l: &LogSettings) -> String {
    match &l.file {
        Some(file) => format!("level: {}\nfile:  {file}", l.level),
        None => format!(
            "level: {}\nfile:  none (the system journal keeps the logs)",
            l.level
        ),
    }
}

pub(crate) fn problems(problems: &[Problem]) -> String {
    if problems.is_empty() {
        return "no warnings or errors".into();
    }
    let mut out = String::new();
    for p in problems {
        let _ = write!(out, "{} {:5} {}", time(p.time_ms), p.level, p.message);
        for (k, v) in &p.fields {
            let _ = write!(out, " {k}={v}");
        }
        out.push('\n');
    }
    out.pop();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_here_notes() {
        let mut folder = Folder {
            id: "f".into(),
            path: "/p".into(),
            mode: Mode::Full,
            budget_bytes: None,
            members: Vec::new(),
            retention_days: Some(30),
            sync: Default::default(),
            recycle_bin: Default::default(),
            configured: false,
            only_here: 0,
            watch_error: None,
            unavailable: None,
            paused: false,
            pinned_bytes: 0,
            conflicts: 0,
        };
        assert_eq!(only_here(&folder), None);
        folder.only_here = 2;
        assert_eq!(
            only_here(&folder).unwrap(),
            "2 file(s) only on this device: no full replica has confirmed them yet"
        );
        folder.mode = Mode::Partial;
        assert!(only_here(&folder).unwrap().ends_with("until one does)"));
    }

    #[test]
    fn sizes() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn times() {
        assert_eq!(time(0), "1970-01-01 00:00");
        assert_eq!(time(1_758_600_000_000), "2025-09-23 04:00");
        assert_eq!(time(951_782_400_000), "2000-02-29 00:00");
    }
}
