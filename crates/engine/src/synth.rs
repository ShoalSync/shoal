//! Synthetic folders for measuring memory at scale (ADR 0035): a folder's metadata written
//! directly, as if its files had been scanned, beside sparse files that match it, so a folder of
//! a million files and terabytes of content takes minutes and no real data. Not for use outside
//! the measurement.

use std::fs;
use std::path::Path;

use tidefell_identity::DeviceKey;
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ChunkHash, ChunkRef, ContentHash, EntryId, Manifest, VersionVector};

use crate::config::FolderConfig;
use crate::state::{FolderState, LocalFile};
use crate::{io_err, Error, Result};

/// The average chunk of real files (`Params::DEFAULT`).
const CHUNK: u64 = 256 * 1024;

/// What to generate.
#[derive(Debug, Clone, Copy)]
pub struct SynthFolder {
    /// Files, spread over a thousand directories.
    pub files: usize,
    /// Their total size, in bytes, about evenly spread.
    pub total_bytes: u64,
}

fn hash32(seed: &[u8]) -> [u8; 32] {
    *blake3::hash(seed).as_bytes()
}

/// Writes `spec`'s files into `folder`'s root, as sparse files, and its metadata into the
/// database the engine at `data_dir` opens, as this device's (`key`) own changes.
pub fn generate(
    data_dir: &Path,
    folder: &FolderConfig,
    key: &DeviceKey,
    spec: SynthFolder,
) -> Result<()> {
    crate::root::create(&folder.path, folder.id, key.id())?;
    let metadata_dir = data_dir.join("metadata");
    let mut state =
        FolderState::open_or_rebuild(data_dir, &metadata_dir, folder.clone(), key.id(), None)?;
    let size = (spec.total_bytes / spec.files.max(1) as u64).max(1);
    for n in 0..spec.files {
        let path = format!("d{:03}/f{n:07}.bin", n % 1000);
        let abs = folder.path.join(&path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        let file = fs::File::create(&abs).map_err(io_err(&abs))?;
        file.set_len(size).map_err(io_err(&abs))?;
        drop(file);
        let disk = crate::scan::stat(&abs).ok_or_else(|| Error::Io {
            path: abs.clone(),
            source: std::io::Error::other("the file vanished"),
        })?;
        let seed = (n as u64).to_le_bytes();
        let content = ContentHash(hash32(&[b"content".as_slice(), &seed].concat()));
        let chunks = (0..size.div_ceil(CHUNK))
            .map(|i| ChunkRef {
                hash: ChunkHash(hash32(&[&seed[..], &i.to_le_bytes()].concat())),
                len: CHUNK.min(size - i * CHUNK) as u32,
            })
            .collect();
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&seed);
        let entry = EntryId(id);
        let (_, base) = state.meta.local_change_from(
            key.signing_key(),
            entry,
            &VersionVector::new(),
            &path,
            EntryKind::File {
                content,
                size,
                executable: false,
            },
            (disk.mtime_ns / 1_000_000) as i64,
        )?;
        state.add_manifest(Manifest { content, chunks });
        state.local.insert(
            path,
            LocalFile {
                entry,
                content,
                size,
                mtime_ns: disk.mtime_ns,
                executable: disk.executable,
                base,
            },
        );
        if n % 10_000 == 9_999 {
            state.save()?;
        }
    }
    state.close()
}
