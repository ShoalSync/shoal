//! What a folder is doing and what went wrong, for status displays (ADR 0030). Kept in memory
//! only: it describes this run of the engine.

use std::collections::{BTreeMap, HashMap};

use shoal_proto::{ContentHash, DeviceId};

use crate::views::{Operation, PathProblem, ProblemKind};
use crate::Error;

/// One folder's scans, exchanges with peers and problems.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    /// A scan is running.
    pub(crate) scanning: bool,
    /// When the last scan finished, in milliseconds since the Unix epoch, how long it took, and
    /// why it failed, if it did.
    pub(crate) last_scan: Option<(i64, u64, Option<String>)>,
    /// When the folder last finished exchanging changes with each peer.
    pub(crate) exchanged: HashMap<DeviceId, i64>,
    /// Paths that failed, by path and operation.
    pub(crate) problems: BTreeMap<(String, Operation), PathProblem>,
    /// Writes and deletes that failed in a way that lasts (ADR 0037), with the content written:
    /// not tried again until that content changes, a scan looks at the path, or the user asks.
    pub(crate) stuck: HashMap<(String, Operation), Option<ContentHash>>,
}

/// A failure found during one pass: a scan, or a materialise pass.
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    pub(crate) path: String,
    pub(crate) operation: Operation,
    pub(crate) kind: ProblemKind,
    pub(crate) message: String,
    /// Not tried this pass, only carried over: not another attempt.
    pub(crate) carried: bool,
    /// Trying again changes nothing until the file, its folder or the device does (ADR 0037).
    pub(crate) lasting: bool,
}

impl Failure {
    pub(crate) fn new(
        path: &str,
        operation: Operation,
        kind: ProblemKind,
        message: impl ToString,
    ) -> Self {
        Self {
            path: path.to_owned(),
            operation,
            kind,
            message: message.to_string(),
            carried: false,
            lasting: matches!(
                kind,
                ProblemKind::PermissionDenied | ProblemKind::NameNotAllowed
            ),
        }
    }

    /// A failure from an I/O error, classified by its kind.
    pub(crate) fn io(path: &str, operation: Operation, error: &std::io::Error) -> Self {
        Self {
            lasting: io_lasts(error),
            ..Self::new(path, operation, io_kind(error), error)
        }
    }

    /// A failure from an engine error.
    pub(crate) fn error(path: &str, operation: Operation, error: &Error) -> Self {
        let (kind, lasting) = match error {
            Error::Io { source, .. } => (io_kind(source), io_lasts(source)),
            _ => (ProblemKind::Other, false),
        };
        Self {
            lasting,
            ..Self::new(path, operation, kind, error)
        }
    }
}

/// True if an I/O error stays until something changes (ADR 0037): permission refused, a
/// read-only filesystem, or the device's own I/O error (EIO). Others, such as an interrupted or
/// timed-out call, or a directory vanishing meanwhile, are tried again on the next pass.
pub(crate) fn io_lasts(error: &std::io::Error) -> bool {
    use std::io::ErrorKind as K;
    const EIO: i32 = 5;
    matches!(error.kind(), K::PermissionDenied | K::ReadOnlyFilesystem)
        || (cfg!(unix) && error.raw_os_error() == Some(EIO))
}

/// What kind of problem an I/O error is.
pub(crate) fn io_kind(error: &std::io::Error) -> ProblemKind {
    use std::io::ErrorKind as K;
    match error.kind() {
        K::PermissionDenied | K::ReadOnlyFilesystem => ProblemKind::PermissionDenied,
        K::StorageFull | K::QuotaExceeded => ProblemKind::DiskFull,
        _ => ProblemKind::Other,
    }
}

impl Activity {
    /// The problem of a write (with `content`) or delete (`None`) at `path` that failed in a
    /// way that lasts and has not changed since, to carry over instead of trying it again.
    pub(crate) fn stuck(
        &self,
        path: &str,
        operation: Operation,
        content: Option<ContentHash>,
    ) -> Option<Failure> {
        let key = (path.to_owned(), operation);
        if self.stuck.get(&key) != Some(&content) {
            return None;
        }
        let problem = self.problems.get(&key)?;
        Some(Failure {
            carried: true,
            ..Failure::new(path, operation, problem.kind, &problem.message)
        })
    }

    /// Records the outcome of a write (with `content`) or delete at `path`: a failure of a
    /// lasting kind is not tried again until something changes (ADR 0037).
    pub(crate) fn tried(
        &mut self,
        path: &str,
        operation: Operation,
        content: Option<ContentHash>,
        failed: Option<&Failure>,
    ) {
        let key = (path.to_owned(), operation);
        if failed.is_some_and(|f| f.lasting) {
            self.stuck.insert(key, content);
        } else {
            self.stuck.remove(&key);
        }
    }

