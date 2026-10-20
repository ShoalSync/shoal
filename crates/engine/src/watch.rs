//! Watching folders for changes (ADR 0029), so scans look only where something changed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::event::EventKind;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::ignores::Ignores;
use crate::scan::{Scope, RESERVED_DIR};

/// How long changes must settle before they are scanned.
const SETTLE: Duration = Duration::from_secs(1);
/// The longest a scan waits for changes to settle, so a file written continuously is still
/// picked up.
const MAX_WAIT: Duration = Duration::from_secs(5);
/// Past this many changed paths, one full scan is cheaper than tracking them.
const MAX_PATHS: usize = 4096;

/// What the watcher asks the scanner to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wanted {
    /// Scan everything.
    Full,
    /// Scan these parts.
    Scope(Scope),
}

/// How a folder's root is watched: on its own, with the directories below it added as scans
/// walk them, as inotify needs; on Windows, recursively in one watch, since each watch there
/// holds its directory open and would stop it being renamed or removed (ADR 0042).
#[cfg(not(windows))]
const ROOT_WATCH: RecursiveMode = RecursiveMode::NonRecursive;
#[cfg(windows)]
const ROOT_WATCH: RecursiveMode = RecursiveMode::Recursive;

/// One folder's watcher: a non-recursive watch on every directory the scans walked, or, on
/// Windows, one recursive watch on the root.
pub(crate) struct FolderWatch {
    root: PathBuf,
    /// The device the root is on, to find directories inside it that are other mounts.
    root_device: Option<u64>,
    watcher: RecommendedWatcher,
    watched: BTreeSet<PathBuf>,
    events: mpsc::UnboundedReceiver<notify::Result<Event>>,
}

impl FolderWatch {
    /// Starts watching `root`, or says why it cannot be watched.
    pub(crate) fn new(root: &Path) -> Result<Self, String> {
        if let Some(kind) = network_filesystem(root) {
            return Err(format!(
                "{kind} filesystem: changes from other machines are not reported"
            ));
        }
        let (tx, events) = mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|e| e.to_string())?;
        let mut watch = Self {
            root: root.to_owned(),
            root_device: device(root),
            watcher,
            watched: BTreeSet::new(),
            events,
        };
        watch
            .watcher
            .watch(root, ROOT_WATCH)
            .map_err(|e| format!("cannot watch {}: {e}", root.display()))?;
        watch.watched.insert(root.to_owned());
        Ok(watch)
    }

    /// Watches directories a scan walked, and forgets those that are gone. Fails if a watch
    /// cannot be added, for example at the system's limit.
    pub(crate) fn add(&mut self, dirs: &[PathBuf]) -> Result<(), String> {
        if ROOT_WATCH == RecursiveMode::Recursive {
            // The root's watch covers them.
            let _ = dirs;
            return Ok(());
        }
        let gone: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|d| !d.is_dir())
            .cloned()
            .collect();
        for dir in gone {
            let _ = self.watcher.unwatch(&dir);
            self.watched.remove(&dir);
        }
        for dir in dirs {
            if self.watched.contains(dir) {
                continue;
            }
            if let Some(kind) = nested_mount(self.root_device, dir, network_filesystem) {
                let rel = dir.strip_prefix(&self.root).unwrap_or(dir);
                return Err(format!(
                    "{kind} mount at {}: changes from other machines are not reported",
                    rel.display()
                ));
            }
            match self.watcher.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(dir.clone());
                }
                // Removed since it was walked; its parent's watch reports that.
                Err(_) if !dir.is_dir() => {}
                Err(e) => return Err(format!("cannot watch {}: {e}", dir.display())),
            }
        }
        Ok(())
    }

    /// Waits for changes, lets them settle, and returns what to scan. `None` once the watch has
    /// stopped.
    pub(crate) async fn next(&mut self, ignores: &Ignores) -> Option<Wanted> {
        let mut paths = BTreeSet::new();
        let mut full = false;
        let first = self.events.recv().await?;
        let deadline = Instant::now() + MAX_WAIT;
        full |= self.collect(first, ignores, &mut paths);
        loop {
            let settle = (Instant::now() + SETTLE).min(deadline);
            match tokio::time::timeout_at(settle, self.events.recv()).await {
                Ok(Some(event)) => full |= self.collect(event, ignores, &mut paths),
                Ok(None) => return None,
                Err(_) => break,
            }
        }
        if full || paths.len() > MAX_PATHS {
            return Some(Wanted::Full);
        }
        if paths.is_empty() {
            return Some(Wanted::Scope(Scope::default()));
        }
        let mut scope = Scope::default();
        for rel in &paths {
            let abs = self.root.join(rel);
            if abs.is_dir() {
                scope.add(rel, true);
            } else {
                scope.add(rel.rsplit_once('/').map_or("", |(dir, _)| dir), false);
                if !abs.exists() {
                    // A directory removed or moved away: everything recorded under it is gone.
                    scope.add(rel, true);
                }
            }
        }
        Some(Wanted::Scope(scope))
    }

    /// Adds an event's paths to `paths`. Returns true if it needs a full scan.
    fn collect(
        &self,
        event: notify::Result<Event>,
        ignores: &Ignores,
        paths: &mut BTreeSet<String>,
    ) -> bool {
        let Ok(event) = event else {
            // An error from the watch, such as a lost event queue.
            return true;
        };
        if event.need_rescan() {
            return true;
        }
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        for path in &event.paths {
            let Some(rel) = path
                .strip_prefix(&self.root)
                .ok()
                .and_then(Path::to_str)
                .map(|s| s.replace(std::path::MAIN_SEPARATOR, "/"))
            else {
                return true;
            };
            if rel.is_empty() {
                return true;
            }
            if rel == RESERVED_DIR || rel.starts_with(&format!("{RESERVED_DIR}/")) {
                continue;
            }
            // Changed patterns change what every scan looks at.
            if ignores.is_pattern_file(&rel) {
                return true;
            }
            if ignores.is_ignored(&rel, path.is_dir()) {
                continue;
            }
            paths.insert(rel);
        }
        false
    }
}

