//! Human-readable output.

use std::fmt::Write as _;

use tidefell_sdk::types::{Event, File, Folder, KnownPeer, Peer, Recycled, Status};

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
        "device:  {}\nuser:    {}\nname:    {}\nversion: {}",
        s.device, s.user, s.name, s.version
    );
    if !folders.is_empty() {
        out.push_str("\nfolders:");
        for f in folders {
            let _ = write!(out, "\n  {}  {}\n    {}", f.id, f.path, recycle_bin(f));
        }
    }
    out
}

/// How long a folder keeps deleted files: "for 30 days" or "forever".
pub(crate) fn retention(f: &Folder) -> String {
    f.retention_days
        .map_or("forever".to_string(), |d| format!("for {d} days"))
}

/// A one-line summary of a folder's recycle bin on this device.
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
        if !f.on_disk {
            notes.push("pending".to_string());
        }
        if f.conflicts > 0 {
            notes.push(format!("{} conflict(s)", f.conflicts));
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
        let _ = writeln!(
            out,
            "{}  deleted {}  {:>10}  {}  ({expires})",
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