    /// Replaces the problems of the operations in `operations`, for the paths `covered` says a
    /// pass looked at, with that pass's `failures`. A path failing again keeps when it first
    /// failed and counts one more attempt. Returns true if the set of failing paths changed.
    pub(crate) fn settle(
        &mut self,
        operations: &[Operation],
        covered: impl Fn(&str) -> bool,
        failures: Vec<Failure>,
        now_ms: i64,
    ) -> bool {
        let before: Vec<(String, Operation)> = self.problems.keys().cloned().collect();
        let mut fresh = BTreeMap::new();
        for failure in failures {
            let key = (failure.path.clone(), failure.operation);
            let (since_ms, attempts) = self.problems.get(&key).map_or((now_ms, 1), |p| {
                (p.since_ms, p.attempts + u32::from(!failure.carried))
            });
            fresh.insert(
                key,
                PathProblem {
                    path: failure.path,
                    operation: failure.operation,
                    kind: failure.kind,
                    message: failure.message,
                    since_ms,
                    attempts,
                },
            );
        }
        self.problems
            .retain(|(path, op), _| !operations.contains(op) || !covered(path));
        self.problems.extend(fresh);
        !self.problems.keys().eq(before.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(path: &str, op: Operation) -> Failure {
        Failure::new(path, op, ProblemKind::PermissionDenied, "denied")
    }

    #[test]
    fn lasting_failures_wait_for_a_change() {
        let mut activity = Activity::default();
        let all = |_: &str| true;
        let content = Some(ContentHash([1; 32]));
        activity.settle(
            &[Operation::Write],
            all,
            vec![fail("a", Operation::Write)],
            1,
        );
        activity.tried(
            "a",
            Operation::Write,
            content,
            Some(&fail("a", Operation::Write)),
        );
        // The same content is not tried again; its problem is carried, not counted again.
        let carried = activity.stuck("a", Operation::Write, content).unwrap();
        activity.settle(&[Operation::Write], all, vec![carried], 2);
        assert_eq!(
            activity.problems[&("a".into(), Operation::Write)].attempts,
            1
        );
        // Other content is.
        assert!(activity
            .stuck("a", Operation::Write, Some(ContentHash([2; 32])))
            .is_none());
        // A transient failure, or success, ends it.
        let full = Failure::new("a", Operation::Write, ProblemKind::DiskFull, "full");
        activity.tried("a", Operation::Write, content, Some(&full));
        assert!(activity.stuck("a", Operation::Write, content).is_none());
        // Other I/O errors are transient, unless the device reports an I/O error.
        let interrupted = std::io::Error::from(std::io::ErrorKind::Interrupted);
        assert!(!Failure::io("a", Operation::Write, &interrupted).lasting);
        let eio = std::io::Error::from_raw_os_error(5);
        assert!(Failure::io("a", Operation::Write, &eio).lasting);
        let gone = Error::NoSuchFile("a".into());
        assert!(!Failure::error("a", Operation::Write, &gone).lasting);
    }

    #[test]
    fn problems_count_attempts_and_clear() {
        let mut activity = Activity::default();
        let all = |_: &str| true;
        assert!(activity.settle(&[Operation::Scan], all, vec![fail("a", Operation::Scan)], 1));
        assert!(!activity.settle(&[Operation::Scan], all, vec![fail("a", Operation::Scan)], 5));
        let problem = &activity.problems[&("a".into(), Operation::Scan)];
        assert_eq!((problem.since_ms, problem.attempts), (1, 2));

        // A write problem is left alone by scans, and a scan outside a path leaves it alone.
        activity.settle(
            &[Operation::Write],
            all,
            vec![fail("b", Operation::Write)],
            6,
        );
        assert!(!activity.settle(&[Operation::Scan], |p| p == "x", Vec::new(), 7));
        assert_eq!(activity.problems.len(), 2);
        // A scan that covers the path and finds nothing wrong clears it.
        assert!(activity.settle(&[Operation::Scan], |p| p == "a", Vec::new(), 8));
        assert_eq!(activity.problems.len(), 1);
    }

    #[test]
    fn io_errors_are_classified() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            io_kind(&Error::from(ErrorKind::PermissionDenied)),
            ProblemKind::PermissionDenied
        );
        assert_eq!(
            io_kind(&Error::from(ErrorKind::ReadOnlyFilesystem)),
            ProblemKind::PermissionDenied
        );
        assert_eq!(
            io_kind(&Error::from(ErrorKind::StorageFull)),
            ProblemKind::DiskFull
        );
        assert_eq!(
            io_kind(&Error::from(ErrorKind::NotFound)),
            ProblemKind::Other
        );
    }
}
