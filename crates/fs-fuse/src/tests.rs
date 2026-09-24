//! The filesystem's logic against an in-memory backend, without the kernel.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fuser::{Errno, FileType, INodeNo};

use crate::backend::{Backend, BackendError, FileEntry};
use crate::core::{Core, Kind};

const ROOT: u64 = INodeNo::ROOT.0;

/// A folder whose files live in a directory, recording what the filesystem asks of it.
struct Fake {
    dir: PathBuf,
    files: Mutex<BTreeMap<String, bool>>,
    unavailable: Mutex<Vec<String>>,
    log: Mutex<Vec<String>>,
    pins: Mutex<BTreeMap<String, i32>>,
}

impl Fake {
    fn new(dir: &Path, files: &[(&str, &str)]) -> Self {
        let fake = Self {
            dir: dir.to_owned(),
            files: Mutex::new(BTreeMap::new()),
            unavailable: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            pins: Mutex::new(BTreeMap::new()),
        };
        for (path, content) in files {
            let abs = dir.join(path);
            fs::create_dir_all(abs.parent().unwrap()).unwrap();
            fs::write(abs, content).unwrap();
            fake.files.lock().unwrap().insert(path.to_string(), false);
        }
        fake
    }

    fn content(&self, path: &str) -> Option<String> {
        if !self.files.lock().unwrap().contains_key(path) {
            return None;
        }
        Some(fs::read_to_string(self.dir.join(path)).unwrap())
    }

    fn log(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }
}

impl Backend for &'static Fake {
    fn files(&self) -> Result<Vec<FileEntry>, BackendError> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .iter()
            .map(|(path, executable)| FileEntry {
                path: path.clone(),
                size: fs::metadata(self.dir.join(path)).unwrap().len(),
                modified_ms: 1_000,
                executable: *executable,
            })
            .collect())
    }

    fn open(&self, path: &str) -> Result<PathBuf, BackendError> {
        if !self.files.lock().unwrap().contains_key(path) {
            return Err(BackendError::NotFound);
        }
        if self.unavailable.lock().unwrap().iter().any(|p| p == path) {
            return Err(BackendError::Unavailable);
        }
        self.log(format!("open {path}"));
        *self
            .pins
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default() += 1;
        Ok(self.dir.join(path))
    }

    fn close(&self, path: &str) {
        *self
            .pins
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default() -= 1;
    }

    fn write(&self, path: &str, source: &Path) -> Result<(), BackendError> {
        self.log(format!("write {path}"));
        let abs = self.dir.join(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(source).unwrap().permissions().mode() & 0o111 != 0
        };
        fs::rename(source, abs).unwrap();
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), executable);
        Ok(())
    }

    fn delete(&self, path: &str) -> Result<(), BackendError> {
        self.log(format!("delete {path}"));
        self.files
            .lock()
            .unwrap()
            .remove(path)
            .ok_or(BackendError::NotFound)?;
        fs::remove_file(self.dir.join(path)).unwrap();
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), BackendError> {
        self.log(format!("rename {from} {to}"));
        let mut files = self.files.lock().unwrap();
        let moves: Vec<(String, String)> = files
            .keys()
            .filter_map(|p| {
                if p == from {
                    Some((p.clone(), to.to_owned()))
                } else {
                    p.strip_prefix(&format!("{from}/"))
                        .map(|rest| (p.clone(), format!("{to}/{rest}")))
                }
            })
            .collect();
        if moves.is_empty() {
            return Err(BackendError::NotFound);
        }
        for (old, new) in moves {
            let exec = files.remove(&old).unwrap();
            files.insert(new.clone(), exec);
            let dst = self.dir.join(&new);
            fs::create_dir_all(dst.parent().unwrap()).unwrap();
            fs::rename(self.dir.join(&old), dst).unwrap();
        }
        Ok(())
    }
}

struct Setup {
    _tmp: tempfile::TempDir,
    fake: &'static Fake,
    core: Core<&'static Fake>,
}

fn setup(files: &[(&str, &str)]) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let fake: &'static Fake = Box::leak(Box::new(Fake::new(&tmp.path().join("folder"), files)));
    let core = Core::new(fake, tmp.path().join("staging"), 1000, 1000).unwrap();
    Setup {
        _tmp: tmp,
        fake,
        core,
    }
}

