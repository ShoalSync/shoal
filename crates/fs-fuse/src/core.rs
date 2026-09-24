//! The filesystem's logic, independent of the kernel interface so tests can drive it directly.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{Errno, FileAttr, FileType, INodeNo};
use tidefell_proto::sync::is_valid_path;
use tracing::warn;

use crate::backend::{Backend, BackendError, FileEntry};

/// How long the listing from the backend is trusted before it is read again.
const LISTING_TTL: Duration = Duration::from_secs(1);

fn errno(e: BackendError) -> Errno {
    match e {
        BackendError::NotFound => Errno::ENOENT,
        BackendError::Unavailable => Errno::EIO,
        BackendError::Invalid(_) => Errno::EINVAL,
        BackendError::Other(m) => {
            warn!(error = %m, "folder operation failed");
            Errno::EIO
        }
    }
}

fn io_errno(e: io::Error) -> Errno {
    warn!(error = %e, "staging file operation failed");
    Errno::from(e)
}

fn system_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH
    }
}

/// The path of `name` in the directory `parent` ("" is the root).
pub(crate) fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn under(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn moved(path: &str, from: &str, to: &str) -> Option<String> {
    if path == from {
        Some(to.to_owned())
    } else {
        path.strip_prefix(from)
            .filter(|rest| rest.starts_with('/'))
            .map(|rest| format!("{to}{rest}"))
    }
}

/// What a path is in the mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
}

/// Inode numbers, stable for as long as the mount lives. Renames carry numbers along.
#[derive(Default)]
struct Inodes {
    by_path: HashMap<String, u64>,
    by_ino: HashMap<u64, String>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut inodes = Self {
            next: INodeNo::ROOT.0 + 1,
            ..Self::default()
        };
        inodes.by_path.insert(String::new(), INodeNo::ROOT.0);
        inodes.by_ino.insert(INodeNo::ROOT.0, String::new());
        inodes
    }

    fn ino(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.by_path.get(path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_path.insert(path.to_owned(), ino);
        self.by_ino.insert(ino, path.to_owned());
        ino
    }

    fn path(&self, ino: u64) -> Option<&str> {
        self.by_ino.get(&ino).map(String::as_str)
    }

    fn rename(&mut self, from: &str, to: &str) {
        let affected: Vec<(String, u64)> = self
            .by_path
            .iter()
            .filter_map(|(p, ino)| moved(p, from, to).map(|_| (p.clone(), *ino)))
            .collect();
        // Whatever was at the destination is gone.
        let replaced: Vec<String> = self
            .by_path
            .keys()
            .filter(|p| moved(p, to, to).is_some())
            .cloned()
            .collect();
        for p in replaced {
            if let Some(ino) = self.by_path.remove(&p) {
                self.by_ino.remove(&ino);
            }
        }
        for (old, ino) in affected {
            self.by_path.remove(&old);
            let new = moved(&old, from, to).expect("affected paths move");
            self.by_path.insert(new.clone(), ino);
            self.by_ino.insert(ino, new);
        }
    }
}

/// Content being written through the mount, kept in a staging file until it is recorded.
pub(crate) struct Staged {
    file: PathBuf,
    meta: Mutex<StagedMeta>,
}

struct StagedMeta {
    /// Where the content goes; follows renames.
    path: String,
    /// Open write handles.
    open: usize,
    /// Changed since it was last recorded.
    dirty: bool,
    /// Unlinked; the content is dropped when the last handle closes.
    deleted: bool,
}

impl Staged {
    fn meta(&self) -> MutexGuard<'_, StagedMeta> {
        self.meta.lock().unwrap()
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
    }
}

enum Handle {
    /// A file opened for reading, pinned in the folder until it is released.
    Read { file: File, pinned: String },
    /// A file opened for writing.
    Write { file: File, staged: Arc<Staged> },
}

struct State {
    files: BTreeMap<String, FileEntry>,
    loaded_at: Option<Instant>,
    inodes: Inodes,
    /// Empty directories made through the mount; they exist only here.
    dirs: BTreeSet<String>,
    /// Content being written, by the path it will be recorded at.
    staged: HashMap<String, Arc<Staged>>,
    handles: HashMap<u64, Arc<Handle>>,
}

