//! A real FUSE mount over a folder kept in a directory: ordinary file operations through the
//! kernel. Needs `/dev/fuse` (and `fusermount3` unless run as root); without them the test
//! is skipped, unless `SHOAL_REQUIRE_FUSE` is set, as in CI.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use shoal_fs_fuse::{mount, Backend, BackendError, FileEntry};

struct DirBackend {
    dir: PathBuf,
    files: Mutex<BTreeMap<String, ()>>,
    opened: Mutex<Vec<String>>,
}

impl Backend for DirBackend {
    fn files(&self) -> Result<Vec<FileEntry>, BackendError> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .keys()
            .map(|path| {
                let meta = fs::metadata(self.dir.join(path)).unwrap();
                FileEntry {
                    path: path.clone(),
                    size: meta.len(),
                    modified_ms: 1_700_000_000_000,
                    executable: false,
                }
            })
            .collect())
    }

    fn open(&self, path: &str) -> Result<PathBuf, BackendError> {
        self.opened.lock().unwrap().push(path.to_owned());
        Ok(self.dir.join(path))
    }

    fn close(&self, _path: &str) {}

    fn write(&self, path: &str, source: &Path) -> Result<(), BackendError> {
        let dst = self.dir.join(path);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::rename(source, dst).unwrap();
        self.files.lock().unwrap().insert(path.to_owned(), ());
        Ok(())
    }

    fn delete(&self, path: &str) -> Result<(), BackendError> {
        self.files.lock().unwrap().remove(path);
        fs::remove_file(self.dir.join(path)).map_err(|e| BackendError::Other(e.to_string()))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), BackendError> {
        let mut files = self.files.lock().unwrap();
        files.remove(from).ok_or(BackendError::NotFound)?;
        files.insert(to.to_owned(), ());
        fs::rename(self.dir.join(from), self.dir.join(to))
            .map_err(|e| BackendError::Other(e.to_string()))
    }
}

/// True when running as root: `/proc/self` belongs to the process's effective user.
fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

fn fuse_available() -> bool {
    let usable = Path::new("/dev/fuse").exists()
        && (is_root()
            || std::process::Command::new("fusermount3")
                .arg("--version")
                .output()
                .is_ok());
    if !usable && std::env::var_os("SHOAL_REQUIRE_FUSE").is_some() {
        panic!("SHOAL_REQUIRE_FUSE is set but FUSE is not usable here");
    }
    usable
}

#[test]
fn file_operations_through_the_kernel() {
    if !fuse_available() {
        eprintln!("skipping: FUSE is not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("folder");
    fs::create_dir_all(folder.join("docs")).unwrap();
    fs::write(folder.join("a.txt"), "alpha").unwrap();
    fs::write(folder.join("docs/b.txt"), "bee").unwrap();
    let backend = DirBackend {
        dir: folder.clone(),
        files: Mutex::new(
            [("a.txt".to_owned(), ()), ("docs/b.txt".to_owned(), ())]
                .into_iter()
                .collect(),
        ),
        opened: Mutex::new(Vec::new()),
    };
    let mnt = tmp.path().join("mnt");
    let mount = mount(backend, &mnt, tmp.path().join("staging")).unwrap();

    let mut names: Vec<String> = fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "docs"]);
    assert_eq!(fs::metadata(mnt.join("a.txt")).unwrap().len(), 5);
    assert_eq!(fs::read_to_string(mnt.join("docs/b.txt")).unwrap(), "bee");

    // Write a new file, edit one, save by rename, delete, and make directories.
    fs::write(mnt.join("new.txt"), "hello").unwrap();
    assert_eq!(fs::read_to_string(folder.join("new.txt")).unwrap(), "hello");
    fs::write(mnt.join("a.txt"), "ALPHA!").unwrap();
    assert_eq!(fs::read_to_string(folder.join("a.txt")).unwrap(), "ALPHA!");
    fs::write(mnt.join(".b.tmp"), "saved").unwrap();
    fs::rename(mnt.join(".b.tmp"), mnt.join("docs/b.txt")).unwrap();
    assert_eq!(
        fs::read_to_string(folder.join("docs/b.txt")).unwrap(),
        "saved"
    );
    assert_eq!(fs::read_to_string(mnt.join("docs/b.txt")).unwrap(), "saved");
    fs::remove_file(mnt.join("new.txt")).unwrap();
    assert!(!folder.join("new.txt").exists());
    assert!(!mnt.join("new.txt").exists());
    fs::create_dir(mnt.join("empty")).unwrap();
    assert!(mnt.join("empty").is_dir());
    fs::remove_dir(mnt.join("empty")).unwrap();
    assert!(fs::remove_dir(mnt.join("docs")).is_err());

    mount.unmount().unwrap();
    assert!(fs::read_dir(&mnt).unwrap().next().is_none(), "unmounted");
}
