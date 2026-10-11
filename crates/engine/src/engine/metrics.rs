//! Prometheus metrics (ADR 0030): what `tidefelld` serves at `/metrics` when `[metrics]` is on,
//! in the OpenMetrics text format. Counters are kept as things happen; everything else is read
//! from the engine when the page is built.

use std::fmt::{Display, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};

use tidefell_net::PathKind;

use crate::Engine;

/// Why fetching content failed, for `tidefell_fetch_failures_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchFailure {
    /// A request to a peer failed: the connection was lost, it timed out or it was refused.
    Request,
    /// A peer at its upload limit turned a request away, to be asked again later.
    Busy,
    /// A peer did not have a chunk it was asked for.
    Missing,
    /// No connected peer had the content: the file waits.
    Unavailable,
    /// A chunk received could not be stored.
    Storage,
}

impl FetchFailure {
    const ALL: [FetchFailure; 5] = [
        FetchFailure::Request,
        FetchFailure::Busy,
        FetchFailure::Missing,
        FetchFailure::Unavailable,
        FetchFailure::Storage,
    ];

    fn label(self) -> &'static str {
        match self {
            FetchFailure::Request => "request",
            FetchFailure::Busy => "busy",
            FetchFailure::Missing => "missing",
            FetchFailure::Unavailable => "unavailable",
            FetchFailure::Storage => "storage",
        }
    }
}

/// Counters the engine keeps as things happen.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    fetch_failures: [AtomicU64; FetchFailure::ALL.len()],
}

impl Counters {
    pub(crate) fn fetch_failed(&self, reason: FetchFailure) {
        self.fetch_failures[reason as usize].fetch_add(1, Ordering::Relaxed);
    }

    fn fetch_failures(&self, reason: FetchFailure) -> u64 {
        self.fetch_failures[reason as usize].load(Ordering::Relaxed)
    }
}

/// An OpenMetrics page being written. Each family's samples follow its `# TYPE` line.
struct Page(String);

impl Page {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}");
        let _ = writeln!(self.0, "# TYPE {name} {kind}");
    }

    /// A sample of family `name`; counters take the `_total` suffix.
    fn sample(&mut self, name: &str, suffix: &str, labels: &[(&str, &str)], value: impl Display) {
        self.0.push_str(name);
        self.0.push_str(suffix);
        if !labels.is_empty() {
            let labels: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
            let _ = write!(self.0, "{{{}}}", labels.join(","));
        }
        let _ = writeln!(self.0, " {value}");
    }

    fn counter(&mut self, name: &str, help: &str, samples: &[(&[(&str, &str)], u64)]) {
        self.family(name, "counter", help);
        for (labels, value) in samples {
            self.sample(name, "_total", labels, value);
        }
    }

    fn gauge<T: Display>(&mut self, name: &str, help: &str, samples: &[(&[(&str, &str)], T)]) {
        self.family(name, "gauge", help);
        for (labels, value) in samples {
            self.sample(name, "", labels, value);
        }
    }
}

