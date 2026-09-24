//! Folders mounted with FUSE (ADR 0017).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tidefell_engine::{Engine, Event};
use tidefell_fs_fuse::{EngineFolder, Mount};
use tidefell_proto::FolderId;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

/// How long changes are gathered before a mount is refreshed.
const REFRESH_DELAY: Duration = Duration::from_millis(200);

/// The daemon's mounts, and the task that tells them about changes.
pub(crate) struct Mounts {
    mounts: Vec<(FolderId, Arc<Mount>)>,
    watcher: Option<JoinHandle<()>>,
}

impl Mounts {
    /// Mounts each `(folder, mount point)`; `folders` gives each folder's backing directory.
    pub(crate) fn start(
        engine: &Arc<Engine>,
        wanted: Vec<(FolderId, PathBuf)>,
    ) -> Result<Self, String> {
        let mut mounts = Vec::new();
        for (folder, mountpoint) in wanted {
            let path = engine
                .folders()
                .into_iter()
                .find(|f| f.id == folder)
                .map(|f| f.path)
                .ok_or_else(|| format!("folder {folder} is not running"))?;
            let backend =
                EngineFolder::new(engine.clone(), folder, tokio::runtime::Handle::current());
            // Inside the engine's reserved directory, which scans skip, on the folder's own
            // filesystem so recording a write is a rename.
            let staging = path.join(".tidefell").join("mount");
            let mount = tidefell_fs_fuse::mount(backend, &mountpoint, staging).map_err(|e| {
                format!(
                    "cannot mount folder {folder} at {}: {e}",
                    mountpoint.display()
                )
            })?;
            mounts.push((folder, Arc::new(mount)));
        }
        let watcher = (!mounts.is_empty()).then(|| {
            let mut events = engine.events();
            let mounts = mounts.clone();
            tokio::spawn(async move {
                loop {
                    let mut changed = Vec::new();
                    match events.recv().await {
                        Ok(event) => changed.extend(folder_of(&event)),
                        Err(RecvError::Lagged(_)) => changed.extend(mounts.iter().map(|m| m.0)),
                        Err(RecvError::Closed) => return,
                    }
                    tokio::time::sleep(REFRESH_DELAY).await;
                    while let Ok(event) = events.try_recv() {
                        changed.extend(folder_of(&event));
                    }
                    for (folder, mount) in &mounts {
                        if changed.contains(folder) {
                            let mount = mount.clone();
                            let _ = tokio::task::spawn_blocking(move || mount.changed()).await;
                        }
                    }
                }
            })
        });
        Ok(Self { mounts, watcher })
    }

    /// Unmounts everything.
    pub(crate) async fn stop(mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
            let _ = watcher.await;
        }
        for (_, mount) in self.mounts.drain(..) {
            let Ok(mount) = Arc::try_unwrap(mount) else {
                continue;
            };
            let point = mount.mountpoint().to_owned();
            match tokio::task::spawn_blocking(move || mount.unmount()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(mountpoint = %point.display(), error = %e, "unmount failed")
                }
                Err(e) => tracing::warn!(error = %e, "unmount task failed"),
            }
        }
    }
}

fn folder_of(event: &Event) -> Option<FolderId> {
    match event {
        Event::FolderChanged(f) | Event::FolderRemoved(f) => Some(*f),
        Event::FileUpdated { folder, .. } | Event::FileRemoved { folder, .. } => Some(*folder),
        _ => None,
    }
}