impl Setup {
    /// The inode of `path`, looked up component by component.
    fn ino(&self, path: &str) -> Result<u64, Errno> {
        let mut ino = ROOT;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            ino = self.core.lookup(ino, part)?.ino.0;
        }
        Ok(ino)
    }

    fn parent(&self, path: &str) -> (u64, String) {
        match path.rsplit_once('/') {
            Some((dir, name)) => (self.ino(dir).unwrap(), name.to_owned()),
            None => (ROOT, path.to_owned()),
        }
    }

    fn names(&self, dir: &str) -> Vec<(String, Kind)> {
        let ino = self.ino(dir).unwrap();
        self.core
            .readdir(ino)
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.kind))
            .collect()
    }

    fn read_all(&self, path: &str) -> Result<String, Errno> {
        let fh = self.core.open(self.ino(path)?, false, false)?;
        let data = self.core.read(fh, 0, 1 << 20)?;
        self.core.release(fh)?;
        Ok(String::from_utf8(data).unwrap())
    }

    /// Writes `data` like `echo data > path`.
    fn write_file(&self, path: &str, data: &str) {
        let fh = match self.ino(path) {
            Ok(ino) => self.core.open(ino, true, true).unwrap(),
            Err(_) => {
                let (parent, name) = self.parent(path);
                self.core.create(parent, &name, 0o644).unwrap().1
            }
        };
        self.core.write(fh, 0, data.as_bytes()).unwrap();
        self.core.sync(fh).unwrap();
        self.core.release(fh).unwrap();
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        let (p1, n1) = self.parent(from);
        let (p2, n2) = self.parent(to);
        self.core.rename(p1, &n1, p2, &n2, false)
    }

    fn log(&self) -> Vec<String> {
        std::mem::take(&mut *self.fake.log.lock().unwrap())
    }
}

#[test]
fn lists_placeholders_from_metadata() {
    let s = setup(&[
        ("a.txt", "alpha"),
        ("docs/b.txt", "bee"),
        ("docs/deep/c.txt", "c"),
    ]);
    assert_eq!(
        s.names(""),
        vec![
            ("a.txt".to_owned(), Kind::File),
            ("docs".to_owned(), Kind::Dir)
        ]
    );
    assert_eq!(
        s.names("docs"),
        vec![
            ("b.txt".to_owned(), Kind::File),
            ("deep".to_owned(), Kind::Dir)
        ]
    );
    let attr = s.core.getattr(s.ino("a.txt").unwrap()).unwrap();
    assert_eq!(attr.size, 5);
    assert_eq!(attr.kind, FileType::RegularFile);
    assert_eq!(attr.perm, 0o644);
    assert_eq!(
        s.core.getattr(s.ino("docs").unwrap()).unwrap().kind,
        FileType::Directory
    );
    assert_eq!(s.ino("nope").unwrap_err(), Errno::ENOENT);
    assert_eq!(
        s.core.readdir(s.ino("a.txt").unwrap()).unwrap_err(),
        Errno::ENOTDIR
    );
    // Listing and attributes never fetch content.
    assert!(s.log().is_empty());
}

#[test]
fn reading_fetches_and_pins_until_release() {
    let s = setup(&[("a.txt", "alpha")]);
    let ino = s.ino("a.txt").unwrap();
    let fh = s.core.open(ino, false, false).unwrap();
    assert_eq!(s.log(), ["open a.txt"]);
    assert_eq!(s.fake.pins.lock().unwrap()["a.txt"], 1);
    assert_eq!(s.core.read(fh, 1, 3).unwrap(), b"lph");
    assert_eq!(s.core.read(fh, 3, 100).unwrap(), b"ha");
    assert_eq!(s.core.write(fh, 0, b"x").unwrap_err(), Errno::EBADF);
    s.core.release(fh).unwrap();
    assert_eq!(s.fake.pins.lock().unwrap()["a.txt"], 0);

    s.fake.unavailable.lock().unwrap().push("a.txt".to_owned());
    assert_eq!(s.core.open(ino, false, false).unwrap_err(), Errno::EIO);
    assert_eq!(s.core.open(ROOT, false, false).unwrap_err(), Errno::EISDIR);
}

