//! What the filesystem needs from a folder, and the engine's implementation of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tidefell_engine::Engine;
use tidefell_proto::FolderId;

/// A live file in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path within the folder.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub modified_ms: i64,
    /// Whether the file is executable.
    pub executable: bool,
}

/// Why a backend operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// No live file has this path.
    #[error("no such file")]
    NotFound,
    /// The content could not be fetched from any connected peer.
    #[error("not available from any connected peer")]
    Unavailable,
    /// The request was invalid, such as a path that cannot be synced.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// A folder as the filesystem sees it. Calls block; the filesystem makes them from its own
/// threads and never while holding its locks.
pub trait Backend: Send + Sync + 'static {
    /// Every live file.
    fn files(&self) -> Result<Vec<FileEntry>, BackendError>;
    /// Makes `path` available on disk, fetching it if needed, and returns where it is. The
    /// file stays there until a matching [`Backend::close`].
    fn open(&self, path: &str) -> Result<PathBuf, BackendError>;
    /// Releases one [`Backend::open`].
    fn close(&self, path: &str);
    /// Records new content for `path`, taking ownership of the file at `source`.
    fn write(&self, path: &str, source: &Path) -> Result<(), BackendError>;
    /// Deletes the live file at `path`.
    fn delete(&self, path: &str) -> Result<(), BackendError>;
    /// Moves the file at `from`, or every file under the directory `from`, to `to`,
    /// replacing any file already there.
    fn rename(&self, from: &str, to: &str) -> Result<(), BackendError>;
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
        Error::Unavailable(_) => BackendError::Unavailable,
        Error::Invalid(m) => BackendError::Invalid(m),
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
                size: f.size,
                modified_ms: f.modified_ms,
                executable: f.executable,
            })
            .collect())
    }

    fn open(&self, path: &str) -> Result<PathBuf, BackendError> {
        self.runtime
            .block_on(self.engine.open(self.folder, path))
            .map_err(engine_error)
    }

    fn close(&self, path: &str) {
        let _ = self.engine.close(self.folder, path);
    }

    fn write(&self, path: &str, source: &Path) -> Result<(), BackendError> {
        self.runtime
            .block_on(self.engine.write(self.folder, path, source))
            .map_err(engine_error)
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
}
