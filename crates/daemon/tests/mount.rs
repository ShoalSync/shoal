//! The Phase 4b exit criterion: a partial replica mounts a folder with FUSE, lists every
//! file, fetches placeholders when they are read, and syncs changes made through the mount.
//! Needs `/dev/fuse` (and `fusermount3` unless run as root); without them the test is
//! skipped, unless `SHOAL_REQUIRE_FUSE` is set, as in CI.
#![cfg(target_os = "linux")]

use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The shoald this test runs: the one built with it, unless `SHOALD` names another, as
/// when CI runs this test built elsewhere.
fn bin() -> String {
    std::env::var("SHOALD").unwrap_or_else(|_| env!("CARGO_BIN_EXE_shoald").to_owned())
}

fn shoald(args: &[&str]) -> String {
    let out = Command::new(bin()).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "shoald {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn field(output: &str, name: &str) -> String {
    output
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}:")))
        .unwrap()
        .trim()
        .to_string()
}

fn fuse_available() -> bool {
    let root = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| o.stdout.trim_ascii() == b"0");
    let usable = Path::new("/dev/fuse").exists()
        && (root
            || Command::new("fusermount3")
                .arg("--version")
                .output()
                .is_ok());
    if !usable && std::env::var_os("SHOAL_REQUIRE_FUSE").is_some() {
        panic!("SHOAL_REQUIRE_FUSE is set but FUSE is not usable here");
    }
    usable
}

/// A running daemon; stopped with SIGINT so it unmounts, or killed outright.
struct Daemon(Child);