/// The kind of network filesystem mounted at `dir`, if `dir` is on another device than the
/// folder's root (`root_device`) and `classify` says it is one.
fn nested_mount(
    root_device: Option<u64>,
    dir: &Path,
    classify: impl Fn(&Path) -> Option<&'static str>,
) -> Option<&'static str> {
    let root_device = root_device?;
    if device(dir)? == root_device {
        return None;
    }
    classify(dir)
}

/// The ID of the device `path` is on.
fn device(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The kind of network or user-space filesystem `path` is on, whose watches do not report
/// changes made by other clients.
fn network_filesystem(path: &Path) -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        let magic = rustix::fs::statfs(path).ok()?.f_type as u64;
        match magic {
            0x6969 => Some("NFS"),
            0x517b => Some("SMB"),
            0xff53_4d42 => Some("CIFS"),
            0xfe53_4d42 => Some("SMB2"),
            0x6573_5546 => Some("FUSE"),
            0x0102_1997 => Some("9P"),
            0x00c3_6400 => Some("Ceph"),
            0x0bd0_0bd0 => Some("Lustre"),
            0x1373 => Some("device"),
            _ => None,
        }
    }
    // A network drive, mapped to a letter or not, resolves to a UNC path (ADR 0042).
    #[cfg(windows)]
    {
        std::fs::canonicalize(path)
            .ok()
            .filter(|p| is_unc(p))
            .map(|_| "network drive")
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = path;
        None
    }
}

/// Whether `path` is on a network share: `\\server\share` or `\\?\UNC\server\share`.
#[cfg(windows)]
fn is_unc(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    matches!(
        path.components().next(),
        Some(Component::Prefix(p))
            if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_that_cannot_be_watched_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let error = FolderWatch::new(&dir.path().join("missing")).err().unwrap();
        assert!(error.contains("cannot watch"), "{error}");
        assert!(FolderWatch::new(dir.path()).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_network_filesystem_mounted_inside_the_folder_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = device(dir.path());
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let nfs = |_: &Path| Some("NFS");
        // On the root's device: not a mount, whatever the filesystem.
        assert_eq!(nested_mount(root, &sub, nfs), None);
        // On another device (procfs stands in for a mount): classified.
        assert_eq!(nested_mount(root, Path::new("/proc"), nfs), Some("NFS"));
        assert_eq!(nested_mount(root, Path::new("/proc"), |_| None), None);
    }

    #[cfg(windows)]
    #[test]
    fn shares_are_network_drives_and_drive_letters_are_not() {
        assert!(is_unc(Path::new(r"\\server\share\folder")));
        assert!(is_unc(Path::new(r"\\?\UNC\server\share\folder")));
        assert!(!is_unc(Path::new(r"\\?\C:\Users\ci\folder")));
        assert!(!is_unc(Path::new(r"C:\Users\ci\folder")));
        assert!(!is_unc(Path::new(r"\\.\pipe\tidefell")));
    }

    #[test]
    fn local_disks_are_not_network_filesystems() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(network_filesystem(dir.path()), None);
    }
}
