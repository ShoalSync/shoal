//! What the provider needs from a folder, and the engine's implementation of it.

use std::sync::Arc;

use tidefell_engine::Engine;
use tidefell_proto::{ContentHash, EntryId, FolderId};

/// A live file in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Where the file is in the folder, `/`-separated.
    pub path: String,
    /// The entry it is.
    pub entry: EntryId,
    /// Its current content.
    pub content: ContentHash,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub modified_ms: i64,
    /// Whether the engine has the current version on disk.
    pub on_disk: bool,
    /// Whether it is pinned on this device.
    pub pinned: bool,
}

/// Why a backend operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// No live file has this path.
    #[error("no such file")]
    NotFound,
    /// The content could not be fetched from any connected peer, or is no longer current.
    #[error("not available from any connected peer")]
    Unavailable,
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// The fetched content's destination: called with each piece of it, in order, by offset.
pub type Sink<'a> = &'a mut (dyn FnMut(u64, &[u8]) -> std::io::Result<()> + Send);

/// A folder as the provider sees it. Calls block; the provider makes them from its own threads.
pub trait Backend: Send + Sync + 'static {
    /// Every live file.
    fn files(&self) -> Result<Vec<FileEntry>, BackendError>;
    /// The paths of the placeholders recorded.
    fn placeholders(&self) -> Result<Vec<String>, BackendError>;
    /// Records the placeholder just made at `path`, showing `content`.
    fn placeholder_made(&self, path: &str, content: ContentHash) -> Result<(), BackendError>;
    /// Forgets the placeholder at `path`, about to be removed.
    fn placeholder_gone(&self, path: &str) -> Result<(), BackendError>;
    /// Fetches `content`, the file at `path`'s, into `sink`.
    fn fetch(&self, path: &str, content: ContentHash, sink: Sink<'_>) -> Result<(), BackendError>;
    /// Looks again at the file at `path`, changed in a way a watch does not report.
    fn scan_path(&self, path: &str);
    /// Looks again at the whole folder.
    fn scan(&self);
    /// Deletes the live file at `path`, already gone from the folder.
    fn delete(&self, path: &str) -> Result<(), BackendError>;
    /// Moves the file or directory at `from`, already moved in the folder, to `to`.
    fn rename(&self, from: &str, to: &str) -> Result<(), BackendError>;
    /// Pins or unpins `path` on this device.
    fn set_pinned(&self, path: &str, pinned: bool) -> Result<(), BackendError>;
}

/// One folder of a running [`Engine`].
pub struct EngineFolder {
    engine: Arc<Engine>,
    folder: FolderId,
    runtime: tokio::runtime::Handle,
}

impl EngineFolder {
    /// Serves `folder` from `engine`, running async engine calls on `runtime`.
    pub fn new(engine: Arc<Engine>, folder: FolderId, runtime: tokio::runtime::Handle) -> Self {
        Self {
            engine,
            folder,
            runtime,
        }
    }
}

fn engine_error(e: tidefell_engine::Error) -> BackendError {
    use tidefell_engine::Error;
    match e {
        Error::NoSuchFile(_) => BackendError::NotFound,
        Error::Unavailable(_) | Error::Paused(_) => BackendError::Unavailable,
        other => BackendError::Other(other.to_string()),
    }
}

impl Backend for EngineFolder {
    fn files(&self) -> Result<Vec<FileEntry>, BackendError> {
        Ok(self
            .engine
            .files(self.folder)
            .map_err(engine_error)?
            .into_iter()
            .map(|f| FileEntry {
                path: f.path,
                entry: f.entry,
                content: f.content,
                size: f.size,
                modified_ms: f.modified_ms,
                on_disk: f.on_disk,
                pinned: f.pinned,
            })
            .collect())
    }

    fn placeholders(&self) -> Result<Vec<String>, BackendError> {
        Ok(self
            .engine
            .placeholders(self.folder)
            .map_err(engine_error)?
            .into_iter()
            .map(|(path, _)| path)
            .collect())
    }

    fn placeholder_made(&self, path: &str, content: ContentHash) -> Result<(), BackendError> {
        self.engine
            .placeholder_made(self.folder, path, content)
            .map_err(engine_error)
    }

    fn placeholder_gone(&self, path: &str) -> Result<(), BackendError> {
        self.engine
            .placeholder_gone(self.folder, path)
            .map_err(engine_error)
    }

    fn fetch(&self, path: &str, content: ContentHash, sink: Sink<'_>) -> Result<(), BackendError> {
        self.runtime
            .block_on(self.engine.fetch(self.folder, path, Some(content), sink))
            .map_err(engine_error)
    }

    fn scan_path(&self, path: &str) {
        if let Err(e) = self
            .runtime
            .block_on(self.engine.scan_path(self.folder, path))
        {
            tracing::debug!(path, error = %e, "cannot scan");
        }
    }

    fn scan(&self) {
        if let Err(e) = self.runtime.block_on(self.engine.scan(self.folder)) {
            tracing::debug!(error = %e, "cannot scan");
        }
    }

    fn delete(&self, path: &str) -> Result<(), BackendError> {
        self.runtime
            .block_on(self.engine.delete(self.folder, path))
            .map_err(engine_error)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), BackendError> {
        self.runtime
            .block_on(self.engine.rename(self.folder, from, to))
            .map_err(engine_error)
    }

    fn set_pinned(&self, path: &str, pinned: bool) -> Result<(), BackendError> {
        let done = if pinned {
            self.runtime.block_on(self.engine.pin(self.folder, path))
        } else {
            self.runtime.block_on(self.engine.unpin(self.folder, path))
        };
        done.map_err(engine_error)
    }
}
