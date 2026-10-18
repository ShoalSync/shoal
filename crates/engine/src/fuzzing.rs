//! Entry points for the fuzz targets (ADR 0031): what the engine reads from disk, which may be
//! truncated or corrupted.

use std::fs;
use std::time::Duration;

use tidefell_meta::Retention;
use tidefell_proto::{DeviceId, FolderId};

use crate::config::{FolderConfig, ReplicaMode};
use crate::state::FolderState;

/// Opens a folder whose state directory holds `meta` and `state` as its files, and throws the
/// result away. Errors are fine; a panic is a finding.
pub fn open_folder(meta: &[u8], state: &[u8]) {
    let data = std::env::temp_dir().join(format!("tidefell-fuzz-{}", std::process::id()));
    let folder = FolderId([7; 32]);
    let dir = data.join("folders").join(folder.to_string());
    let _ = fs::remove_dir_all(&data);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    if !meta.is_empty() && fs::write(dir.join("meta"), meta).is_err() {
        return;
    }
    if fs::write(dir.join("state"), state).is_err() {
        return;
    }
    let config = FolderConfig {
        id: folder,
        path: data.join("root"),
        mode: ReplicaMode::Full,
        members: Vec::new(),
        retention: Retention::Days(30),
        scan_interval: Duration::from_secs(3600),
        sync: Default::default(),
        confirm_deletes: Default::default(),
        encryption: None,
    };
    let _ = FolderState::open(&data, &data.join("metadata"), config, DeviceId([1; 32]));
    let _ = fs::remove_dir_all(&data);
}
