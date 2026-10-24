//! Partial folders as Windows sync roots (ADR 0042).
//!
//! One task owns every root: it connects each partial folder as it appears, makes a root's
//! placeholders match its folder after the engine's changes, as a FUSE mount is refreshed, and
//! disconnects each one, waiting, before the engine stops. A root dropped instead would
//! disconnect on a thread of its own, after the engine it calls into had gone.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tidefell_engine::{Engine, Event, ReplicaMode};
use tidefell_fs_cfapi::{EngineFolder, SyncRoot};
use tidefell_proto::FolderId;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

/// How long changes are gathered before the roots are told.
const REFRESH_DELAY: Duration = Duration::from_millis(200);

/// How often the folders are looked at again without an event: a folder's root back after its
/// drive was away, or a root Windows refused before.
const RECHECK: Duration = Duration::from_secs(60);

/// The daemon's sync roots, kept by a task of their own.
pub(crate) struct SyncRoots {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl SyncRoots {
    /// Connects every partial folder of `engine`'s, now and as they are added.
    pub(crate) fn start(engine: &Arc<Engine>) -> Self {
        // Before the folders are first listed, so none added in between is missed.
        let events = engine.events();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(engine.clone(), events, stopped));
        Self { stop, task }
    }

    /// Disconnects every root, returning once all are.
    pub(crate) async fn stop(self) {
        let _ = self.stop.send(());
        if let Err(e) = self.task.await {
            tracing::warn!(error = %e, "the sync roots' task failed");
        }
    }
}

/// What a batch of events asks for.
#[derive(Default)]
struct Batch {
    /// The folders added or removed, or events missed: which roots there are to be looked at.
    folders: bool,
    /// The folders whose files changed.
    changed: HashSet<FolderId>,
}

impl Batch {
    fn add(&mut self, event: &Event) {
        match event {
            Event::FolderAdded(_) | Event::FolderRemoved(_) => self.folders = true,
            Event::FolderChanged(folder)
            | Event::FileUpdated { folder, .. }
            | Event::FileRemoved { folder, .. } => {
                self.changed.insert(*folder);
            }
            _ => {}
        }
    }
}

async fn run(
    engine: Arc<Engine>,
    mut events: broadcast::Receiver<Event>,
    mut stopped: oneshot::Receiver<()>,
) {
    let mut roots = Roots::default();
    roots.update(&engine).await;
    let mut recheck = tokio::time::interval(RECHECK);
    recheck.tick().await;
    loop {
        let mut batch = Batch::default();
        tokio::select! {
            _ = &mut stopped => break,
            _ = recheck.tick() => batch.folders = true,
            event = events.recv() => match event {
                Ok(event) => batch.add(&event),
                Err(RecvError::Lagged(_)) => {
                    batch.folders = true;
                    batch.changed.extend(roots.connected.keys());
                }
                Err(RecvError::Closed) => break,
            },
        }
        tokio::select! {
            _ = &mut stopped => break,
            () = tokio::time::sleep(REFRESH_DELAY) => {}
        }
        loop {
            match events.try_recv() {
                Ok(event) => batch.add(&event),
                Err(TryRecvError::Lagged(_)) => {
                    batch.folders = true;
                    batch.changed.extend(roots.connected.keys());
                }
                Err(_) => break,
            }
        }
        if batch.folders {
            roots.update(&engine).await;
        }
        for folder in batch.changed {
            roots.changed(&engine, folder).await;
        }
    }
    for (folder, root) in roots.connected.drain() {
        engine.set_freer(folder, None);
        disconnect(folder, root, false).await;
    }
}

#[derive(Default)]
struct Roots {
    connected: HashMap<FolderId, SyncRoot>,
    /// Why each folder not connected could not be, so each reason is logged once.
    refused: HashMap<FolderId, String>,
}

