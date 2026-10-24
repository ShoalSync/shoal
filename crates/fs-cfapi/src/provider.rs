//! The provider's answers to Windows' callbacks (ADR 0042).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{info, ticket, Request, SyncFilter};
use cloud_filter::placeholder::PinState;
use cloud_filter::utility::WriteAt;
use tracing::{debug, warn};

use crate::backend::{Backend, BackendError};
use crate::ops;

/// How often a fetch still waiting on peers tells Windows so: it cancels one that sends
/// nothing for 60 s.
const KEEPALIVE: Duration = Duration::from_secs(10);
/// Data goes to Windows in pieces of at least this, a multiple of its 4 KiB alignment, except
/// the last.
const PIECE: usize = 1 << 20;
const ALIGN: usize = 4096;

/// Writes of data to Windows, and connections and disconnections of sync roots, kept apart
/// across the process (ADR 0042): a write in flight while another root connected or
/// disconnected hung until Windows gave up on it. Writes share it; connecting and
/// disconnecting take it alone. It is held only for each call, never while the engine waits
/// on peers.
pub(crate) static TRANSFERS: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Work a callback hands over: done on the provider's own thread, in order, so Windows'
/// callback threads return at once and are never held by the engine.
type Job = Box<dyn FnOnce(&dyn Backend) + Send>;

pub(crate) struct Provider {
    root: PathBuf,
    backend: Arc<dyn Backend>,
    jobs: std::sync::Mutex<std::sync::mpsc::Sender<Job>>,
}

impl Provider {
    pub(crate) fn new(root: PathBuf, backend: Arc<dyn Backend>) -> Self {
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        let worker = backend.clone();
        // Ends once the provider, and so the sender, is dropped.
        std::thread::spawn(move || {
            for job in queue {
                job(&*worker);
            }
        });
        Self {
            root,
            backend,
            jobs: std::sync::Mutex::new(jobs),
        }
    }

    fn rel(&self, abs: &Path) -> Option<String> {
        ops::to_rel(&self.root, abs).filter(|rel| !ops::reserved(rel))
    }

    fn later(&self, job: impl FnOnce(&dyn Backend) + Send + 'static) {
        let _ = self.jobs.lock().unwrap().send(Box::new(job));
    }
}

/// Fetches `content` for the placeholder at `rel` into `ticket`, then has the engine record the
/// filled file. Fails the fetch, so the reading program gets an error at once, if the content
/// cannot be had or is no longer current.
fn fill(
    backend: &dyn Backend,
    rel: &str,
    content: shoal_proto::ContentHash,
    total: u64,
    ticket: ticket::FetchData,
) {
    let ticket = Arc::new(ticket);
    let written = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let keepalive = {
        let (ticket, written, finished) = (ticket.clone(), written.clone(), finished.clone());
        std::thread::spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                std::thread::park_timeout(KEEPALIVE);
                if !finished.load(Ordering::Relaxed) {
                    let _ = ticket.report_progress(total, written.load(Ordering::Relaxed));
                }
            }
        })
    };
    let mut buffer: Vec<u8> = Vec::with_capacity(PIECE + ALIGN);
    let mut at = 0u64;
    let mut send = |buffer: &mut Vec<u8>, all: bool| -> std::io::Result<()> {
        let len = if all {
            buffer.len()
        } else {
            buffer.len() / ALIGN * ALIGN
        };
        if len == 0 {
            return Ok(());
        }
        {
            let _transfer = TRANSFERS.read().unwrap_or_else(|e| e.into_inner());
            ticket
                .write_at(&buffer[..len], at)
                .map_err(std::io::Error::other)?;
        }
        at += len as u64;
        written.store(at, Ordering::Relaxed);
        buffer.drain(..len);
        Ok(())
    };
    debug!(path = rel, "filling");
    let fetched = backend.fetch(rel, content, &mut |_offset, data| {
        buffer.extend_from_slice(data);
        if buffer.len() >= PIECE {
            send(&mut buffer, false)?;
        }
        Ok(())
    });
    let result = fetched
        .and_then(|()| send(&mut buffer, true).map_err(|e| BackendError::Other(e.to_string())));
    finished.store(true, Ordering::Relaxed);
    keepalive.thread().unpark();
    let _ = keepalive.join();
    match result {
        Ok(()) => {
            debug!(path = rel, "filled the placeholder");
            backend.scan_path(rel);
        }
        Err(e) => {
            debug!(path = rel, error = %e, "cannot fill the placeholder");
            let _ = ticket.fail(CloudErrorKind::NetworkUnavailable);
        }
    }
}