impl State {
    fn kind(&self, path: &str) -> Option<Kind> {
        if self.staged.contains_key(path) || self.files.contains_key(path) {
            return Some(Kind::File);
        }
        if path.is_empty() || self.dirs.contains(path) || self.has_children(path) {
            return Some(Kind::Dir);
        }
        None
    }

    fn has_children(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        let in_files = self
            .files
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(p, _)| p.starts_with(&prefix));
        let in_dirs = self
            .dirs
            .range(prefix.clone()..)
            .next()
            .is_some_and(|p| p.starts_with(&prefix));
        in_files || in_dirs || self.staged.keys().any(|p| p.starts_with(&prefix))
    }

    /// The immediate children of `dir`, sorted by name.
    fn children(&self, dir: &str) -> Vec<(String, Kind)> {
        let mut out: BTreeMap<String, Kind> = BTreeMap::new();
        let mut add = |path: &str, is_file: bool| {
            if !under(path, dir) || path == dir {
                return;
            }
            let rest = if dir.is_empty() {
                path
            } else {
                &path[dir.len() + 1..]
            };
            match rest.split_once('/') {
                Some((name, _)) => {
                    out.insert(name.to_owned(), Kind::Dir);
                }
                None => {
                    let kind = if is_file { Kind::File } else { Kind::Dir };
                    out.entry(rest.to_owned()).or_insert(kind);
                }
            }
        };
        for p in self.files.keys() {
            add(p, true);
        }
        for p in self.staged.keys() {
            add(p, true);
        }
        for p in &self.dirs {
            add(p, false);
        }
        out.into_iter().collect()
    }
}

/// A directory listing entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirEntry {
    pub(crate) ino: u64,
    pub(crate) kind: Kind,
    pub(crate) name: String,
}

/// The filesystem over one folder.
pub(crate) struct Core<B: Backend> {
    backend: B,
    staging_dir: PathBuf,
    uid: u32,
    gid: u32,
    mounted_at: SystemTime,
    state: Mutex<State>,
    next_handle: AtomicU64,
}