impl Roots {
    /// Connects the partial folders not connected, and disconnects the roots of folders gone,
    /// forgetting the registration of a folder removed.
    async fn update(&mut self, engine: &Arc<Engine>) {
        let folders = engine.folders();
        let gone: Vec<FolderId> = self
            .connected
            .keys()
            .filter(|id| {
                !folders
                    .iter()
                    .any(|f| f.id == **id && f.unavailable.is_none())
            })
            .copied()
            .collect();
        for folder in gone {
            let removed = !folders.iter().any(|f| f.id == folder);
            if let Some(root) = self.connected.remove(&folder) {
                engine.set_freer(folder, None);
                disconnect(folder, root, removed).await;
            }
        }
        self.refused
            .retain(|id, _| folders.iter().any(|f| f.id == *id));
        for info in folders {
            if !matches!(info.mode, ReplicaMode::Partial { .. })
                || info.unavailable.is_some()
                || self.connected.contains_key(&info.id)
            {
                continue;
            }
            let (folder, path) = (info.id, info.path);
            let backend =
                EngineFolder::new(engine.clone(), folder, tokio::runtime::Handle::current());
            let result = tokio::task::spawn_blocking(move || {
                if let Some(problem) = tidefell_fs_cfapi::unusable(&path, folder) {
                    return Err(problem);
                }
                let name = path
                    .file_name()
                    .map_or_else(|| folder.to_string(), |n| n.to_string_lossy().into_owned());
                tidefell_fs_cfapi::connect(backend, &path, folder, &name)
                    .map_err(|e| format!("Windows refused it: {e}"))
            })
            .await
            .unwrap_or_else(|e| Err(format!("connecting failed: {e}")));
            match result {
                Ok(root) => {
                    tracing::info!(folder = %folder, root = %root.root().display(), "sync root connected");
                    self.refused.remove(&folder);
                    // Eviction dehydrates, leaving the file online-only where it is.
                    engine.set_freer(folder, Some(Arc::new(tidefell_fs_cfapi::free)));
                    self.connected.insert(folder, root);
                }
                Err(reason) => {
                    if self.refused.get(&folder) != Some(&reason) {
                        tracing::warn!(folder = %folder, reason = %reason, "the folder cannot be a cloud folder; its files not on this device are not shown");
                    }
                    self.refused.insert(folder, reason);
                }
            }
        }
    }

    /// Makes `folder`'s placeholders match it again, if it is connected, and shows its storage
    /// priorities in Explorer if it is an encrypted folder with storage peers (ADR 0023).
    async fn changed(&mut self, engine: &Arc<Engine>, folder: FolderId) {
        if !self.connected.contains_key(&folder) {
            return;
        }
        let ranked = engine.folders().into_iter().any(|f| {
            f.id == folder
                && f.encryption
                    .as_ref()
                    .is_some_and(|e| e.stored.is_none() && !e.storage_peers.is_empty())
        });
        let priorities: Option<Vec<(String, u8)>> = if ranked {
            match engine.priorities(folder).await {
                Ok(p) => Some(
                    p.entries
                        .into_iter()
                        .map(|e| (e.path, e.priority))
                        .collect(),
                ),
                Err(e) => {
                    tracing::debug!(folder = %folder, error = %e, "cannot rank the folder's files yet");
                    None
                }
            }
        } else {
            None
        };
        let Some(root) = self.connected.remove(&folder) else {
            return;
        };
        // Moved to the blocking thread and back: the root is only ever this task's.
        match tokio::task::spawn_blocking(move || {
            let result = root.changed();
            if let Some(priorities) = priorities {
                if let Err(e) = root.show_priorities(&priorities) {
                    tracing::debug!(error = %e, "cannot show the storage priorities");
                }
            }
            (root, result)
        })
        .await
        {
            Ok((root, result)) => {
                if let Err(e) = result {
                    tracing::warn!(folder = %folder, error = %e, "cannot update the placeholders");
                }
                self.connected.insert(folder, root);
            }
            Err(e) => {
                tracing::warn!(folder = %folder, error = %e, "updating the placeholders failed")
            }
        }
    }
}

/// Disconnects `root`, waiting, and forgets its registration too if its folder was `removed`.
async fn disconnect(folder: FolderId, root: SyncRoot, removed: bool) {
    let result = tokio::task::spawn_blocking(move || {
        if removed {
            root.unregister()
        } else {
            root.disconnect();
            Ok(())
        }
    })
    .await;
    match result {
        Ok(Ok(())) if removed => tracing::info!(folder = %folder, "sync root unregistered"),
        Ok(Ok(())) => tracing::info!(folder = %folder, "sync root disconnected"),
        Ok(Err(e)) => {
            tracing::warn!(folder = %folder, error = %e, "cannot unregister the sync root")
        }
        Err(e) => tracing::warn!(folder = %folder, error = %e, "disconnecting failed"),
    }
}