impl SyncFilter for Provider {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        _info: info::FetchData,
    ) -> CResult<()> {
        let path = request.path();
        let (Some(rel), Some((_, content))) =
            (self.rel(&path), ops::read_blob(request.file_blob()))
        else {
            return Err(CloudErrorKind::InvalidRequest);
        };
        let total = request.file_size();
        let process = request.try_process();
        debug!(
            path = %rel,
            process = process.as_ref().map_or(0, |p| p.id()),
            image = ?process.as_ref().and_then(|p| p.path()),
            "fetch asked for"
        );
        let backend = self.backend.clone();
        // The callback returns at once: Windows runs few at a time.
        std::thread::spawn(move || fill(&*backend, &rel, content, total, ticket));
        Ok(())
    }

    fn fetch_placeholders(
        &self,
        _request: Request,
        ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> CResult<()> {
        // Every placeholder is made up front.
        ticket
            .pass_with_placeholder(&mut [])
            .map_err(|_| CloudErrorKind::InvalidRequest)
    }

    fn dehydrate(
        &self,
        request: Request,
        ticket: ticket::Dehydrate,
        _info: info::Dehydrate,
    ) -> CResult<()> {
        if self.rel(&request.path()).is_none() {
            return Err(CloudErrorKind::DehydrationDisallowed);
        }
        ticket.pass().map_err(|_| CloudErrorKind::InvalidRequest)
    }

    fn dehydrated(&self, request: Request, _info: info::Dehydrated) {
        // Freed: the scan drops the file's record and records its placeholder.
        if let Some(rel) = self.rel(&request.path()) {
            self.later(move |backend| backend.scan_path(&rel));
        }
    }

    fn delete(
        &self,
        _request: Request,
        ticket: ticket::Delete,
        _info: info::Delete,
    ) -> CResult<()> {
        ticket.pass().map_err(|_| CloudErrorKind::InvalidRequest)
    }

    fn deleted(&self, request: Request, _info: info::Deleted) {
        let Some(rel) = self.rel(&request.path()) else {
            return;
        };
        self.later(move |backend| match backend.delete(&rel) {
            Ok(()) => debug!(path = %rel, "deleted"),
            // A folder, or a file the engine does not have: a scan finds what went.
            Err(BackendError::NotFound) => backend.scan(),
            Err(e) => warn!(path = %rel, error = %e, "cannot record a delete"),
        });
    }

    fn rename(
        &self,
        _request: Request,
        ticket: ticket::Rename,
        _info: info::Rename,
    ) -> CResult<()> {
        ticket.pass().map_err(|_| CloudErrorKind::InvalidRequest)
    }

    fn renamed(&self, request: Request, info: info::Renamed) {
        let (from, to) = (self.rel(&info.source_path()), self.rel(&request.path()));
        self.later(move |backend| match (from, to) {
            (Some(from), Some(to)) => match backend.rename(&from, &to) {
                Ok(()) => debug!(from, to, "renamed"),
                Err(BackendError::NotFound) => backend.scan(),
                Err(e) => {
                    warn!(from, to, error = %e, "cannot record a rename");
                    backend.scan();
                }
            },
            // Moved into or out of the folder: a delete or a new file, which a scan finds.
            _ => backend.scan(),
        });
    }

    fn state_changed(&self, changes: Vec<PathBuf>) {
        let changes: Vec<(PathBuf, String)> = changes
            .into_iter()
            .filter_map(|abs| self.rel(&abs).map(|rel| (abs, rel)))
            .collect();
        if changes.is_empty() {
            return;
        }
        self.later(move |backend| changed_state(backend, changes));
    }
}

/// Pins and frees made in Explorer, for the files whose state changed.
fn changed_state(backend: &dyn Backend, changes: Vec<(PathBuf, String)>) {
    let Ok(files) = backend.files() else {
        return;
    };
    for (abs, rel) in changes {
        let Some(file) = files.iter().find(|f| f.path == rel) else {
            continue;
        };
        // Pinned or unpinned in Explorer.
        if let Some((_, pin)) = ops::identity(&abs) {
            let pinned = pin == PinState::Pinned;
            if pinned != file.pinned {
                if let Err(e) = backend.set_pinned(&rel, pinned) {
                    debug!(path = %rel, error = %e, "cannot change the pin");
                }
            }
        }
        // Freed in Explorer.
        if file.on_disk && std::fs::symlink_metadata(&abs).is_ok_and(|m| ops::dehydrated(&m)) {
            backend.scan_path(&rel);
        }
    }
}