impl<B: Backend> Core<B> {
    /// `staging_dir` holds content being written; it should be on the same filesystem as the
    /// folder so recording it is a rename.
    pub(crate) fn new(backend: B, staging_dir: PathBuf, uid: u32, gid: u32) -> io::Result<Self> {
        fs::create_dir_all(&staging_dir)?;
        // Leftovers from a previous mount that did not shut down cleanly.
        for entry in fs::read_dir(&staging_dir)?.flatten() {
            let _ = fs::remove_file(entry.path());
        }
        Ok(Self {
            backend,
            staging_dir,
            uid,
            gid,
            mounted_at: SystemTime::now(),
            state: Mutex::new(State {
                files: BTreeMap::new(),
                loaded_at: None,
                inodes: Inodes::new(),
                dirs: BTreeSet::new(),
                staged: HashMap::new(),
                handles: HashMap::new(),
            }),
            next_handle: AtomicU64::new(1),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Marks the listing stale so the next operation reads it again.
    pub(crate) fn invalidate(&self) {
        self.lock().loaded_at = None;
    }

    /// Reads the listing again if it is stale, and returns the state.
    fn fresh(&self) -> Result<MutexGuard<'_, State>, Errno> {
        let stale = self
            .lock()
            .loaded_at
            .is_none_or(|t| t.elapsed() >= LISTING_TTL);
        if stale {
            let files = self.backend.files().map_err(errno)?;
            let mut state = self.lock();
            state.files = files.into_iter().map(|f| (f.path.clone(), f)).collect();
            state.loaded_at = Some(Instant::now());
            // A directory stops being "empty" once a file appears in it.
            let dirs: Vec<String> = state.dirs.iter().cloned().collect();
            for d in dirs {
                let prefix = format!("{d}/");
                if state.files.keys().any(|p| p.starts_with(&prefix)) {
                    state.dirs.remove(&d);
                }
            }
            return Ok(state);
        }
        Ok(self.lock())
    }

    /// Reads the listing now and returns the inodes of files whose size or modification time
    /// changed, for the kernel to drop cached pages.
    pub(crate) fn refresh(&self) -> Result<Vec<u64>, Errno> {
        let before = self.lock().files.clone();
        self.invalidate();
        let state = self.fresh()?;
        Ok(state
            .files
            .iter()
            .filter(|(p, f)| {
                before
                    .get(*p)
                    .is_some_and(|b| b.size != f.size || b.modified_ms != f.modified_ms)
            })
            .filter_map(|(p, _)| state.inodes.by_path.get(p).copied())
            .collect())
    }

    fn path_of(&self, state: &State, ino: u64) -> Result<String, Errno> {
        state
            .inodes
            .path(ino)
            .map(str::to_owned)
            .ok_or(Errno::ENOENT)
    }

    fn file_attr(&self, ino: u64, size: u64, mtime: SystemTime, executable: bool) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: FileType::RegularFile,
            perm: if executable { 0o755 } else { 0o644 },
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn dir_attr(&self, ino: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: 0,
            blocks: 0,
            atime: self.mounted_at,
            mtime: self.mounted_at,
            ctime: self.mounted_at,
            crtime: self.mounted_at,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn attr(&self, state: &mut State, path: &str) -> Result<FileAttr, Errno> {
        let kind = state.kind(path).ok_or(Errno::ENOENT)?;
        let ino = state.inodes.ino(path);
        match kind {
            Kind::Dir => Ok(self.dir_attr(ino)),
            Kind::File => {
                if let Some(staged) = state.staged.get(path) {
                    let meta = fs::metadata(&staged.file).map_err(io_errno)?;
                    return Ok(self.file_attr(
                        ino,
                        meta.len(),
                        meta.modified().unwrap_or(self.mounted_at),
                        meta.permissions().mode() & 0o111 != 0,
                    ));
                }
                let f = &state.files[path];
                Ok(self.file_attr(ino, f.size, system_time(f.modified_ms), f.executable))
            }
        }
    }

    fn name_path(&self, state: &State, parent: u64, name: &str) -> Result<String, Errno> {
        let parent = self.path_of(state, parent)?;
        Ok(child_path(&parent, name))
    }

    pub(crate) fn lookup(&self, parent: u64, name: &str) -> Result<FileAttr, Errno> {
        let mut state = self.fresh()?;
        let path = self.name_path(&state, parent, name)?;
        self.attr(&mut state, &path)
    }

    pub(crate) fn getattr(&self, ino: u64) -> Result<FileAttr, Errno> {
        let mut state = self.fresh()?;
        let path = self.path_of(&state, ino)?;
        self.attr(&mut state, &path)
    }

    pub(crate) fn readdir(&self, ino: u64) -> Result<Vec<DirEntry>, Errno> {
        let mut state = self.fresh()?;
        let path = self.path_of(&state, ino)?;
        match state.kind(&path) {
            Some(Kind::Dir) => {}
            Some(Kind::File) => return Err(Errno::ENOTDIR),
            None => return Err(Errno::ENOENT),
        }
        let children = state.children(&path);
        Ok(children
            .into_iter()
            .map(|(name, kind)| DirEntry {
                ino: state.inodes.ino(&child_path(&path, &name)),
                kind,
                name,
            })
            .collect())
    }

    fn new_handle(&self, state: &mut State, handle: Handle) -> u64 {
        let fh = self.next_handle.fetch_add(1, Ordering::Relaxed);
        state.handles.insert(fh, Arc::new(handle));
        fh
    }

    fn staging_file(&self) -> Result<(PathBuf, File), Errno> {
        for _ in 0..16 {
            let name = format!("{:016x}", rand_u64());
            let path = self.staging_dir.join(name);
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io_errno(e)),
            }
        }
        Err(Errno::EIO)
    }

    /// Starts (or joins) writing `path`. Unless `truncate`, existing content is copied into
    /// the staging file first, fetching it if it is a placeholder.
    fn stage(&self, path: &str, truncate: bool, executable: bool) -> Result<Arc<Staged>, Errno> {
        {
            let state = self.lock();
            if let Some(staged) = state.staged.get(path) {
                let staged = staged.clone();
                staged.meta().open += 1;
                if truncate {
                    let file = OpenOptions::new()
                        .write(true)
                        .open(&staged.file)
                        .map_err(io_errno)?;
                    file.set_len(0).map_err(io_errno)?;
                    staged.meta().dirty = true;
                }
                return Ok(staged);
            }
        }
        let (file_path, file) = self.staging_file()?;
        let existing = self.lock().files.get(path).cloned();
        let mut exec = executable;
        if let Some(entry) = &existing {
            exec = entry.executable;
            if !truncate {
                let local = self.backend.open(path).map_err(errno)?;
                let copied = fs::copy(&local, &file_path);
                self.backend.close(path);
                copied.map_err(io_errno)?;
            }
        }
        drop(file);
        fs::set_permissions(
            &file_path,
            Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
        )
        .map_err(io_errno)?;
        let staged = Arc::new(Staged {
            file: file_path,
            meta: Mutex::new(StagedMeta {
                path: path.to_owned(),
                open: 1,
                // New and truncated files must be recorded even if nothing is written.
                dirty: existing.is_none() || truncate,
                deleted: false,
            }),
        });
        let mut state = self.lock();
        if let Some(other) = state.staged.get(path) {
            // Another handle started staging meanwhile; join it.
            let other = other.clone();
            other.meta().open += 1;
            return Ok(other);
        }
        state.staged.insert(path.to_owned(), staged.clone());
        Ok(staged)
    }

    /// Records the staged content if it changed.
    fn commit(&self, staged: &Staged) -> Result<(), Errno> {
        let path = {
            let meta = staged.meta();
            if !meta.dirty || meta.deleted {
                return Ok(());
            }
            meta.path.clone()
        };
        // The backend takes ownership of what it records, so hand it a copy and keep the
        // staging file for handles that are still open.
        let (copy, file) = self.staging_file()?;
        drop(file);
        fs::copy(&staged.file, &copy).map_err(io_errno)?;
        let perms = fs::metadata(&staged.file).map_err(io_errno)?.permissions();
        fs::set_permissions(&copy, perms.clone()).map_err(io_errno)?;
        let result = self.backend.write(&path, &copy);
        let _ = fs::remove_file(&copy);
        result.map_err(errno)?;
        staged.meta().dirty = false;
        let size = fs::metadata(&staged.file).map(|m| m.len()).unwrap_or(0);
        let mut state = self.lock();
        state.files.insert(
            path.clone(),
            FileEntry {
                path,
                size,
                modified_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                executable: perms.mode() & 0o111 != 0,
            },
        );
        state.loaded_at = None;
        Ok(())
    }

    fn release_staged(&self, staged: &Arc<Staged>) -> Result<(), Errno> {
        let result = self.commit(staged);
        let last = {
            let mut meta = staged.meta();
            meta.open -= 1;
            meta.open == 0
        };
        if last {
            let mut state = self.lock();
            let path = staged.meta().path.clone();
            if state
                .staged
                .get(&path)
                .is_some_and(|s| Arc::ptr_eq(s, staged))
            {
                state.staged.remove(&path);
            }
        }
        result
    }

    pub(crate) fn open(&self, ino: u64, write: bool, truncate: bool) -> Result<u64, Errno> {
        let path = {
            let state = self.fresh()?;
            let path = self.path_of(&state, ino)?;
            match state.kind(&path) {
                Some(Kind::File) => path,
                Some(Kind::Dir) => return Err(Errno::EISDIR),
                None => return Err(Errno::ENOENT),
            }
        };
        if write {
            let staged = self.stage(&path, truncate, false)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&staged.file)
                .map_err(io_errno)?;
            let mut state = self.lock();
            return Ok(self.new_handle(&mut state, Handle::Write { file, staged }));
        }
        // Readers see content still being written through the mount.
        let staged = self.lock().staged.get(&path).cloned();
        if let Some(staged) = staged {
            staged.meta().open += 1;
            let file = File::open(&staged.file).map_err(io_errno)?;
            let mut state = self.lock();
            return Ok(self.new_handle(&mut state, Handle::Write { file, staged }));
        }
        let local = self.backend.open(&path).map_err(errno)?;
        let file = match File::open(&local) {
            Ok(file) => file,
            Err(e) => {
                self.backend.close(&path);
                return Err(io_errno(e));
            }
        };
        let mut state = self.lock();
        Ok(self.new_handle(&mut state, Handle::Read { file, pinned: path }))
    }

