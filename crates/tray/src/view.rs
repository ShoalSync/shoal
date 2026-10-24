//! What the tray shows: the daemon's state, as its status and folders say, in words.

use std::path::PathBuf;

use shoal_sdk::types::{Folder, FolderState, Status};

/// Where the daemon is, as the supervisor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Daemon {
    /// Not set up on this device yet: no configuration.
    NotSetUp,
    /// Started, not answering yet.
    Starting,
    /// Answering.
    Running,
    /// Stopping, as asked.
    Stopping,
    /// Stopped after failing to start or run too often, with the reason.
    Failed(String),
}

/// A folder in the tray's menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderItem {
    /// Its ID.
    pub id: String,
    /// Its name and state, as the menu shows it.
    pub label: String,
    /// Its name.
    pub name: String,
    /// Where it is, to open in Explorer.
    pub path: PathBuf,
    /// Whether it is an encrypted folder this device is a member device of, with storage
    /// peers: its Storage window is offered.
    pub storage: bool,
}

/// What the tray shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// One line: the icon's tooltip, and the menu's first, disabled, item.
    pub summary: String,
    /// The folders, in the daemon's order.
    pub folders: Vec<FolderItem>,
    /// Whether syncing is paused on the whole device, so the menu offers to resume.
    pub paused: bool,
    /// Whether the daemon answers, so pausing and the folders are offered.
    pub running: bool,
    /// Whether it stopped after failing too often, so starting it again is offered.
    pub failed: bool,
    /// Whether this device is not set up yet, so setting it up is offered.
    pub setup: bool,
}

/// The tray's view of `daemon`, with its `status` and `folders` once it answers.
pub fn view(daemon: &Daemon, status: Option<&Status>, folders: &[Folder]) -> View {
    let summary = match (daemon, status) {
        (Daemon::NotSetUp, _) => "Shoal is not set up on this device".to_owned(),
        (Daemon::Starting, _) | (Daemon::Running, None) => "Shoal is starting".to_owned(),
        (Daemon::Stopping, _) => "Shoal is stopping".to_owned(),
        (Daemon::Failed(reason), _) => format!("Shoal stopped: {reason}"),
        (Daemon::Running, Some(status)) => summary(status, folders),
    };
    let running = matches!(daemon, Daemon::Running) && status.is_some();
    View {
        summary,
        folders: if running {
            folders.iter().map(item).collect()
        } else {
            Vec::new()
        },
        paused: status.is_some_and(|s| s.paused),
        running,
        failed: matches!(daemon, Daemon::Failed(_)),
        setup: matches!(daemon, Daemon::NotSetUp),
    }
}

fn summary(status: &Status, folders: &[Folder]) -> String {
    if status.revoked {
        return "This device was removed from your devices".to_owned();
    }
    if status.paused {
        return "Syncing is paused".to_owned();
    }
    let errors = folders
        .iter()
        .filter(|f| f.state == FolderState::Error)
        .count();
    if errors > 0 {
        return format!(
            "{} need{} attention",
            plural(errors, "folder"),
            verb_s(errors)
        );
    }
    let pending: u64 = folders.iter().map(|f| f.pending_files).sum();
    let peers = status.peers.iter().filter(|p| p.connected).count();
    if pending > 0 {
        let what = format!("{} to sync", plural(pending as usize, "file"));
        return if peers == 0 {
            format!("{what}, waiting for your other devices")
        } else {
            what
        };
    }
    match peers {
        0 => "Up to date here; none of your other devices is connected".to_owned(),
        n => format!("Up to date with {}", plural(n, "device")),
    }
}

