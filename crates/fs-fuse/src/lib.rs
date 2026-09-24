//! Tidefell `fs-fuse` crate: Linux placeholder filesystem (ADR 0017).
//!
//! Mounts one folder with FUSE. Listings come from the folder's metadata, opening a file
//! fetches it if it is a placeholder, and writes are staged and recorded when the file is
//! closed.

mod backend;
mod core;
mod fs;

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use fuser::{BackgroundSession, Config, INodeNo, MountOption, Notifier};
use tracing::{debug, info, warn};

pub use backend::{Backend, BackendError, EngineFolder, FileEntry};

use crate::core::Core;
use crate::fs::FuseFs;

/// Kernel-free access to the filesystem's refresh, so [`Mount`] need not be generic.
trait Refresh: Send + Sync {
    fn refresh(&self) -> Vec<u64>;
}

impl<B: Backend> Refresh for Core<B> {
    fn refresh(&self) -> Vec<u64> {
        self.refresh().unwrap_or_default()
    }
}

/// A mounted folder. Dropping it unmounts.
pub struct Mount {
    session: Option<BackgroundSession>,
    notifier: Notifier,
    core: Arc<dyn Refresh>,
    mountpoint: PathBuf,
}

/// Mounts `backend` at `mountpoint`, staging written content in `staging_dir`, which should
/// be on the same filesystem as the folder.
pub fn mount<B: Backend>(backend: B, mountpoint: &Path, staging_dir: PathBuf) -> io::Result<Mount> {
    clear_stale(mountpoint);
    std::fs::create_dir_all(mountpoint)?;
    let owner = std::fs::metadata(mountpoint)?;
    let core = Arc::new(Core::new(backend, staging_dir, owner.uid(), owner.gid())?);
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("tidefell".to_owned()),
        MountOption::Subtype("tidefell".to_owned()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoAtime,
    ];
    // Fetching a placeholder can take a while; other requests keep being served meanwhile.
    config.n_threads = Some(4);
    let session = fuser::spawn_mount(FuseFs(core.clone()), mountpoint, &config)?;
    let notifier = session.notifier();
    info!(mountpoint = %mountpoint.display(), "mounted folder");
    Ok(Mount {
        session: Some(session),
        notifier,
        core,
        mountpoint: mountpoint.to_owned(),
    })
}

impl Mount {
    /// Where the folder is mounted.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Tells the mount the folder changed, so listings are read again and the kernel drops
    /// cached content of files that changed.
    pub fn changed(&self) {
        for ino in self.core.refresh() {
            if let Err(e) = self.notifier.inval_inode(INodeNo(ino), 0, 0) {
                debug!(error = %e, "cache invalidation failed");
            }
        }
    }

    /// Unmounts, waiting for in-flight requests to finish.
    pub fn unmount(mut self) -> io::Result<()> {
        let result = match self.session.take() {
            Some(session) => session.umount_and_join(),
            None => Ok(()),
        };
        info!(mountpoint = %self.mountpoint.display(), "unmounted folder");
        result
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            if let Err(e) = session.umount_and_join() {
                warn!(mountpoint = %self.mountpoint.display(), error = %e, "unmount failed");
            }
        }
    }
}

/// Unmounts a mount left behind by a process that exited without unmounting, which leaves
/// the mount point failing with "transport endpoint is not connected".
fn clear_stale(mountpoint: &Path) {
    match std::fs::metadata(mountpoint) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => {}
        _ => return,
    }
    warn!(mountpoint = %mountpoint.display(), "clearing a stale mount");
    for (program, args) in [("fusermount3", ["-u", "-z"]), ("fusermount", ["-u", "-z"])] {
        if Command::new(program)
            .args(args)
            .arg(mountpoint)
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
    }
    let _ = Command::new("umount").arg("-l").arg(mountpoint).status();
}

#[cfg(test)]
mod tests;