#[test]
fn writes_are_staged_and_recorded_on_flush() {
    let s = setup(&[("a.txt", "alpha")]);
    let ino = s.ino("a.txt").unwrap();

    // Writing in place keeps the rest of the content, fetched first.
    let fh = s.core.open(ino, true, false).unwrap();
    assert_eq!(s.log(), ["open a.txt"]);
    assert_eq!(
        s.fake.pins.lock().unwrap()["a.txt"],
        0,
        "staging does not keep a pin"
    );
    s.core.write(fh, 0, b"AL").unwrap();
    // Nothing is recorded until the file is flushed; readers see the staged content.
    assert_eq!(s.fake.content("a.txt").unwrap(), "alpha");
    assert_eq!(s.read_all("a.txt").unwrap(), "ALpha");
    s.core.sync(fh).unwrap();
    assert_eq!(s.log(), ["write a.txt"]);
    assert_eq!(s.fake.content("a.txt").unwrap(), "ALpha");
    // An unchanged flush records nothing.
    s.core.sync(fh).unwrap();
    s.core.release(fh).unwrap();
    assert!(s.log().is_empty());

    // Truncating skips the fetch.
    let fh = s.core.open(ino, true, true).unwrap();
    s.core.write(fh, 0, b"new").unwrap();
    s.core.release(fh).unwrap();
    assert_eq!(s.log(), ["write a.txt"]);
    assert_eq!(s.fake.content("a.txt").unwrap(), "new");
    assert_eq!(s.core.getattr(ino).unwrap().size, 3);
}

#[test]
fn new_files_and_editor_saves() {
    let s = setup(&[("notes.txt", "old")]);

    // A new, empty file is still recorded.
    let (attr, fh) = s.core.create(ROOT, "empty.txt", 0o644).unwrap();
    assert_eq!(attr.size, 0);
    assert!(s.names("").contains(&("empty.txt".to_owned(), Kind::File)));
    s.core.release(fh).unwrap();
    assert_eq!(s.fake.content("empty.txt").unwrap(), "");

    // Executable files keep the bit.
    let (_, fh) = s.core.create(ROOT, "run.sh", 0o755).unwrap();
    s.core.release(fh).unwrap();
    assert!(s.fake.files.lock().unwrap()["run.sh"]);
    assert_eq!(
        s.core.getattr(s.ino("run.sh").unwrap()).unwrap().perm,
        0o755
    );

    // Save by writing a temporary file and renaming it over the original.
    s.log();
    s.write_file(".notes.txt.tmp", "new");
    s.rename(".notes.txt.tmp", "notes.txt").unwrap();
    assert_eq!(
        s.log(),
        ["write .notes.txt.tmp", "rename .notes.txt.tmp notes.txt"]
    );
    assert_eq!(s.fake.content("notes.txt").unwrap(), "new");
    assert!(s.fake.content(".notes.txt.tmp").is_none());
    assert_eq!(s.read_all("notes.txt").unwrap(), "new");

    // Renaming a file that is still open and unrecorded moves where it will be recorded.
    let (_, fh) = s.core.create(ROOT, "draft", 0o644).unwrap();
    s.core.write(fh, 0, b"draft text").unwrap();
    s.rename("draft", "notes.txt").unwrap();
    assert_eq!(s.read_all("notes.txt").unwrap(), "draft text");
    s.core.release(fh).unwrap();
    assert_eq!(s.fake.content("notes.txt").unwrap(), "draft text");
    assert!(s.fake.content("draft").is_none());

    assert_eq!(
        s.core.create(ROOT, "bad\\name", 0o644).unwrap_err(),
        Errno::EINVAL
    );
}