impl Engine {
    /// This device's metrics in the OpenMetrics text format, ending with `# EOF` (ADR 0030).
    /// Folders are labelled by the first 8 characters of their ID. It reads every folder's
    /// state, so call it off the async runtime.
    pub fn metrics(&self) -> String {
        let mut page = Page(String::new());
        let traffic = self.node.traffic_counts();
        let local: &[(&str, &str)] = &[("network", "local")];
        let remote: &[(&str, &str)] = &[("network", "remote")];
        page.counter(
            "tidefell_content_sent_bytes",
            "File content sent to peers, on the local network or not.",
            &[(local, traffic.sent_local), (remote, traffic.sent_remote)],
        );
        page.counter(
            "tidefell_content_received_bytes",
            "File content received from peers, on the local network or not.",
            &[
                (local, traffic.received_local),
                (remote, traffic.received_remote),
            ],
        );
        page.counter(
            "tidefell_chunks_served",
            "Chunks of file content sent to peers.",
            &[(&[], traffic.chunks_served)],
        );
        page.counter(
            "tidefell_requests",
            "Requests sent to peers, by kind; each opens a stream.",
            &[
                (&[("kind", "manifest")], traffic.manifest_requests),
                (&[("kind", "availability")], traffic.availability_requests),
                (&[("kind", "replica")], traffic.replica_requests),
                (&[("kind", "chunks")], traffic.chunk_requests),
            ],
        );
        page.counter(
            "tidefell_chunks_requested",
            "Chunks of file content asked for in requests to peers.",
            &[(&[], traffic.chunks_requested)],
        );
        let reasons: Vec<[(&str, &str); 1]> = FetchFailure::ALL
            .iter()
            .map(|r| [("reason", r.label())])
            .collect();
        let failures: Vec<(&[(&str, &str)], u64)> = FetchFailure::ALL
            .iter()
            .zip(&reasons)
            .map(|(r, labels)| (&labels[..], self.inner.counters.fetch_failures(*r)))
            .collect();
        page.counter(
            "tidefell_fetch_failures",
            "Failures fetching file content, by reason.",
            &failures,
        );
        let connected: Vec<Option<PathKind>> = self
            .peer_states()
            .into_iter()
            .filter(|p| p.connected)
            .map(|p| p.path)
            .collect();
        let count = |kind| connected.iter().filter(|p| **p == Some(kind)).count();
        page.gauge(
            "tidefell_connections",
            "Peers connected now, directly or through a relay.",
            &[
                (&[("path", "direct")], count(PathKind::Direct)),
                (&[("path", "relay")], count(PathKind::Relay)),
            ],
        );

        let folders = self.folders();
        let labels: Vec<String> = folders
            .iter()
            .map(|f| f.id.to_string().chars().take(8).collect())
            .collect();
        let labels: Vec<[(&str, &str); 1]> = labels.iter().map(|l| [("folder", &l[..])]).collect();
        let per_folder = |value: &dyn Fn(&crate::FolderInfo) -> Option<String>| {
            folders
                .iter()
                .zip(&labels)
                .filter_map(|(f, l)| Some((&l[..], value(f)?)))
                .collect::<Vec<_>>()
        };
        page.gauge(
            "tidefell_folder_pending_files",
            "Files the folder is to write and does not have yet.",
            &per_folder(&|f| Some(f.pending_files.to_string())),
        );
        page.gauge(
            "tidefell_folder_pending_bytes",
            "The size of the files the folder is to write and does not have yet.",
            &per_folder(&|f| Some(f.pending_bytes.to_string())),
        );
        page.gauge(
            "tidefell_folder_scan_duration_seconds",
            "How long the folder's last scan took.",
            &per_folder(&|f| {
                f.last_scan_duration_ms
                    .map(|ms| (ms as f64 / 1000.0).to_string())
            }),
        );
        page.gauge(
            "tidefell_folder_recycle_bin_items",
            "Deleted files in the folder's recycle bin.",
            &per_folder(&|f| Some(f.recycle_bin.items.to_string())),
        );
        page.gauge(
            "tidefell_folder_recycle_bin_bytes",
            "The size of the deleted files in the folder's recycle bin.",
            &per_folder(&|f| Some(f.recycle_bin.bytes.to_string())),
        );
        page.gauge(
            "tidefell_folder_problems",
            "Paths in the folder that failed to scan, fetch, write or delete.",
            &per_folder(&|f| Some(f.problems.to_string())),
        );

        if let Err(e) = self.node.encode_iroh_metrics(&mut page.0) {
            tracing::warn!(error = %e, "encoding iroh's metrics failed");
        }
        page.0.push_str("# EOF\n");
        page.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_and_samples() {
        let mut page = Page(String::new());
        page.counter("t_bytes", "Bytes.", &[(&[("network", "local")], 3)]);
        page.gauge("t_files", "Files.", &[(&[], 2)]);
        assert_eq!(
            page.0,
            "# HELP t_bytes Bytes.\n# TYPE t_bytes counter\nt_bytes_total{network=\"local\"} 3\n\
             # HELP t_files Files.\n# TYPE t_files gauge\nt_files 2\n"
        );
        let counters = Counters::default();
        counters.fetch_failed(FetchFailure::Busy);
        assert_eq!(counters.fetch_failures(FetchFailure::Busy), 1);
        assert_eq!(counters.fetch_failures(FetchFailure::Storage), 0);
    }
}