fn item(folder: &Folder) -> FolderItem {
    let path = PathBuf::from(&folder.path);
    let name = path
        .file_name()
        .map_or_else(|| folder.path.clone(), |n| n.to_string_lossy().into_owned());
    let state = match folder.state {
        _ if folder.unavailable.is_some() => Some("not available".to_owned()),
        FolderState::Error => Some("needs attention".to_owned()),
        FolderState::Paused => Some("paused".to_owned()),
        FolderState::Rebuilding => Some("rebuilding".to_owned()),
        FolderState::Syncing | FolderState::Scanning if folder.pending_files > 0 => Some(format!(
            "{} to sync",
            plural(folder.pending_files as usize, "file")
        )),
        _ => None,
    };
    let storage = folder
        .encryption
        .as_ref()
        .is_some_and(|e| e.stored.is_none() && !e.storage_peers.is_empty());
    FolderItem {
        id: folder.id.clone(),
        label: match state {
            Some(state) => format!("{name} ({state})"),
            None => name.clone(),
        },
        name,
        path,
        storage,
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn verb_s(n: usize) -> &'static str {
    if n == 1 {
        "s"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(json: serde_json::Value) -> Status {
        let mut base = serde_json::json!({
            "device": "d", "user": "u", "name": "laptop", "version": "0", "local_discovery": true,
            "paused": false, "peers": [], "transfer": {"sent": 0, "received": 0, "send_rate": 0,
            "receive_rate": 0}, "metadata_dir": "m", "revoked": false,
        });
        for (k, v) in json.as_object().unwrap() {
            base[k] = v.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn folder(path: &str, json: serde_json::Value) -> Folder {
        let mut base = serde_json::json!({
            "id": "f", "path": path, "mode": "full", "members": [], "sync": {},
            "recycle_bin": {"items": 0, "bytes": 0}, "configured": true, "only_here": 0,
            "paused": false, "pinned_bytes": 0, "conflicts": 0, "state": "idle",
            "pending_files": 0, "pending_bytes": 0, "peers": [], "problems": 0,
            "held_deletes": 0,
        });
        for (k, v) in json.as_object().unwrap() {
            base[k] = v.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn connected() -> serde_json::Value {
        serde_json::json!({"peers": [{"device": "p", "connected": true}]})
    }

    #[test]
    fn says_what_the_daemon_is_doing() {
        let idle = [folder("C:/Users/me/Photos", serde_json::json!({}))];
        let up = view(&Daemon::Running, Some(&status(connected())), &idle);
        assert_eq!(up.summary, "Up to date with 1 device");
        assert!(up.running && !up.paused);
        assert_eq!(up.folders[0].label, "Photos");
        assert_eq!(up.folders[0].path, PathBuf::from("C:/Users/me/Photos"));
        assert!(!up.folders[0].storage);

        let alone = view(
            &Daemon::Running,
            Some(&status(serde_json::json!({}))),
            &idle,
        );
        assert_eq!(
            alone.summary,
            "Up to date here; none of your other devices is connected"
        );

        let busy = [folder(
            "C:/Users/me/Docs",
            serde_json::json!({"state": "syncing", "pending_files": 3}),
        )];
        let syncing = view(&Daemon::Running, Some(&status(connected())), &busy);
        assert_eq!(syncing.summary, "3 files to sync");
        assert_eq!(syncing.folders[0].label, "Docs (3 files to sync)");

        let broken = [folder("C:/x/Docs", serde_json::json!({"state": "error"}))];
        let attention = view(&Daemon::Running, Some(&status(connected())), &broken);
        assert_eq!(attention.summary, "1 folder needs attention");

        let sealed = [
            folder(
                "C:/x/Vault",
                serde_json::json!({"encryption": {"storage_peers": ["s"]}}),
            ),
            folder("C:/x/Alone", serde_json::json!({"encryption": {}})),
            folder(
                "C:/x/Held",
                serde_json::json!({"encryption": {"storage_peers": ["s"], "stored": {
                    "versions": 0, "superseded": 0, "bytes": 0, "quota_bytes": 0,
                    "missing_chunks": 0}}}),
            ),
        ];
        let shown = view(&Daemon::Running, Some(&status(connected())), &sealed);
        let storage: Vec<bool> = shown.folders.iter().map(|f| f.storage).collect();
        assert_eq!(
            storage,
            [true, false, false],
            "only members, with storage peers"
        );

        let paused = view(
            &Daemon::Running,
            Some(&status(serde_json::json!({"paused": true}))),
            &idle,
        );
        assert_eq!(paused.summary, "Syncing is paused");
        assert!(paused.paused);
    }

    #[test]
    fn offers_nothing_while_the_daemon_does_not_answer() {
        for (daemon, summary) in [
            (Daemon::NotSetUp, "Shoal is not set up on this device"),
            (Daemon::Starting, "Shoal is starting"),
            (Daemon::Running, "Shoal is starting"),
            (Daemon::Stopping, "Shoal is stopping"),
            (
                Daemon::Failed("its configuration is not valid".into()),
                "Shoal stopped: its configuration is not valid",
            ),
        ] {
            let shown = view(&daemon, None, &[]);
            assert_eq!(shown.summary, summary);
            assert!(!shown.running);
            assert!(shown.folders.is_empty());
        }
    }
}
