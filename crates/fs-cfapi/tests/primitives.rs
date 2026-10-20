//! Phase 8 step 5's second spike (ADR 0042): how Windows treats the placeholder operations the
//! engine's seam would rely on. Each test prints what it found (`primitive:` lines in the log)
//! and asserts only that the operation ran, so one run answers every question. Ignored by
//! default, as findings rather than checks: run them with
//! `cargo test -p tidefell-fs-cfapi --test primitives -- --ignored --nocapture`.
#![cfg(windows)]

use std::collections::HashMap;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{info, ticket, Request, SyncFilter};
use cloud_filter::metadata::Metadata;
use cloud_filter::placeholder::{ConvertOptions, PinOptions, PinState, Placeholder, UpdateOptions};
use cloud_filter::placeholder_file::PlaceholderFile;
use cloud_filter::root::{
    Connection, HydrationType, PopulationType, SecurityId, Session, SyncRootId, SyncRootIdBuilder,
    SyncRootInfo,
};
use cloud_filter::utility::WriteAt;

const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// What the provider was called for: the callback, its path, the calling process and anything
/// else worth knowing.
#[derive(Debug, Clone)]
struct Call {
    kind: &'static str,
    path: PathBuf,
    pid: u32,
    extra: String,
}

/// Serves content by blob, optionally after a delay, and records every callback.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<Call>>,
    content: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    delay: Mutex<Duration>,
    /// Report progress every ten seconds while delaying.
    progress: Mutex<bool>,
    /// How the fetch is answered.
    answer: Mutex<Answer>,
}

/// How a fetch is answered.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Answer {
    /// In the callback, after any delay.
    #[default]
    Inline,
    /// By failing the callback at once.
    FailNow,
    /// From a thread of its own after the delay, the callback returning at once.
    Spawned,
    /// By failing the ticket from a thread of its own after the delay.
    SpawnedFail,
}

