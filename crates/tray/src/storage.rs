//! An encrypted folder's Storage window (ADR 0023's amendment, its section of 2026-10-23): the
//! folder's tree with each file and directory's priority for the storage peers, which the user
//! sets or puts back to the default, and what each storage peer keeps, from which the user drops
//! what is ranked low. What it shows is worked out here, platform-neutral and tested
//! everywhere; the window is in `ui`. Calls to the daemon run on a thread of their own, so the
//! window never waits on them.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;

use shoal_sdk::types::{Priorities, PriorityEntry, StorageDropResult, StoragePeerReport};
use shoal_sdk::Client;

/// A file or directory priority that no rule sets.
pub const DEFAULT_PRIORITY: u8 = 128;

/// A line of the tree: a file or directory, after its parent's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The index of its parent's line, or none at the top.
    pub parent: Option<usize>,
    /// What the line says: its name, priority and where that comes from.
    pub label: String,
    /// Its path within the folder.
    pub path: String,
    /// Whether it is a directory.
    pub directory: bool,
    /// Its priority.
    pub priority: u8,
    /// Whether a rule for exactly this file or directory sets it, which "Use the default"
    /// removes.
    pub set_here: bool,
    /// What gives it its priority, in words.
    pub source: String,
}

/// A directory being put together, its children by name.
#[derive(Default)]
struct Dir<'a> {
    entry: Option<&'a PriorityEntry>,
    dirs: BTreeMap<&'a str, Dir<'a>>,
    files: BTreeMap<&'a str, &'a PriorityEntry>,
}

/// The tree's lines: directories before files, each by name, every line after its parent's.
/// A directory the folder does not list on its own is shown, at the default, for what is in it.
pub fn rows(priorities: &Priorities) -> Vec<Row> {
    let mut top = Dir::default();
    for entry in &priorities.entries {
        let mut dir = &mut top;
        let mut parts = entry.path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                if entry.directory {
                    dir.dirs.entry(part).or_default().entry = Some(entry);
                } else {
                    dir.files.insert(part, entry);
                }
            } else {
                dir = dir.dirs.entry(part).or_default();
            }
        }
    }
    let mut rows = Vec::new();
    add(&mut rows, priorities, &top, None, "");
    rows
}

fn add(rows: &mut Vec<Row>, priorities: &Priorities, dir: &Dir, parent: Option<usize>, at: &str) {
    for (name, sub) in &dir.dirs {
        let path = if at.is_empty() {
            (*name).to_owned()
        } else {
            format!("{at}/{name}")
        };
        let (priority, rule) = sub
            .entry
            .map_or((DEFAULT_PRIORITY, None), |e| (e.priority, e.rule));
        rows.push(row(priorities, parent, name, &path, true, priority, rule));
        let index = rows.len() - 1;
        add(rows, priorities, sub, Some(index), &path);
    }
    for (name, entry) in &dir.files {
        rows.push(row(
            priorities,
            parent,
            name,
            &entry.path,
            false,
            entry.priority,
            entry.rule,
        ));
    }
}

fn row(
    priorities: &Priorities,
    parent: Option<usize>,
    name: &str,
    path: &str,
    directory: bool,
    priority: u8,
    rule: Option<u32>,
) -> Row {
    let rule = rule.and_then(|i| priorities.rules.get(i as usize));
    let set_here = rule.is_some_and(|r| r.pattern == pattern_for(path, directory));
    let source = match rule {
        None => "the default".to_owned(),
        Some(_) if set_here => "set here".to_owned(),
        Some(r) => format!("from the rule for {}", r.pattern),
    };
    Row {
        parent,
        label: format!(
            "{name}{}   {priority} ({})",
            if directory { "/" } else { "" },
            word(priority)
        ),
        path: path.to_owned(),
        directory,
        priority,
        set_here,
        source,
    }
}

/// A priority in a word.
pub fn word(priority: u8) -> &'static str {
    match priority {
        0 => "never stored",
        1..=63 => "low",
        64..=127 => "below normal",
        128 => "normal",
        129..=191 => "above normal",
        192..=255 => "high",
    }
}

/// The rule pattern for exactly the file or directory at `path`: anchored at the folder's root,
/// a directory's ending in `/`, with what patterns treat as special escaped.
pub fn pattern_for(path: &str, directory: bool) -> String {
    let mut pattern = String::with_capacity(path.len() + 2);
    pattern.push('/');
    let trailing = path.len() - path.trim_end_matches(' ').len();
    for (i, c) in path.char_indices() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') || (c == ' ' && i >= path.len() - trailing) {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    if directory {
        pattern.push('/');
    }
    pattern
}

/// Reads a priority as typed: a whole number from 0 to 255.
pub fn parse_priority(text: &str) -> Result<u8, String> {
    text.trim().parse().map_err(|_| {
        format!(
            "{:?} is not a priority: give a whole number from 0 to 255",
            text.trim()
        )
    })
}

