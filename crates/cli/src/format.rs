//! Human-readable output.

use std::fmt::Write as _;

use tidefell_sdk::types::{
    Activity, ChangeKind, Check, CheckLevel, Crash, CrashReport, Direction, Event, File, Folder,
    FolderState, KnownPeer, Limits, LocalDeletes, LogSettings, Mode, Operation, PathKind,
    PathProblem, Peer, PeerState, Problem, ProblemKind, Rebuild, Recycled, Status, SyncOptions,
    Version, VersionKind,
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

/// Now, in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
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

/// Days since 1970-01-01 of a civil date, the inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Milliseconds since the Unix epoch of a time written `YYYY-MM-DD HH:MM[:SS]`, with `T` for the
/// space if wanted, in UTC unless it ends in `Z` or an offset such as `+02:00`.
pub(crate) fn parse_time(text: &str) -> Result<i64, String> {
    let bad = || {
        format!("{text:?} is not a time such as \"2026-10-16 14:30\" (UTC) or \"2026-10-16T14:30+02:00\"")
    };
    let text = text.trim();
    let (date, rest) = text.split_once([' ', 'T']).ok_or_else(bad)?;
    let mut date = date.split('-').map(|p| p.parse::<i64>());
    let (Some(Ok(y)), Some(Ok(m)), Some(Ok(d)), None) =
        (date.next(), date.next(), date.next(), date.next())
    else {
        return Err(bad());
    };
    let (clock, offset_s) = if let Some(clock) = rest.strip_suffix('Z') {
        (clock, 0)
    } else if let Some(at) = rest.rfind(['+', '-']) {
        let (clock, offset) = rest.split_at(at);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let digits: String = offset[1..].chars().filter(char::is_ascii_digit).collect();
        if digits.len() != 4 {
            return Err(bad());
        }
        let hours: i64 = digits[..2].parse().map_err(|_| bad())?;
        let minutes: i64 = digits[2..].parse().map_err(|_| bad())?;
        (clock, sign * (hours * 3600 + minutes * 60))
    } else {
        (rest, 0)
    };
    let mut clock = clock.split(':').map(|p| p.parse::<i64>());
    let (Some(Ok(h)), Some(Ok(min))) = (clock.next(), clock.next()) else {
        return Err(bad());
    };
    let s = match clock.next() {
        None => 0,
        Some(Ok(s)) => s,
        Some(Err(_)) => return Err(bad()),
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || s > 59 {
        return Err(bad());
    }
    let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60 + s - offset_s;
    Ok(secs * 1000)
}

pub(crate) fn loss_preview(
    device: &str,
    lost_ms: i64,
    previews: &[tidefell_sdk::types::LossPreview],
) -> String {
    let mut out = format!(
        "Revoking {device} as lost at {} UTC would, on this device:",
        time(lost_ms)
    );
    let mut any = false;
    for p in previews {
        if p.moved {
            out.push_str(&format!(
                "\n  folder {}: the time moves to {} UTC, as far back as its recycle bin keeps",
                p.folder,
                time(p.lost_ms)
            ));
        }
        for f in &p.withdrawn {
            any = true;
            let at = f.received_ms.map_or("time not recorded".into(), |ms| {
                format!("received {}", time(ms))
            });
            let what = if f.put_back {
                "put back what it replaced in"
            } else {
                "move to the recycle bin"
            };
            out.push_str(&format!(
                "\n  {what} {} ({} bytes, {at}), in folder {}",
                f.path, f.size, p.folder
            ));
        }
        if p.unknown > 0 {
            out.push_str(&format!(
                "\n  folder {}: leave {} version(s) it cannot place before or after the time to the other devices",
                p.folder, p.unknown
            ));
        }
    }
    if !any {
        out.push_str("\n  withdraw nothing: it received nothing from the device since then");
    }
    out.push_str(
        "\nEach of your devices judges by when it received each change, so others may differ.",
    );
    out
}

pub(crate) fn held_withdrawals(held: &[tidefell_sdk::types::HeldWithdrawals]) -> String {
    if held.is_empty() {
        return "no withdrawals wait for a decision".into();
    }
    held.iter()
        .map(|h| {
            format!(
                "revoked device {} changed {} file(s) after it was lost: {}",
                h.device,
                h.paths.len(),
                h.paths.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    if !s.metadata_dir.is_empty() {
        out.push_str(&format!("\nmetadata: {}", s.metadata_dir));
    }
    if s.revoked {
        out.push_str(
            "\nREVOKED: your user revoked this device; it syncs with no one. Pair it again.",
        );
    }
    if s.paused {
        out.push_str("\nsyncing paused on this device: `tidefell resume` to continue");
    }
    if let Some(c) = &s.crash {
        let _ = write!(
            out,
            "\nTidefell stopped unexpectedly at {}: {}\n  crash file: {}\n  \
             `tidefell crashes --report` writes it up for the project's tracker; \
             `tidefell crashes --dismiss` then stops this notice",
            time(c.time_ms),
            c.message,
            c.file
        );
    }
    let t = &s.transfer;
    let _ = write!(
        out,
        "\ntransfer: {} sent, {} received since start; now {}/s up, {}/s down",
        size(t.sent),
        size(t.received),
        size(t.send_rate),
        size(t.receive_rate)
    );
    if !s.peers.is_empty() {
        out.push_str("\npeers:");
        for p in &s.peers {
            let _ = write!(out, "\n  {}  {}", p.device, peer_line(p));
        }
    }
    if !folders.is_empty() {
        out.push_str("\nfolders:");
        for f in folders {
            let _ = write!(
                out,
                "\n  {}  {}\n    {}\n    {}",
                f.id,
                f.path,
                folder_state(f),
                recycle_bin(f)
            );
            if f.problems > 0 {
                let _ = write!(
                    out,
                    "\n    {} path(s) with problems: `tidefell problems {}`",
                    f.problems, f.id
                );
            }
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

/// A peer's name and connection: "laptop, connected direct (12 ms) since …" or "not
/// connected, last seen …: error".
fn peer_line(p: &PeerState) -> String {
    let name = p.name.clone().unwrap_or_else(|| "(not seen yet)".into());
    if p.connected {
        let path = match p.path {
            Some(PathKind::Direct) => "direct",
            Some(PathKind::Relay) => "through a relay",
            None => "no path now",
        };
        let rtt = p.rtt_ms.map(|ms| format!(" ({ms} ms)")).unwrap_or_default();
        let since = p
            .connected_since_ms
            .map(|ms| format!(" since {}", time(ms)))
            .unwrap_or_default();
        return format!("{name}, connected {path}{rtt}{since}");
    }
    let seen = p
        .last_seen_ms
        .map_or("not seen since start".to_string(), |ms| {
            format!("last seen {}", time(ms))
        });
    let error = p
        .last_error
        .as_ref()
        .map(|e| format!(": {e}"))
        .unwrap_or_default();
    format!("{name}, not connected, {seen}{error}")
}

/// What a folder is doing, what it waits for, and its last scan.
fn folder_state(f: &Folder) -> String {
    let state = match f.state {
        FolderState::Idle => "idle",
        FolderState::Scanning => "scanning",
        FolderState::Syncing => "syncing",
        FolderState::Paused => "paused",
        FolderState::Rebuilding => "rebuilding",
        FolderState::Error => "error",
    };
    let mut line = state.to_string();
    if let Some(error) = &f.error {
        let _ = write!(line, " ({error})");
    }
    if let Some(r) = &f.rebuild {
        match r.finished_ms {
            None => {
                let _ = write!(line, " ({}; {})", r.reason, waiting_for(r));
            }
            // A rebuild is news for a week (ADR 0034); `tidefell folder list` always says.
            Some(at) if now_ms() - at < 7 * 24 * 3600 * 1000 => {
                let _ = write!(line, "; {}", rebuild(r));
            }
            Some(_) => {}
        }
    }
    if let Some(low) = &f.low_space {
        let _ = write!(line, "; {low}");
    }
    if f.held_deletes > 0 {
        let _ = write!(
            line,
            "; {} deleted files held: `tidefell folder deletes {}`",
            f.held_deletes, f.id
        );
    }
    if f.pending_files > 0 {
        let _ = write!(
            line,
            ", {} file(s), {} to fetch",
            f.pending_files,
            size(f.pending_bytes)
        );
    }
    if let (Some(at), Some(took)) = (f.last_scan_ms, f.last_scan_duration_ms) {
        let _ = write!(line, "; last scan {} ({took} ms)", time(at));
    }
    let behind: Vec<&str> = f
        .peers
        .iter()
        .filter(|p| !p.up_to_date)
        .map(|p| p.device.get(..8).unwrap_or(&p.device))
        .collect();
    if !behind.is_empty() {
        let _ = write!(line, "; not up to date with {}", behind.join(", "));
    }
    line
}

/// The files in a zip file and their sizes.
pub(crate) fn zip_entries(zip: &[u8]) -> Vec<(String, u64)> {
    let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(zip)) else {
        return Vec::new();
    };
    (0..archive.len())
        .filter_map(|i| {
            let file = archive.by_index(i).ok()?;
            Some((file.name().ok()?.into_owned(), file.size()))
        })
        .collect()
}

/// A folder's activity, one change per line.
pub(crate) fn activity(activity: &[Activity]) -> String {
    if activity.is_empty() {
        return "no activity".into();
    }
    activity
        .iter()
        .map(|a| {
            let what = match a.kind {
                ChangeKind::Added => "added",
                ChangeKind::Changed => "changed",
                ChangeKind::Deleted => "deleted",
                ChangeKind::Renamed => "renamed",
                ChangeKind::Restored => "restored",
                ChangeKind::Conflict => "conflict",
            };
            let path = match &a.old_path {
                Some(old) => format!("{old} -> {}", a.path),
                None => a.path.clone(),
            };
            let who = if a.local {
                "this device"
            } else {
                &a.device_name
            };
            format!("{}  {what:<8}  {path}  by {who}", time(a.time_ms))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `tidefell doctor`'s checks, one per line, with advice under warnings and failures.
pub(crate) fn checks(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            let level = match c.level {
                CheckLevel::Pass => "pass",
                CheckLevel::Warning => "WARN",
                CheckLevel::Failure => "FAIL",
            };
            let mut line = format!("{level}  {:<8} {}", c.name, c.detail);
            if let Some(advice) = &c.advice {
                let _ = write!(line, "\n      {advice}");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Paths of one folder that failed.
pub(crate) fn path_problems(problems: &[PathProblem]) -> String {
    if problems.is_empty() {
        return "no problems".into();
    }
    problems
        .iter()
        .map(|p| {
            let operation = match p.operation {
                Operation::Scan => "scan",
                Operation::Fetch => "fetch",
                Operation::Write => "write",
                Operation::Delete => "delete",
            };
            let kind = match p.kind {
                ProblemKind::PermissionDenied => "permission denied",
                ProblemKind::DiskFull => "disk full",
                ProblemKind::ContentUnavailable => "content unavailable",
                ProblemKind::NameNotAllowed => "name not allowed",
                ProblemKind::ChangedWhileWriting => "changed while writing",
                ProblemKind::Other => "error",
            };
            format!(
                "{}\n  {operation}: {kind}: {}\n  since {}, {} attempt(s)",
                p.path,
                p.message,
                time(p.since_ms),
                p.attempts
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        if let Some(r) = &f.rebuild {
            let _ = writeln!(out, "    {}", rebuild(r));
        }
    }
    out.trim_end().to_string()
}

/// Whose index a folder being rebuilt waits for.
fn waiting_for(r: &Rebuild) -> String {
    if r.waiting_for.is_empty() {
        "waiting for a device sharing the folder to connect".into()
    } else {
        let devices: Vec<&str> = r
            .waiting_for
            .iter()
            .map(|d| d.get(..8).unwrap_or(d))
            .collect();
        format!("waiting for {}", devices.join(", "))
    }
}

/// A folder's last rebuild (ADR 0034).
fn rebuild(r: &Rebuild) -> String {
    match r.finished_ms {
        Some(at) => format!(
            "rebuilt {}, started {} as {}",
            time(at),
            time(r.started_ms),
            r.reason
        ),
        None => format!(
            "rebuilding since {}, as {}; {}",
            time(r.started_ms),
            r.reason,
            waiting_for(r)
        ),
    }
}

/// The deletes a scan held (ADR 0037), and what can be done about them.
pub(crate) fn held_deletes(id: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return "no deletes are held".into();
    }
    let mut out = format!(
        "{} files were found deleted at once, so their deletes are held:\n",
        paths.len()
    );
    for path in paths.iter().take(20) {
        let _ = writeln!(out, "  {path}");
    }
    if paths.len() > 20 {
        let _ = writeln!(out, "  and {} more", paths.len() - 20);
    }
    let _ = write!(
        out,
        "`tidefell folder deletes {id} confirm` deletes them on every device;\n\
         `tidefell folder deletes {id} restore` brings them back from your other devices"
    );
    out
}

/// The answer to `tidefell folder rebuild`.
pub(crate) fn rebuilding(f: &Folder) -> String {
    let what = f.rebuild.as_ref().map(rebuild).unwrap_or_default();
    format!(
        "rebuilding {}: files on disk are matched to what other devices send\n{what}",
        f.id
    )
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
        let by = if f.author_name.is_empty() {
            String::new()
        } else {
            format!("  by {}", f.author_name)
        };
        let _ = writeln!(
            out,
            "{:>10}  {}  {}{by}{notes}",
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

/// A crash report: the issue's title and body, then where to file it.
pub(crate) fn crash_report(r: &CrashReport) -> String {
    let file = match &r.url {
        Some(url) => format!(
            "Open this link to file it (its body may be cut to fit; paste the rest from above):\n{url}"
        ),
        None => "Copy the title and body above into a new issue on the project's tracker. Nothing \
                 has been sent."
            .to_owned(),
    };
    format!("Title: {}\n\n{}\n{file}", r.title, r.body)
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

pub(crate) fn revocations(list: &[tidefell_sdk::types::Revocation]) -> String {
    if list.is_empty() {
        return "no revoked devices".into();
    }
    list.iter()
        .map(|r| {
            let name = r.name.as_deref().map_or(String::new(), |n| format!("{n} "));
            format!("{name}{} revoked {}", r.device, time(r.revoked_ms))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_of_loss_are_read_in_utc_unless_offset() {
        assert_eq!(parse_time("1970-01-01 00:00").unwrap(), 0);
        assert_eq!(parse_time("2026-10-16 14:30").unwrap(), 1_792_161_000_000);
        assert_eq!(
            parse_time("2026-10-16T14:30:05Z").unwrap(),
            1_792_161_005_000
        );
        assert_eq!(
            parse_time("2026-10-16T16:30+02:00").unwrap(),
            1_792_161_000_000
        );
        assert_eq!(
            parse_time("2026-10-16 09:30-0500").unwrap(),
            1_792_161_000_000
        );
        assert_eq!(
            time(parse_time("2024-02-29 23:59").unwrap()),
            "2024-02-29 23:59"
        );
        for bad in [
            "yesterday",
            "2026-13-01 00:00",
            "2026-10-16",
            "2026-10-16 25:00",
        ] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
    }

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
            state: FolderState::Idle,
            error: None,
            pending_files: 0,
            pending_bytes: 0,
            last_scan_ms: None,
            last_scan_duration_ms: None,
            peers: Vec::new(),
            problems: 0,
            rebuild: None,
            held_deletes: 0,
            low_space: None,
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
    fn held_deletes_list_a_few_and_say_what_to_do() {
        assert_eq!(held_deletes("f", &[]), "no deletes are held");
        let paths: Vec<String> = (0..25).map(|n| format!("file-{n}")).collect();
        let text = held_deletes("f", &paths);
        assert!(text.starts_with("25 files were found deleted at once"));
        assert!(text.contains("  file-19\n") && !text.contains("file-20\n"));
        assert!(text.contains("and 5 more"));
        assert!(text.contains("tidefell folder deletes f confirm"));
        assert!(text.contains("tidefell folder deletes f restore"));
    }

    #[test]
    fn rebuild_lines() {
        let mut r = Rebuild {
            started_ms: 0,
            reason: "its metadata database was damaged".into(),
            finished_ms: None,
            waiting_for: vec!["ab".repeat(32)],
        };
        assert!(rebuild(&r).ends_with("as its metadata database was damaged; waiting for abababab"));
        r.waiting_for.clear();
        assert!(waiting_for(&r).contains("to connect"));
        r.finished_ms = Some(0);
        assert!(rebuild(&r).starts_with("rebuilt "));
    }

    #[test]
    fn activity_lines() {
        let change = |kind, old_path: Option<&str>, local| Activity {
            time_ms: 0,
            kind,
            path: "b.txt".into(),
            old_path: old_path.map(Into::into),
            device: "ab".repeat(32),
            device_name: "phone".into(),
            local,
        };
        assert_eq!(activity(&[]), "no activity");
        assert_eq!(
            activity(&[
                change(ChangeKind::Renamed, Some("a.txt"), false),
                change(ChangeKind::Added, None, true),
            ]),
            "1970-01-01 00:00  renamed   a.txt -> b.txt  by phone\n\
             1970-01-01 00:00  added     b.txt  by this device"
        );
    }

    #[test]
    fn crash_reports() {
        let mut report = CrashReport {
            title: "Crash: boom".into(),
            body: "Tidefell stopped unexpectedly: boom\n".into(),
            url: None,
        };
        let text = crash_report(&report);
        assert!(text.starts_with("Title: Crash: boom\n\nTidefell stopped unexpectedly: boom\n"));
        assert!(text.contains("Nothing has been sent"));
        report.url = Some("https://example.org/issues/new?title=x".into());
        assert!(crash_report(&report).ends_with("\nhttps://example.org/issues/new?title=x"));
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
