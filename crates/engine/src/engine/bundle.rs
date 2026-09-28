//! The diagnostics bundle (ADR 0030): one zip file describing this device, redacted by default,
//! for the user to look at and attach to a report themselves.

use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};
use tidefell_net::PathKind;

use super::Engine;
use crate::diagnostics;
use crate::redact::Redactor;
use crate::views::{CheckLevel, FolderState, Operation, ProblemKind};

/// Log bytes taken from each log file, from its end.
const LOG_BYTES: usize = 5 * 1024 * 1024;

/// What goes into a bundle besides what the engine knows.
#[derive(Debug, Clone, Default)]
pub struct BundleOptions {
    /// Keep real paths and folder names instead of hashes. IDs are shortened and addresses
    /// reduced to their kind either way.
    pub include_paths: bool,
    /// About the app the engine runs in, such as the Android API level and device model, or
    /// the app's settings where there is no configuration file.
    pub app: Option<Value>,
}

/// One file of a bundle, already redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleEntry {
    /// Its name in the zip file.
    pub name: String,
    /// Its content.
    pub text: String,
}

fn level(level: CheckLevel) -> &'static str {
    match level {
        CheckLevel::Pass => "pass",
        CheckLevel::Warning => "warning",
        CheckLevel::Failure => "failure",
    }
}

fn state(state: FolderState) -> &'static str {
    match state {
        FolderState::Idle => "idle",
        FolderState::Scanning => "scanning",
        FolderState::Syncing => "syncing",
        FolderState::Paused => "paused",
        FolderState::Error => "error",
    }
}

/// The last `max` bytes of a file, from a line start.
fn tail(path: &Path, max: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(max);
    let text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    Some(match (start, text.find('\n')) {
        (0, _) | (_, None) => text,
        (_, Some(newline)) => text[newline + 1..].to_owned(),
    })
}

impl Engine {
    /// Keeps the configuration the engine was started with, for diagnostics bundles. The
    /// daemon passes it as it read it; the bundle redacts it with everything else.
    pub fn set_diagnostics_config(&self, config: Value) {
        *self.inner.diagnostics_config.lock().unwrap() = Some(config);
    }