/// What each storage peer keeps, in lines: what it holds and its room, then for each priority
/// how much of its files it has.
pub fn usage(reports: &[StoragePeerReport]) -> Vec<String> {
    if reports.is_empty() {
        return vec!["The folder has no storage peers.".to_owned()];
    }
    let mut lines = Vec::new();
    for report in reports {
        let name = short(&report.device);
        match (&report.error, report.connected, report.held_bytes) {
            (Some(error), _, _) => lines.push(format!("{name}: did not say ({error})")),
            (None, false, _) => lines.push(format!("{name}: not connected")),
            (None, true, Some(held)) => lines.push(format!(
                "{name}: holds {}, room for {} more",
                bytes(held),
                bytes(report.room_bytes.unwrap_or(0))
            )),
            (None, true, None) => lines.push(format!("{name}: did not say")),
        }
        for level in &report.levels {
            let state = if level.priority == 0 {
                "never stored there".to_owned()
            } else if level.held_bytes >= level.bytes {
                "complete".to_owned()
            } else {
                format!(
                    "{}%, {} not stored",
                    level.held_bytes * 100 / level.bytes.max(1),
                    bytes(level.bytes - level.held_bytes)
                )
            };
            lines.push(format!(
                "    {:>3}: {} file{}, {}: {state}",
                level.priority,
                level.files,
                if level.files == 1 { "" } else { "s" },
                bytes(level.bytes)
            ));
        }
    }
    lines
}

/// A storage peer's name in the window: the start of its device ID.
pub fn short(device: &str) -> String {
    device.chars().take(12).collect()
}

/// What to ask before dropping from a storage peer.
pub fn drop_question(device: &str, below: u8) -> String {
    format!(
        "Make storage peer {} forget what it holds of files ranked below {below}?\n\nIt does so \
         only now, as you ask, and fetches them again, highest first, only when it has room.",
        short(device)
    )
}

/// What to ask when dropping would leave files with no copy known elsewhere.
pub fn at_risk_question(result: &StorageDropResult) -> String {
    const SHOWN: usize = 15;
    let mut list: Vec<&str> = result
        .at_risk
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect();
    let more;
    if result.at_risk.len() > SHOWN {
        more = format!("and {} more", result.at_risk.len() - SHOWN);
        list.push(&more);
    }
    format!(
        "Nothing was dropped. No copy is known of these files other than on that storage peer, \
         so dropping them may lose them:\n\n{}\n\nDrop them anyway?",
        list.join("\n")
    )
}

/// What dropping did.
pub fn dropped(result: &StorageDropResult) -> String {
    format!(
        "The storage peer dropped {} ({} chunk{}).",
        bytes(result.dropped_bytes),
        result.dropped_chunks,
        if result.dropped_chunks == 1 { "" } else { "s" }
    )
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// What the window asks of the daemon.
pub enum Request {
    /// The priorities and what the storage peers keep.
    Load,
    /// Sets the rule for `pattern`, or removes it for `None`; then loads again.
    Set {
        pattern: String,
        priority: Option<u8>,
    },
    /// Drops from storage peer `device` what is ranked below `below`; then loads again.
    Drop {
        device: String,
        below: u8,
        force: bool,
    },
}

/// What the daemon answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The folder's priorities, and what each storage peer keeps.
    Loaded(Priorities, Vec<StoragePeerReport>),
    /// Asked to drop: what it did, or why nothing was.
    Dropped {
        device: String,
        below: u8,
        result: StorageDropResult,
    },
    /// What was asked failed, and why.
    Failed(String),
}

/// Asks the daemon of `data_dir` what the window needs, on a thread of its own, for as long as
/// it is kept.
pub struct Session {
    requests: mpsc::Sender<Request>,
}

impl Session {
    /// Starts asking about folder `folder`; `answers` is given each answer, from that thread.
    pub fn start(data_dir: &Path, folder: &str, answers: impl Fn(Answer) + Send + 'static) -> Self {
        let client = Client::local(Client::socket_path(data_dir));
        let folder = folder.to_owned();
        let (requests, asked) = mpsc::channel();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => return answers(Answer::Failed(e.to_string())),
            };
            let load = || async {
                let priorities = client.priorities(&folder).await?;
                let reports = client.storage_report(&folder).await?;
                Ok::<_, shoal_sdk::Error>(Answer::Loaded(priorities, reports))
            };
            while let Ok(request) = asked.recv() {
                let answer = runtime.block_on(async {
                    match request {
                        Request::Load => load().await,
                        Request::Set { pattern, priority } => {
                            client.set_priority(&folder, &pattern, priority).await?;
                            load().await
                        }
                        Request::Drop {
                            device,
                            below,
                            force,
                        } => {
                            let result =
                                client.storage_drop(&folder, &device, below, force).await?;
                            Ok(Answer::Dropped {
                                device,
                                below,
                                result,
                            })
                        }
                    }
                });
                answers(answer.unwrap_or_else(|e| Answer::Failed(e.to_string())));
            }
        });
        Self { requests }
    }

    /// Asks `request`; its answer comes later.
    pub fn ask(&self, request: Request) {
        let _ = self.requests.send(request);
    }
}