impl Recorder {
    fn record(&self, kind: &'static str, request: &Request, extra: String) {
        self.calls.lock().unwrap().push(Call {
            kind,
            path: request.path(),
            pid: request.process().map_or(0, |p| p.id()),
            extra,
        });
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl SyncFilter for Recorder {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        let range = info.required_file_range();
        self.record("fetch_data", &request, format!("{range:?}"));
        let delay = *self.delay.lock().unwrap();
        let answer = *self.answer.lock().unwrap();
        if answer == Answer::FailNow {
            return Err(CloudErrorKind::NetworkUnavailable);
        }
        if matches!(answer, Answer::Spawned | Answer::SpawnedFail) {
            let content = self
                .content
                .lock()
                .unwrap()
                .get(request.file_blob())
                .cloned()
                .unwrap_or_default();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let start = usize::try_from(range.start).unwrap_or(0).min(content.len());
                let done = if answer == Answer::SpawnedFail {
                    ticket.fail(CloudErrorKind::NetworkUnavailable)
                } else {
                    ticket.write_at(&content[start..], range.start).map(drop)
                };
                report("spawned", format!("answered after {delay:?}: {done:?}"));
            });
            return Ok(());
        }
        if *self.progress.lock().unwrap() {
            let start = Instant::now();
            while start.elapsed() < delay {
                std::thread::sleep(Duration::from_secs(10));
                let reported = ticket.report_progress(range.end, 0);
                report(
                    "progress",
                    format!("after {:?}: {reported:?}", start.elapsed()),
                );
            }
        } else {
            std::thread::sleep(delay);
        }
        // Returning an error makes cloud-filter fail the request, and panic if that fails, so
        // nothing is returned but success.
        let Some(content) = self
            .content
            .lock()
            .unwrap()
            .get(request.file_blob())
            .cloned()
        else {
            self.record("fetch_data unknown", &request, String::new());
            return Ok(());
        };
        let start = usize::try_from(range.start).unwrap_or(0).min(content.len());
        // Failing a request Windows already cancelled makes cloud-filter panic, so a failed
        // write is recorded and not returned.
        if let Err(e) = ticket.write_at(&content[start..], range.start) {
            self.record("fetch_data write failed", &request, format!("{e}"));
        }
        Ok(())
    }

    fn cancel_fetch_data(&self, request: Request, info: info::CancelFetchData) {
        self.record(
            "cancel_fetch_data",
            &request,
            format!("timeout {} user {}", info.timeout(), info.user_cancelled()),
        );
    }

    fn fetch_placeholders(
        &self,
        _request: Request,
        ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> CResult<()> {
        ticket
            .pass_with_placeholder(&mut [])
            .map_err(|_| CloudErrorKind::InvalidRequest)
    }

    fn dehydrate(
        &self,
        request: Request,
        ticket: ticket::Dehydrate,
        info: info::Dehydrate,
    ) -> CResult<()> {
        self.record("dehydrate", &request, format!("{:?}", info.reason()));
        ticket.pass().map_err(|_| CloudErrorKind::InvalidRequest)
    }

    fn dehydrated(&self, request: Request, info: info::Dehydrated) {
        self.record("dehydrated", &request, format!("{:?}", info.reason()));
    }

    fn delete(&self, request: Request, ticket: ticket::Delete, info: info::Delete) -> CResult<()> {
        self.record(
            "delete",
            &request,
            format!("undelete {}", info.is_undelete()),
        );
        ticket.pass().map_err(|_| CloudErrorKind::InvalidRequest)
    }

    fn deleted(&self, request: Request, _info: info::Deleted) {
        self.record("deleted", &request, String::new());
    }

    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        self.record(
            "rename",
            &request,
            format!(
                "to {:?} source in scope {} target in scope {}",
                info.target_path(),
                info.source_in_scope(),
                info.target_in_scope()
            ),
        );
        ticket.pass().map_err(|_| CloudErrorKind::InvalidRequest)
    }

    fn renamed(&self, request: Request, info: info::Renamed) {
        self.record(
            "renamed",
            &request,
            format!("from {:?}", info.source_path()),
        );
    }

    fn state_changed(&self, changes: Vec<PathBuf>) {
        let mut calls = self.calls.lock().unwrap();
        for path in changes {
            calls.push(Call {
                kind: "state_changed",
                path,
                pid: 0,
                extra: String::new(),
            });
        }
    }
}

/// A registered, connected sync root in the user's profile, unregistered when dropped.
struct Root {
    path: PathBuf,
    connection: Option<Connection<Recorder>>,
    id: SyncRootId,
    _dir: tempfile::TempDir,
}

impl Root {
    fn new(name: &str, hydration: HydrationType) -> Self {
        let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
        let dir = tempfile::Builder::new()
            .prefix("Tidefell primitives ")
            .tempdir_in(profile)
            .unwrap();
        let path = dir.path().join(name);
        std::fs::create_dir(&path).unwrap();
        let id = SyncRootIdBuilder::new(format!("TidefellPrimitive{name}"))
            .user_security_id(SecurityId::current_user().unwrap())
            .account_name("primitives")
            .build();
        if id.is_registered().unwrap() {
            id.unregister().unwrap();
        }
        id.register(
            SyncRootInfo::default()
                .with_display_name(format!("Tidefell primitives {name}"))
                .with_hydration_type(hydration)
                .with_population_type(PopulationType::AlwaysFull)
                .with_allow_pinning(true)
                .with_icon("%SystemRoot%\\system32\\imageres.dll,0")
                .with_version("0.0.1")
                .with_path(&path)
                .unwrap(),
        )
        .unwrap();
        let mut root = Self {
            path,
            connection: None,
            id,
            _dir: dir,
        };
        root.connect();
        root
    }

    fn connect(&mut self) {
        self.connection = Some(
            Session::new()
                .connect(&self.path, Recorder::default())
                .unwrap(),
        );
    }

    fn filter(&self) -> &Recorder {
        self.connection.as_ref().unwrap().filter()
    }