    /// Builds a diagnostics bundle's files (ADR 0030): a summary, the configuration, status,
    /// problems, recent logs, crash files and the doctor's checks. Never file contents, keys,
    /// share-link tokens, account passwords or the engine's state files: only the files listed
    /// here go in. It reads the disks, so call it off the async runtime. Fails rather than
    /// redact less than it should.
    pub fn diagnostics(&self, options: &BundleOptions) -> std::io::Result<Vec<BundleEntry>> {
        let data_dir = self.data_dir().to_path_buf();
        let folders = self.folders();
        // Every name the bundle might mention, replaced wherever it appears.
        let mut known: Vec<String> = vec![data_dir.display().to_string()];
        if let Some(home) = std::env::var_os("HOME") {
            known.push(home.to_string_lossy().into_owned());
        }
        for folder in &folders {
            known.push(folder.path.display().to_string());
            for file in self.files(folder.id).unwrap_or_default() {
                known.push(file.path);
            }
            for item in self.recycle_bin(folder.id).unwrap_or_default() {
                known.push(item.path);
            }
        }
        // Paths in the configuration, such as mount points, are names too.
        let config = self.inner.diagnostics_config.lock().unwrap().clone();
        if let Some(config) = &config {
            config_paths(config, &mut known);
        }
        let redactor = Redactor::new(options.include_paths, known)?;
        let mut entries = Vec::new();
        let mut add_json = |name: &str, mut value: Value| {
            redactor.json(&mut value);
            entries.push(BundleEntry {
                name: name.to_owned(),
                text: serde_json::to_string_pretty(&value).unwrap_or_default(),
            });
        };

        let log_file = data_dir.join("logs").join("tidefell.log");
        add_json(
            "summary.json",
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                "device": self.id().to_string(),
                "app": options.app,
                "paths_included": options.include_paths,
                "log_file": log_file.exists(),
                "note": "Paths and names are hashed unless paths_included; IDs are shortened and \
                         addresses reduced to their kind. Without a log file, the logs are in \
                         the system journal; recent-problems.json has the latest warnings.",
            }),
        );
        add_json("config.json", config.unwrap_or(Value::Null));

        let t = self.transfer();
        let peers: Vec<Value> = self
            .peer_states()
            .into_iter()
            .map(|p| {
                json!({
                    "device": p.device.to_string(),
                    "name": p.name,
                    "connected": p.connected,
                    "path": p.path.map(|path| match path {
                        PathKind::Direct => "direct",
                        PathKind::Relay => "relay",
                    }),
                    "rtt_ms": p.rtt_ms,
                    "connected_since_ms": p.connected_since_ms,
                    "last_seen_ms": p.last_seen_ms,
                    "last_error": p.last_error,
                    "clock_offset_ms": p.clock_offset_ms,
                })
            })
            .collect();
        let folder_views: Vec<Value> = folders
            .iter()
            .map(|f| {
                let files = self.files(f.id).unwrap_or_default();
                json!({
                    "id": f.id.to_string(),
                    "path": f.path.display().to_string(),
                    "mode": format!("{:?}", f.mode),
                    "sync": format!("{:?}", f.sync),
                    "retention_days": f.retention.days(),
                    "files": files.len(),
                    "bytes": files.iter().map(|f| f.size).sum::<u64>(),
                    "recycle_bin": { "items": f.recycle_bin.items, "bytes": f.recycle_bin.bytes },
                    "state": state(f.state),
                    "error": f.error,
                    "pending_files": f.pending_files,
                    "pending_bytes": f.pending_bytes,
                    "last_scan_ms": f.last_scan_ms,
                    "last_scan_duration_ms": f.last_scan_duration_ms,
                    "paused": f.paused,
                    "conflicts": f.conflicts,
                    "problems": f.problems,
                    "watch_error": f.watch_error,
                })
            })
            .collect();
        add_json(
            "status.json",
            json!({
                "paused": self.paused(),
                "transfer": {
                    "sent": t.sent, "received": t.received,
                    "send_rate": t.send_rate, "receive_rate": t.receive_rate,
                },
                "peers": peers,
                "folders": folder_views,
            }),
        );

        let problems: Vec<Value> = folders
            .iter()
            .flat_map(|f| {
                self.path_problems(f.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |p| {
                        json!({
                            "folder": f.id.to_string(),
                            "path": p.path,
                            "operation": match p.operation {
                                Operation::Scan => "scan",
                                Operation::Fetch => "fetch",
                                Operation::Write => "write",
                                Operation::Delete => "delete",
                            },
                            "kind": match p.kind {
                                ProblemKind::PermissionDenied => "permission_denied",
                                ProblemKind::DiskFull => "disk_full",
                                ProblemKind::ContentUnavailable => "content_unavailable",
                                ProblemKind::NameNotAllowed => "name_not_allowed",
                                ProblemKind::ChangedWhileWriting => "changed_while_writing",
                                ProblemKind::Other => "other",
                            },
                            "message": p.message,
                            "since_ms": p.since_ms,
                            "attempts": p.attempts,
                        })
                    })
            })
            .collect();
        add_json("problems.json", Value::Array(problems));
        let recent: Vec<Value> = diagnostics::recent_problems()
            .into_iter()
            .map(|p| {
                json!({
                    "time_ms": p.time_ms, "level": p.level, "target": p.target,
                    "message": p.message, "fields": p.fields,
                })
            })
            .collect();
        add_json("recent-problems.json", Value::Array(recent));
        let checks: Vec<Value> = self
            .doctor()
            .into_iter()
            .map(|c| {
                json!({
                    "name": c.name, "level": level(c.level),
                    "detail": c.detail, "advice": c.advice,
                })
            })
            .collect();
        add_json("doctor.json", Value::Array(checks));

        for name in ["tidefell.log", "tidefell.1.log"] {
            if let Some(log) = tail(&data_dir.join("logs").join(name), LOG_BYTES) {
                entries.push(BundleEntry {
                    name: format!("logs/{name}"),
                    text: redactor.log(&log),
                });
            }
        }
        for crash in diagnostics::crashes(&data_dir) {
            let Some(name) = crash.file.file_name() else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&crash.file) {
                entries.push(BundleEntry {
                    name: format!("crashes/{}", name.to_string_lossy()),
                    text: redactor.text(&text),
                });
            }
        }
        Ok(entries)
    }
}

/// The values of path fields anywhere in `value`.
fn config_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Array(items) => items.iter().for_each(|v| config_paths(v, out)),
        Value::Object(fields) => {
            for (key, v) in fields {
                match v {
                    Value::String(s) if crate::redact::PATH_FIELDS.contains(&key.as_str()) => {
                        out.push(s.clone());
                    }
                    _ => config_paths(v, out),
                }
            }
        }
        _ => {}
    }
}

/// The bundle as a zip file.
pub fn zip_bundle(entries: &[BundleEntry]) -> std::io::Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut out);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for entry in entries {
            zip.start_file(&entry.name, options)
                .map_err(std::io::Error::other)?;
            zip.write_all(entry.text.as_bytes())?;
        }
        zip.finish().map_err(std::io::Error::other)?;
    }
    Ok(out.into_inner())
}