#[cfg(test)]
mod tests {
    use shoal_sdk::types::{PriorityRule, StorageLevel};

    use super::*;

    fn entry(path: &str, directory: bool, priority: u8, rule: Option<u32>) -> PriorityEntry {
        PriorityEntry {
            path: path.into(),
            directory,
            priority,
            rule,
        }
    }

    #[test]
    fn the_tree_lists_directories_then_files_under_their_parents() {
        let priorities = Priorities {
            rules: vec![
                PriorityRule {
                    priority: 200,
                    pattern: "/Photos/".into(),
                },
                PriorityRule {
                    priority: 0,
                    pattern: "*.iso".into(),
                },
            ],
            entries: vec![
                entry("Photos", true, 200, Some(0)),
                entry("Photos/a.jpg", false, 200, Some(0)),
                entry("Photos/raw/b.cr2", false, 200, Some(0)),
                entry("disc.iso", false, 0, Some(1)),
                entry("notes.md", false, 128, None),
            ],
        };
        let rows = rows(&priorities);
        let lines: Vec<(Option<usize>, &str, u8, bool, &str)> = rows
            .iter()
            .map(|r| {
                (
                    r.parent,
                    r.path.as_str(),
                    r.priority,
                    r.set_here,
                    r.source.as_str(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            [
                (None, "Photos", 200, true, "set here"),
                (Some(0), "Photos/raw", 128, false, "the default"),
                (
                    Some(1),
                    "Photos/raw/b.cr2",
                    200,
                    false,
                    "from the rule for /Photos/"
                ),
                (
                    Some(0),
                    "Photos/a.jpg",
                    200,
                    false,
                    "from the rule for /Photos/"
                ),
                (None, "disc.iso", 0, false, "from the rule for *.iso"),
                (None, "notes.md", 128, false, "the default"),
            ]
        );
        assert_eq!(rows[0].label, "Photos/   200 (high)");
        assert_eq!(rows[4].label, "disc.iso   0 (never stored)");
    }

    #[test]
    fn a_pattern_is_for_exactly_one_path() {
        assert_eq!(pattern_for("Photos", true), "/Photos/");
        assert_eq!(pattern_for("a/b.txt", false), "/a/b.txt");
        assert_eq!(
            pattern_for("what?[1]*.txt", false),
            "/what\\?\\[1\\]\\*.txt"
        );
        assert_eq!(pattern_for("x\\y", false), "/x\\\\y");
        assert_eq!(pattern_for("a b ", false), "/a b\\ ");
        assert_eq!(pattern_for("#not!a comment", false), "/#not!a comment");
    }

    #[test]
    fn a_priority_is_0_to_255() {
        assert_eq!(parse_priority(" 200 "), Ok(200));
        assert_eq!(parse_priority("0"), Ok(0));
        assert!(parse_priority("256").is_err());
        assert!(parse_priority("high").is_err());
    }

    #[test]
    fn says_what_each_storage_peer_keeps() {
        let level = |priority, files, bytes, held_bytes| StorageLevel {
            priority,
            files,
            bytes,
            held_bytes,
        };
        let reports = [
            StoragePeerReport {
                device: "abcdefghijklmnopqrstuvwxyz".into(),
                connected: true,
                quota_bytes: Some(4 << 20),
                held_bytes: Some(3 << 20),
                room_bytes: Some(0),
                levels: vec![
                    level(200, 1, 2 << 20, 2 << 20),
                    level(50, 2, 2 << 20, 1 << 20),
                    level(0, 1, 1 << 20, 0),
                ],
                error: None,
            },
            StoragePeerReport {
                device: "zyxwvutsrqponm".into(),
                connected: false,
                quota_bytes: None,
                held_bytes: None,
                room_bytes: None,
                levels: vec![],
                error: None,
            },
        ];
        assert_eq!(
            usage(&reports),
            [
                "abcdefghijkl: holds 3.0 MiB, room for 0 B more",
                "    200: 1 file, 2.0 MiB: complete",
                "     50: 2 files, 2.0 MiB: 50%, 1.0 MiB not stored",
                "      0: 1 file, 1.0 MiB: never stored there",
                "zyxwvutsrqpo: not connected",
            ]
        );
        assert_eq!(usage(&[]), ["The folder has no storage peers."]);
    }

    #[test]
    fn asks_before_dropping_what_has_no_other_copy() {
        let result = StorageDropResult {
            done: false,
            dropped_chunks: 0,
            dropped_bytes: 0,
            at_risk: (0..20).map(|i| format!("f{i}")).collect(),
        };
        let asked = at_risk_question(&result);
        assert!(asked.contains("f0\nf1\n"));
        assert!(asked.contains("f14\nand 5 more"));
        assert!(!asked.contains("f15"));
        assert_eq!(
            dropped(&StorageDropResult {
                done: true,
                dropped_chunks: 3,
                dropped_bytes: 3 << 20,
                at_risk: vec![],
            }),
            "The storage peer dropped 3.0 MiB (3 chunks)."
        );
    }
}