impl Daemon {
    fn stop(mut self) {
        let _ = Command::new("kill")
            .args(["-INT", &self.0.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.0.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("shoald did not stop");
    }

    fn kill(mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Node {
    dir: PathBuf,
    port: u16,
    device: String,
}

impl Node {
    fn folder(&self) -> PathBuf {
        self.dir.join("folder")
    }

    fn mnt(&self) -> PathBuf {
        self.dir.join("mnt")
    }

    fn start(&self, folder_id: &str, user: &str, peer: &Node, extra: &str) -> Daemon {
        let config = format!(
            r#"
data_dir = "{data}"

[network]
relay = {{ mode = "disabled" }}
address_lookup = false
local_discovery = false
bind = "127.0.0.1:{port}"

[[network.peers]]
device = "{peer_device}"
addrs = ["127.0.0.1:{peer_port}"]

[[folders]]
id = "{folder_id}"
path = "{folder}"
members = ["{user}"]
retention_days = 30
scan_interval_secs = 1
{extra}
"#,
            data = self.dir.join("data").display(),
            port = self.port,
            peer_device = peer.device,
            peer_port = peer.port,
            folder = self.folder().display(),
        );
        let path = self.dir.join("shoald.toml");
        fs::write(&path, config).unwrap();
        shoald(&["config", "check", path.to_str().unwrap()]);
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("daemon.log"))
            .unwrap();
        Daemon(
            Command::new(bin())
                .args(["run", "--config", path.to_str().unwrap()])
                .env("RUST_LOG", "info,shoal_fs_fuse=debug")
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .unwrap(),
        )
    }
}

fn init(root: &Path, name: &str, user_key: &Path) -> Node {
    let dir = root.join(name);
    let out = shoald(&[
        "init",
        "--data-dir",
        dir.join("data").to_str().unwrap(),
        "--name",
        name,
        "--user-key",
        user_key.to_str().unwrap(),
    ]);
    Node {
        dir,
        port: UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port(),
        device: field(&out, "device"),
    }
}

fn wait_for(what: &str, nodes: &[&Node], mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for node in nodes {
        eprintln!(
            "--- {} log ---\n{}",
            node.dir.display(),
            fs::read_to_string(node.dir.join("daemon.log")).unwrap_or_default()
        );
    }
    panic!("timed out waiting for {what}");
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn partial_replica_mounts_a_folder() {
    if !fuse_available() {
        eprintln!("skipping: FUSE is not available");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let user_key = root.path().join("user.key");
    let nas = init(root.path(), "nas", &user_key);
    let laptop = init(root.path(), "laptop", &user_key);
    let user = field(
        &shoald(&["id", "--data-dir", nas.dir.join("data").to_str().unwrap()]),
        "user",
    );
    let folder_id = shoald(&["new-folder-id"]).trim().to_string();
    fs::create_dir_all(nas.folder().join("docs")).unwrap();
    fs::write(nas.folder().join("beach.jpg"), b"sand and sea").unwrap();
    fs::write(nas.folder().join("docs/notes.txt"), b"first draft").unwrap();

    let _nas_daemon = nas.start(&folder_id, &user, &laptop, "");
    let mount = format!(
        "mode = \"partial\"\nbudget_bytes = 1000000\nmount = \"{}\"",
        laptop.mnt().display()
    );
    let laptop_daemon = laptop.start(&folder_id, &user, &nas, &mount);
    let nodes = [&nas, &laptop];

    // Every file is listed; nothing is stored yet.
    wait_for("the mount lists the folder", &nodes, || {
        names(&laptop.mnt()) == ["beach.jpg", "docs"]
    });
    assert_eq!(
        fs::metadata(laptop.mnt().join("beach.jpg")).unwrap().len(),
        12
    );
    assert!(!laptop.folder().join("beach.jpg").exists());

    // Reading a placeholder fetches it.
    assert_eq!(
        fs::read(laptop.mnt().join("beach.jpg")).unwrap(),
        b"sand and sea"
    );
    assert_eq!(
        fs::read(laptop.folder().join("beach.jpg")).unwrap(),
        b"sand and sea"
    );

    // Changes made through the mount reach the NAS.
    fs::write(laptop.mnt().join("docs/new.txt"), b"written on the laptop").unwrap();
    wait_for("the NAS has the new file", &nodes, || {
        fs::read(nas.folder().join("docs/new.txt")).ok().as_deref()
            == Some(b"written on the laptop".as_slice())
    });
    fs::write(laptop.mnt().join("docs/.notes.tmp"), b"second draft").unwrap();
    fs::rename(
        laptop.mnt().join("docs/.notes.tmp"),
        laptop.mnt().join("docs/notes.txt"),
    )
    .unwrap();
    wait_for("the NAS has the saved notes", &nodes, || {
        fs::read(nas.folder().join("docs/notes.txt"))
            .ok()
            .as_deref()
            == Some(b"second draft".as_slice())
            && !nas.folder().join("docs/.notes.tmp").exists()
    });
    fs::rename(
        laptop.mnt().join("beach.jpg"),
        laptop.mnt().join("docs/beach.jpg"),
    )
    .unwrap();
    fs::remove_file(laptop.mnt().join("docs/new.txt")).unwrap();
    wait_for("the NAS moves and deletes", &nodes, || {
        nas.folder().join("docs/beach.jpg").exists()
            && !nas.folder().join("beach.jpg").exists()
            && !nas.folder().join("docs/new.txt").exists()
    });

    // Changes on the NAS show up in the mount.
    fs::write(nas.folder().join("from-nas.txt"), b"hello laptop").unwrap();
    wait_for("the mount shows the NAS's file", &nodes, || {
        fs::read(laptop.mnt().join("from-nas.txt")).ok().as_deref()
            == Some(b"hello laptop".as_slice())
    });

    // A clean stop unmounts.
    laptop_daemon.stop();
    assert!(names(&laptop.mnt()).is_empty(), "unmounted on shutdown");

    // A daemon killed without unmounting leaves a stale mount, cleared on the next start.
    let laptop_daemon = laptop.start(&folder_id, &user, &nas, &mount);
    wait_for("the mount is back", &nodes, || {
        !names(&laptop.mnt()).is_empty()
    });
    laptop_daemon.kill();
    let laptop_daemon = laptop.start(&folder_id, &user, &nas, &mount);
    wait_for("the mount recovers from a crash", &nodes, || {
        names(&laptop.mnt()).contains(&"from-nas.txt".to_owned())
    });
    laptop_daemon.stop();
}