#[test]
fn deletes_and_renames_go_to_the_folder() {
    let s = setup(&[
        ("a.txt", "a"),
        ("docs/b.txt", "b"),
        ("docs/c.txt", "c"),
        ("other.txt", "o"),
    ]);

    // Renaming placeholders never fetches them.
    s.rename("a.txt", "z.txt").unwrap();
    s.rename("docs", "archive").unwrap();
    assert_eq!(s.log(), ["rename a.txt z.txt", "rename docs archive"]);
    assert_eq!(s.names("archive").len(), 2);
    assert_eq!(s.ino("docs").unwrap_err(), Errno::ENOENT);
    assert_eq!(s.fake.content("archive/c.txt").unwrap(), "c");

    let (parent, _) = s.parent("archive/b.txt");
    s.core.unlink(parent, "b.txt").unwrap();
    assert_eq!(s.log(), ["delete archive/b.txt"]);
    assert_eq!(s.ino("archive/b.txt").unwrap_err(), Errno::ENOENT);
    assert_eq!(s.core.unlink(ROOT, "archive").unwrap_err(), Errno::EISDIR);
    assert_eq!(s.core.unlink(ROOT, "nope").unwrap_err(), Errno::ENOENT);

    // Kernel-visible rules.
    assert_eq!(s.rename("z.txt", "archive").unwrap_err(), Errno::EISDIR);
    assert_eq!(s.rename("archive", "z.txt").unwrap_err(), Errno::ENOTDIR);
    assert_eq!(
        s.rename("archive", "archive/inner").unwrap_err(),
        Errno::EINVAL
    );
    assert_eq!(
        s.core
            .rename(ROOT, "z.txt", ROOT, "other.txt", true)
            .unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(s.rename("missing", "x").unwrap_err(), Errno::ENOENT);
    assert!(s.log().is_empty());

    // Inode numbers follow renames.
    let ino = s.ino("z.txt").unwrap();
    s.rename("z.txt", "y.txt").unwrap();
    assert_eq!(s.ino("y.txt").unwrap(), ino);
}

#[test]
fn empty_directories_live_in_the_mount() {
    let s = setup(&[("a.txt", "a")]);
    let dir = s.core.mkdir(ROOT, "new").unwrap();
    assert_eq!(dir.kind, FileType::Directory);
    assert_eq!(s.core.mkdir(ROOT, "new").unwrap_err(), Errno::EEXIST);
    assert_eq!(s.core.mkdir(ROOT, "a.txt").unwrap_err(), Errno::EEXIST);
    assert!(s.names("").contains(&("new".to_owned(), Kind::Dir)));
    s.core.mkdir(dir.ino.0, "sub").unwrap();

    // Renaming an empty directory needs nothing from the folder.
    s.rename("new", "renamed").unwrap();
    assert!(s.log().is_empty());
    assert!(s.ino("renamed/sub").is_ok());

    assert_eq!(s.core.rmdir(ROOT, "renamed").unwrap_err(), Errno::ENOTEMPTY);
    let renamed = s.ino("renamed").unwrap();
    s.core.rmdir(renamed, "sub").unwrap();
    s.write_file("renamed/file.txt", "x");
    assert_eq!(s.fake.content("renamed/file.txt").unwrap(), "x");
    assert_eq!(s.core.rmdir(ROOT, "renamed").unwrap_err(), Errno::ENOTEMPTY);
    assert_eq!(s.core.rmdir(ROOT, "a.txt").unwrap_err(), Errno::ENOTDIR);
}

#[test]
fn setattr_truncates_and_sets_the_executable_bit() {
    let s = setup(&[("a.sh", "echo hi")]);
    let ino = s.ino("a.sh").unwrap();

    let attr = s
        .core
        .setattr(ino, Some(0o755), None, None, None, None)
        .unwrap();
    assert_eq!(attr.perm, 0o755);
    assert!(s.fake.files.lock().unwrap()["a.sh"]);
    assert_eq!(s.fake.content("a.sh").unwrap(), "echo hi");

    let attr = s
        .core
        .setattr(ino, None, None, None, Some(4), None)
        .unwrap();
    assert_eq!(attr.size, 4);
    assert_eq!(s.fake.content("a.sh").unwrap(), "echo");

    // Unchanged attributes and times record nothing; ownership cannot change.
    s.log();
    s.core
        .setattr(ino, Some(0o755), Some(1000), None, None, None)
        .unwrap();
    assert!(s.log().is_empty());
    assert_eq!(
        s.core
            .setattr(ino, None, Some(0), None, None, None)
            .unwrap_err(),
        Errno::EPERM
    );
}