    pub(crate) fn create(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
    ) -> Result<(FileAttr, u64), Errno> {
        let path = {
            let state = self.fresh()?;
            let path = self.name_path(&state, parent, name)?;
            if !is_valid_path(&path) {
                return Err(Errno::EINVAL);
            }
            path
        };
        let staged = self.stage(&path, true, mode & 0o111 != 0)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staged.file)
            .map_err(io_errno)?;
        let mut state = self.lock();
        let parent_path = path.rsplit_once('/').map(|(p, _)| p.to_owned());
        if let Some(p) = parent_path {
            state.dirs.remove(&p);
        }
        let fh = self.new_handle(&mut state, Handle::Write { file, staged });
        let attr = self.attr(&mut state, &path)?;
        Ok((attr, fh))
    }

    fn handle(&self, fh: u64) -> Result<Arc<Handle>, Errno> {
        self.lock().handles.get(&fh).cloned().ok_or(Errno::EBADF)
    }

    pub(crate) fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let handle = self.handle(fh)?;
        let file = match &*handle {
            Handle::Read { file, .. } | Handle::Write { file, .. } => file,
        };
        let mut buf = vec![0; size as usize];
        let mut read = 0;
        while read < buf.len() {
            match file.read_at(&mut buf[read..], offset + read as u64) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_errno(e)),
            }
        }
        buf.truncate(read);
        Ok(buf)
    }

    pub(crate) fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        let handle = self.handle(fh)?;
        match &*handle {
            Handle::Write { file, staged } => {
                file.write_all_at(data, offset).map_err(io_errno)?;
                staged.meta().dirty = true;
                Ok(data.len() as u32)
            }
            Handle::Read { .. } => Err(Errno::EBADF),
        }
    }

    /// Records written content now (on `flush` and `fsync`).
    pub(crate) fn sync(&self, fh: u64) -> Result<(), Errno> {
        match &*self.handle(fh)? {
            Handle::Write { staged, .. } => self.commit(staged),
            Handle::Read { .. } => Ok(()),
        }
    }

    pub(crate) fn release(&self, fh: u64) -> Result<(), Errno> {
        let Some(handle) = self.lock().handles.remove(&fh) else {
            return Err(Errno::EBADF);
        };
        match &*handle {
            Handle::Read { pinned, .. } => {
                self.backend.close(pinned);
                Ok(())
            }
            Handle::Write { staged, .. } => self.release_staged(staged),
        }
    }

    /// Changes size or the executable bit. Other attribute changes are accepted and ignored,
    /// except ownership, which cannot change.
    pub(crate) fn setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        fh: Option<u64>,
    ) -> Result<FileAttr, Errno> {
        if uid.is_some_and(|u| u != self.uid) || gid.is_some_and(|g| g != self.gid) {
            return Err(Errno::EPERM);
        }
        let current = self.getattr(ino)?;
        let path = self.path_of(&self.lock(), ino)?;
        let exec_change = mode
            .map(|m| m & 0o111 != 0)
            .filter(|exec| *exec != (current.perm & 0o111 != 0));
        if current.kind == FileType::Directory {
            if size.is_some() {
                return Err(Errno::EISDIR);
            }
            return Ok(current);
        }
        if size.is_none() && exec_change.is_none() {
            return Ok(current);
        }
        // Use the writer's staging file if one is open, else stage just for this change.
        let open_staged = fh
            .and_then(|fh| self.lock().handles.get(&fh).cloned())
            .and_then(|h| match &*h {
                Handle::Write { staged, .. } => Some(staged.clone()),
                Handle::Read { .. } => None,
            });
        let (staged, temporary) = match open_staged {
            Some(staged) => (staged, false),
            None => (self.stage(&path, size == Some(0), false)?, true),
        };
        let result = (|| {
            if let Some(size) = size {
                let file = OpenOptions::new()
                    .write(true)
                    .open(&staged.file)
                    .map_err(io_errno)?;
                file.set_len(size).map_err(io_errno)?;
            }
            if let Some(exec) = exec_change {
                fs::set_permissions(
                    &staged.file,
                    Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
                )
                .map_err(io_errno)?;
            }
            staged.meta().dirty = true;
            if !temporary {
                return Ok(());
            }
            self.commit(&staged)
        })();
        if temporary {
            self.release_staged(&staged)?;
        }
        result?;
        self.getattr(ino)
    }

    pub(crate) fn unlink(&self, parent: u64, name: &str) -> Result<(), Errno> {
        let (path, in_backend, staged) = {
            let state = self.fresh()?;
            let path = self.name_path(&state, parent, name)?;
            match state.kind(&path) {
                Some(Kind::File) => {}
                Some(Kind::Dir) => return Err(Errno::EISDIR),
                None => return Err(Errno::ENOENT),
            }
            let in_backend = state.files.contains_key(&path);
            let staged = state.staged.get(&path).cloned();
            (path, in_backend, staged)
        };
        if in_backend {
            self.backend.delete(&path).map_err(errno)?;
        }
        let mut state = self.lock();
        if let Some(staged) = staged {
            staged.meta().deleted = true;
            state.staged.remove(&path);
        }
        state.files.remove(&path);
        state.loaded_at = None;
        Ok(())
    }

    pub(crate) fn mkdir(&self, parent: u64, name: &str) -> Result<FileAttr, Errno> {
        let mut state = self.fresh()?;
        let path = self.name_path(&state, parent, name)?;
        if state.kind(&path).is_some() {
            return Err(Errno::EEXIST);
        }
        if !is_valid_path(&path) {
            return Err(Errno::EINVAL);
        }
        state.dirs.insert(path.clone());
        self.attr(&mut state, &path)
    }

    pub(crate) fn rmdir(&self, parent: u64, name: &str) -> Result<(), Errno> {
        let mut state = self.fresh()?;
        let path = self.name_path(&state, parent, name)?;
        match state.kind(&path) {
            Some(Kind::Dir) => {}
            Some(Kind::File) => return Err(Errno::ENOTDIR),
            None => return Err(Errno::ENOENT),
        }
        if state.has_children(&path) {
            return Err(Errno::ENOTEMPTY);
        }
        state.dirs.remove(&path);
        Ok(())
    }

    pub(crate) fn rename(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
        no_replace: bool,
    ) -> Result<(), Errno> {
        let (from, to, src, backend_move) = {
            let state = self.fresh()?;
            let from = self.name_path(&state, parent, name)?;
            let to = self.name_path(&state, new_parent, new_name)?;
            if from == to {
                return Ok(());
            }
            let src = state.kind(&from).ok_or(Errno::ENOENT)?;
            if !is_valid_path(&to) {
                return Err(Errno::EINVAL);
            }
            if src == Kind::Dir && under(&to, &from) {
                return Err(Errno::EINVAL);
            }
            match (src, state.kind(&to)) {
                (_, Some(_)) if no_replace => return Err(Errno::EEXIST),
                (Kind::File, Some(Kind::Dir)) => return Err(Errno::EISDIR),
                (Kind::Dir, Some(Kind::File)) => return Err(Errno::ENOTDIR),
                (Kind::Dir, Some(Kind::Dir)) if state.has_children(&to) => {
                    return Err(Errno::ENOTEMPTY)
                }
                _ => {}
            }
            let backend_move = match src {
                Kind::File => state.files.contains_key(&from),
                Kind::Dir => state.files.keys().any(|p| under(p, &from)),
            };
            (from, to, src, backend_move)
        };
        if backend_move {
            self.backend.rename(&from, &to).map_err(errno)?;
        }
        let mut state = self.lock();
        // Content written to the destination is replaced.
        if src == Kind::File {
            if let Some(replaced) = state.staged.remove(&to) {
                replaced.meta().deleted = true;
            }
        }
        state.dirs.remove(&to);
        let staged: Vec<(String, Arc<Staged>)> = state
            .staged
            .iter()
            .filter(|(p, _)| moved(p, &from, &to).is_some())
            .map(|(p, s)| (p.clone(), s.clone()))
            .collect();
        for (old, s) in staged {
            let new = moved(&old, &from, &to).expect("filtered");
            state.staged.remove(&old);
            s.meta().path = new.clone();
            state.staged.insert(new, s);
        }
        let dirs: Vec<String> = state
            .dirs
            .iter()
            .filter(|p| moved(p, &from, &to).is_some())
            .cloned()
            .collect();
        for old in dirs {
            state.dirs.remove(&old);
            state
                .dirs
                .insert(moved(&old, &from, &to).expect("filtered"));
        }
        let files: Vec<String> = state
            .files
            .keys()
            .filter(|p| moved(p, &from, &to).is_some())
            .cloned()
            .collect();
        if src == Kind::File && backend_move {
            state.files.remove(&to);
        }
        for old in files {
            if let Some(mut f) = state.files.remove(&old) {
                f.path = moved(&old, &from, &to).expect("filtered");
                state.files.insert(f.path.clone(), f);
            }
        }
        state.inodes.rename(&from, &to);
        state.loaded_at = None;
        Ok(())
    }
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(SystemTime::now())
}