    /// A dehydrated placeholder at `name` serving `content`.
    fn placeholder(&self, name: &str, content: &[u8]) -> PathBuf {
        self.filter()
            .content
            .lock()
            .unwrap()
            .insert(name.as_bytes().to_vec(), content.to_vec());
        PlaceholderFile::new(name)
            .metadata(Metadata::file().size(content.len() as u64))
            .blob(name.as_bytes().to_vec())
            .mark_in_sync()
            .create::<&Path>(&self.path)
            .unwrap();
        self.path.join(name)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        self.connection.take();
        let _ = self.id.unregister();
    }
}

/// Size, modification time, attributes and file ID.
fn describe(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            let placeholder = Placeholder::open(path).ok().and_then(|p| {
                p.info().ok().flatten().map(|i| {
                    format!(
                        "in sync {} pin {:?} on disk {} file id {}",
                        i.is_in_sync(),
                        i.pin_state(),
                        i.on_disk_data_size(),
                        i.file_id()
                    )
                })
            });
            format!(
                "size {} modified {:?} attributes {:#x} placeholder {placeholder:?}",
                m.len(),
                m.modified().ok(),
                m.file_attributes()
            )
        }
        Err(e) => format!("missing: {e}"),
    }
}

fn report(test: &str, what: impl std::fmt::Display) {
    eprintln!("primitive: {test}: {what}");
}

fn report_calls(test: &str, root: &Root) {
    let me = std::process::id();
    for c in root.filter().calls() {
        report(
            test,
            format!(
                "callback {} {:?} pid {} ({}) {}",
                c.kind,
                c.path,
                c.pid,
                if c.pid == me {
                    "this process"
                } else {
                    "another"
                },
                c.extra
            ),
        );
    }
}

/// Settles callbacks that arrive after the call that caused them.
fn settle() {
    std::thread::sleep(Duration::from_secs(2));
}

/// Q1: an ordinary file renamed over a dehydrated placeholder, from inside the sync root and from
/// a staging directory outside it, then converted to an in-sync placeholder.
#[test]
#[ignore = "a spike: prints findings"]
fn renaming_a_file_over_a_placeholder() {
    let root = Root::new("Rename", HydrationType::Full);
    let staging = tempfile::tempdir().unwrap(); // under %LOCALAPPDATA%, as a data directory is
    for (case, source) in [
        ("inside", root.path.join("stage-inside.tmp")),
        ("outside", staging.path().join("stage-outside.tmp")),
    ] {
        let test = format!("rename over placeholder from {case}");
        let target = root.placeholder(&format!("{case}.txt"), b"old content");
        report(&test, format!("before: {}", describe(&target)));
        std::fs::write(&source, b"new content!").unwrap();
        report(&test, format!("staged: {}", describe(&source)));
        let renamed = std::fs::rename(&source, &target);
        settle();
        report(
            &test,
            format!("rename: {renamed:?}; after: {}", describe(&target)),
        );
        if renamed.is_ok() {
            let converted = Placeholder::options()
                .write_access()
                .exclusive()
                .open(&target)
                .and_then(|mut p| {
                    p.convert_to_placeholder(
                        ConvertOptions::default()
                            .mark_in_sync()
                            .blob(format!("{case}.txt").into_bytes()),
                        None,
                    )
                    .map(drop)
                });
            report(
                &test,
                format!("convert: {converted:?}; after: {}", describe(&target)),
            );
            report(
                &test,
                format!(
                    "content {:?}",
                    std::fs::read(&target).map(|c| String::from_utf8_lossy(&c).into_owned())
                ),
            );
        }
    }
    report_calls("rename over placeholder", &root);
}

/// Q2: a dehydrated placeholder pinned ("Always keep on this device"): does Windows hydrate it?
#[test]
#[ignore = "a spike: prints findings"]
fn pinning_a_placeholder() {
    let root = Root::new("Pin", HydrationType::Full);
    let target = root.placeholder("pinned.txt", b"pinned content");
    let pinned = Placeholder::open(&target).and_then(|mut p| {
        p.mark_pin(PinState::Pinned, PinOptions::default())
            .map(drop)
    });
    report("pin", format!("mark_pin: {pinned:?}"));
    std::thread::sleep(Duration::from_secs(10));
    report("pin", format!("10 s later: {}", describe(&target)));
    report_calls("pin", &root);
}

