//! Writing a folder's changes to its database off the folder lock (ADR 0034).
//!
//! A save takes what changed while the folder is locked, which is quick, and queues it here. A
//! thread of the folder's own commits the queue in order, so a later change never lands before
//! an earlier one, and retries a commit that fails. It records the last sequence committed:
//! peers are sent versions only up to it, so nothing reaches them that a crash could lose.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::warn;

#[cfg(debug_assertions)]
use crate::db::Loaded;
use crate::db::{Changes, FolderDb};
use crate::{Error, Result};

/// Called after each commit, to tell peers about the versions it made visible.
pub(crate) type OnCommit = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Queue {
    pending: VecDeque<Changes>,
    /// A commit is under way.
    busy: bool,
    closing: bool,
    /// Why the last commit failed, until one succeeds.
    failed: Option<String>,
}

struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when work is queued or the committer should stop.
    work: Condvar,
    /// Signalled when the queue empties.
    idle: Condvar,
    committed: AtomicU64,
    db: Mutex<FolderDb>,
    on_commit: Mutex<Option<OnCommit>>,
    /// The folder's state directory, where its activity log is.
    dir: std::path::PathBuf,
    /// Lines in the activity log's file, so it is rewritten once it grows (ADR 0030).
    activity_lines: std::sync::atomic::AtomicUsize,
}

/// One folder's committer.
pub(crate) struct Committer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Committer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Committer")
            .field("committed", &self.committed())
            .finish_non_exhaustive()
    }
}

/// How long to wait before retrying a commit that failed.
const RETRY: Duration = Duration::from_secs(2);

impl Committer {
    /// Starts committing to `db`, whose versions are committed up to `committed`, and writing
    /// the activity log in `dir`, which has `activity_lines` lines.
    pub(crate) fn start(
        db: FolderDb,
        committed: u64,
        dir: std::path::PathBuf,
        activity_lines: usize,
    ) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
            idle: Condvar::new(),
            committed: AtomicU64::new(committed),
            db: Mutex::new(db),
            on_commit: Mutex::new(None),
            dir,
            activity_lines: std::sync::atomic::AtomicUsize::new(activity_lines),
        });
        let thread = std::thread::Builder::new()
            .name("tidefell-commit".into())
            .spawn({
                let shared = shared.clone();
                move || run(&shared)
            })
            .ok();
        Self { shared, thread }
    }

    /// Calls `on_commit` after each commit from now on.
    pub(crate) fn on_commit(&self, on_commit: OnCommit) {
        *self.shared.on_commit.lock().unwrap() = Some(on_commit);
    }

    /// Queues `changes` to be committed after everything queued before.
    pub(crate) fn submit(&self, changes: Changes) {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.pending.push_back(changes);
        self.shared.work.notify_one();
    }

    /// The last sequence whose versions are committed.
    pub(crate) fn committed(&self) -> u64 {
        self.shared.committed.load(Ordering::Acquire)
    }

    /// Waits until everything queued is committed. Fails if the last commit failed.
    pub(crate) fn flush(&self) -> Result<()> {
        flush(&self.shared)
    }

    /// Something to wait on for what is queued to be committed, which can outlive the
    /// folder's lock.
    pub(crate) fn waiter(&self) -> Waiter {
        Waiter(self.shared.clone())
    }

    /// Everything committed so far, read back from the database, for checking saves in
    /// development builds.
    #[cfg(debug_assertions)]
    pub(crate) fn read_all(&self) -> Result<Loaded> {
        self.shared.db.lock().unwrap().read_all()
    }

    /// Commits what is queued, then stops; the database closes once this returns.
    pub(crate) fn close(mut self) -> Result<()> {
        let flushed = self.flush();
        self.stop();
        flushed
    }

    fn stop(&mut self) {
        self.shared.queue.lock().unwrap().closing = true;
        self.shared.work.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Committer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Waits for what is queued, as [`Committer::flush`] does.
#[derive(Clone)]
pub(crate) struct Waiter(Arc<Shared>);

impl Waiter {
    pub(crate) fn flush(&self) -> Result<()> {
        flush(&self.0)
    }

    /// Checks the database file for damage, repairing what it can (ADR 0034). Commits wait
    /// meanwhile.
    pub(crate) fn check(&self) -> Result<()> {
        self.0.db.lock().unwrap().check()
    }
}

fn flush(shared: &Shared) -> Result<()> {
    let mut queue = shared.queue.lock().unwrap();
    while (!queue.pending.is_empty() || queue.busy) && queue.failed.is_none() {
        queue = shared.idle.wait(queue).unwrap();
    }
    match &queue.failed {
        Some(reason) => Err(Error::Io {
            path: std::path::PathBuf::new(),
            source: std::io::Error::other(reason.clone()),
        }),
        None => Ok(()),
    }
}

fn run(shared: &Shared) {
    loop {
        let mut changes = {
            let mut queue = shared.queue.lock().unwrap();
            loop {
                if let Some(changes) = queue.pending.pop_front() {
                    queue.busy = true;
                    break changes;
                }
                if queue.closing {
                    return;
                }
                queue = shared.work.wait(queue).unwrap();
            }
        };
        let sequence = changes.header.sequence;
        let at_ms = changes.at_ms;
        let activity = std::mem::take(&mut changes.activity);
        let mut changes = Some(changes);
        while let Some(attempt) = changes.take() {
            let result = shared.db.lock().unwrap().commit(&attempt);
            match result {
                Ok(()) => {
                    shared.queue.lock().unwrap().failed = None;
                }
                Err(e) => {
                    warn!(error = %e, "committing a folder's changes failed; retrying");
                    let mut queue = shared.queue.lock().unwrap();
                    queue.failed = Some(e.to_string());
                    shared.idle.notify_all();
                    if queue.closing {
                        queue.busy = false;
                        return;
                    }
                    drop(queue);
                    std::thread::sleep(RETRY);
                    changes = Some(attempt);
                }
            }
        }
        shared.committed.fetch_max(sequence, Ordering::Release);
        // A record only: failing to write it does not undo the commit.
        let lines = std::cell::Cell::new(shared.activity_lines.load(Ordering::Relaxed));
        if let Err(e) = crate::activity_log::record(&shared.dir, &lines, &activity, at_ms) {
            warn!(error = %e, "writing the activity log failed");
        }
        shared.activity_lines.store(lines.get(), Ordering::Relaxed);
        if let Some(on_commit) = &*shared.on_commit.lock().unwrap() {
            on_commit();
        }
        let mut queue = shared.queue.lock().unwrap();
        queue.busy = false;
        if queue.pending.is_empty() {
            shared.idle.notify_all();
        }
    }
}