/// Q3: dehydrating a hydrated placeholder, and a file written normally then converted.
#[test]
#[ignore = "a spike: prints findings"]
fn dehydrating_in_place() {
    let root = Root::new("Dehydrate", HydrationType::Full);
    let hydrated = root.placeholder("hydrated.txt", b"hydrated content");
    root.filter()
        .content
        .lock()
        .unwrap()
        .insert(b"written.txt".to_vec(), b"written content".to_vec());
    std::fs::read(&hydrated).unwrap();
    let written = root.path.join("written.txt");
    std::fs::write(&written, b"written content").unwrap();
    let converted = Placeholder::options()
        .write_access()
        .exclusive()
        .open(&written)
        .and_then(|mut p| {
            p.convert_to_placeholder(
                ConvertOptions::default()
                    .mark_in_sync()
                    .blob(b"written.txt".to_vec()),
                None,
            )
            .map(drop)
        });
    report("dehydrate", format!("convert written: {converted:?}"));
    for path in [&hydrated, &written] {
        let test = format!("dehydrate {:?}", path.file_name().unwrap());
        report(&test, format!("before: {}", describe(path)));
        let dehydrated = Placeholder::options()
            .write_access()
            .exclusive()
            .open(path)
            .and_then(|mut p| {
                p.update(UpdateOptions::default().dehydrate(), None)
                    .map(drop)
            });
        report(&test, format!("update dehydrate: {dehydrated:?}"));
        report(&test, format!("after: {}", describe(path)));
        let attributes = std::fs::metadata(path).unwrap().file_attributes();
        report(
            &test,
            format!(
                "data not on disk: {}",
                attributes & RECALL_ON_DATA_ACCESS != 0
            ),
        );
    }
    // A dehydrated file read again fetches it through the provider.
    root.filter()
        .content
        .lock()
        .unwrap()
        .insert(b"written.txt".to_vec(), b"written content".to_vec());
    report(
        "dehydrate",
        format!(
            "read written again: {:?}",
            std::fs::read(&written).map(|c| c.len())
        ),
    );
    report_calls("dehydrate", &root);
}

/// Q4: which process each delete and rename callback names, for this process and a child.
#[test]
#[ignore = "a spike: prints findings"]
fn process_ids_of_deletes_and_renames() {
    let root = Root::new("Pids", HydrationType::Full);
    report("pids", format!("this process is {}", std::process::id()));
    let mine = root.placeholder("mine.txt", b"mine");
    let theirs = root.placeholder("theirs.txt", b"theirs");
    let moved = root.placeholder("moved.txt", b"moved");
    let moved_by_them = root.placeholder("moved-by-them.txt", b"moved by them");
    let dir = root.path.join("dir");
    std::fs::create_dir(&dir).unwrap();
    report(
        "pids",
        format!("remove mine: {:?}", std::fs::remove_file(&mine)),
    );
    let del = std::process::Command::new("cmd")
        .args(["/C", "del"])
        .arg(&theirs)
        .status();
    report(
        "pids",
        format!("cmd del: {del:?}, exists {}", theirs.exists()),
    );
    report(
        "pids",
        format!(
            "rename mine: {:?}",
            std::fs::rename(&moved, dir.join("moved.txt"))
        ),
    );
    let ren = std::process::Command::new("cmd")
        .args(["/C", "move"])
        .arg(&moved_by_them)
        .arg(dir.join("moved-by-them.txt"))
        .status();
    report("pids", format!("cmd move: {ren:?}"));
    settle();
    report_calls("pids", &root);
}

/// Q5: a placeholder deleted while the provider is disconnected.
#[test]
#[ignore = "a spike: prints findings"]
fn deleting_while_disconnected() {
    let mut root = Root::new("Offline", HydrationType::Full);
    let target = root.placeholder("offline.txt", b"offline");
    root.connection.take();
    let removed = std::fs::remove_file(&target);
    report(
        "disconnected",
        format!("remove: {removed:?}, exists after {}", target.exists()),
    );
    root.connect();
    settle();
    report_calls("disconnected (after reconnecting)", &root);
}

/// Q6: a fetch that takes longer than the platform's 60-second timeout, with full and
/// progressive hydration.
#[test]
#[ignore = "a spike: prints findings"]
fn a_fetch_past_the_timeout() {
    let roots: Vec<_> = [
        ("Full", HydrationType::Full, false),
        ("Progressive", HydrationType::Progressive, false),
        ("FullWithProgress", HydrationType::Full, true),
    ]
    .into_iter()
    .map(|(name, hydration, progress)| {
        let root = Root::new(&format!("Slow{name}"), hydration);
        *root.filter().delay.lock().unwrap() = Duration::from_secs(70);
        *root.filter().progress.lock().unwrap() = progress;
        let target = root.placeholder("slow.txt", b"slow content");
        (name, root, target)
    })
    .collect();
    std::thread::scope(|s| {
        for (name, root, target) in &roots {
            s.spawn(move || {
                let start = Instant::now();
                let read = std::fs::read(target);
                report(
                    &format!("timeout {name}"),
                    format!(
                        "read after {:?}: {:?}",
                        start.elapsed(),
                        read.map(|c| c.len())
                    ),
                );
                settle();
                report_calls(&format!("timeout {name}"), root);
            });
        }
    });
}

/// Q7: a fetch that sends nothing and reports success, the only safe way to fail one through
/// cloud-filter: what the reading program gets, and when.
#[test]
#[ignore = "a spike: prints findings"]
fn a_fetch_that_sends_nothing() {
    let root = Root::new("Nothing", HydrationType::Full);
    let target = root.placeholder("nothing.txt", b"never sent");
    root.filter().content.lock().unwrap().clear();
    let start = Instant::now();
    let read = std::fs::read(&target);
    report(
        "sends nothing",
        format!(
            "read after {:?}: {:?}",
            start.elapsed(),
            read.map(|c| c.len())
        ),
    );
    settle();
    report_calls("sends nothing", &root);
}

/// Q8: a fetch failed at once from the callback, through the patched cloud-filter: what the
/// reading program gets, and that nothing panics.
#[test]
#[ignore = "a spike: prints findings"]
fn a_fetch_failed_at_once() {
    let root = Root::new("FailNow", HydrationType::Full);
    let target = root.placeholder("fail.txt", b"never sent");
    *root.filter().answer.lock().unwrap() = Answer::FailNow;
    let start = Instant::now();
    let read = std::fs::read(&target);
    report(
        "fail now",
        format!("read after {:?}: {read:?}", start.elapsed()),
    );
    report_calls("fail now", &root);
}

/// Q9: fetches answered from threads of their own, the callback returning at once: whether
/// three slow reads run together, and a fetch failed from its thread.
#[test]
#[ignore = "a spike: prints findings"]
fn fetches_answered_from_other_threads() {
    let root = Root::new("Spawned", HydrationType::Full);
    *root.filter().delay.lock().unwrap() = Duration::from_secs(5);
    *root.filter().answer.lock().unwrap() = Answer::Spawned;
    let targets: Vec<_> = (0..3)
        .map(|i| root.placeholder(&format!("slow{i}.txt"), format!("slow {i}").as_bytes()))
        .collect();
    let start = Instant::now();
    std::thread::scope(|s| {
        for target in &targets {
            s.spawn(move || {
                let read = std::fs::read(target).map(|c| String::from_utf8_lossy(&c).into_owned());
                report(
                    "spawned",
                    format!(
                        "{:?} read after {:?}: {read:?}",
                        target.file_name().unwrap(),
                        start.elapsed()
                    ),
                );
            });
        }
    });
    report("spawned", format!("all three after {:?}", start.elapsed()));

    let failing = root.placeholder("failing.txt", b"never sent");
    *root.filter().answer.lock().unwrap() = Answer::SpawnedFail;
    let start = Instant::now();
    let read = std::fs::read(&failing);
    report(
        "spawned fail",
        format!("read after {:?}: {read:?}", start.elapsed()),
    );
    report_calls("spawned", &root);
}
