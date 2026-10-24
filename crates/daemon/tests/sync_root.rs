//! A partial replica's folder is a Windows sync root while `shoald` runs (ADR 0042): its
//! files not on this device are placeholders that fill when read, changes made there sync, and
//! the roots are disconnected when the daemon stops and connected again when it starts. A
//! partial folder added at runtime becomes a sync root too, and stops being one when removed.
#![cfg(windows)]

use std::fs;
use std::net::UdpSocket;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use shoal_sdk::types::{Mode, NewFolder};
use shoal_sdk::Client;

/// Set on a placeholder whose data is not on disk.
const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

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

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// A running daemon, killed if not stopped.
struct Daemon {
    child: Child,
    client: Client,
}

impl Daemon {
    /// Asks the daemon to stop through its API, as the tray app does, and waits for it.
    fn stop(mut self) {
        block_on(self.client.shutdown()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "shoald exited with {status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("shoald did not stop");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Node {
    dir: PathBuf,
    folder: PathBuf,
    port: u16,
    device: String,
}

impl Node {
    fn data(&self) -> PathBuf {
        self.dir.join("data")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.dir.join("daemon.log")).unwrap_or_default()
    }

    fn start(&self, folder_id: &str, user: &str, peer: &Node, extra: &str) -> Daemon {
        // TOML literal strings: Windows paths keep their backslashes.
        let config = format!(
            r#"
data_dir = '{data}'

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
path = '{folder}'
members = ["{user}"]
retention_days = 30
scan_interval_secs = 1
{extra}
"#,
            data = self.data().display(),
            port = self.port,
            peer_device = peer.device,
            peer_port = peer.port,
            folder = self.folder.display(),
        );
        let path = self.dir.join("shoald.toml");
        fs::write(&path, config).unwrap();
        shoald(&["config", "check", path.to_str().unwrap()]);
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("daemon.log"))
            .unwrap();
        let child = Command::new(bin())
            .args(["run", "--config", path.to_str().unwrap()])
            .env("RUST_LOG", "info,shoal_fs_cfapi=debug")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        Daemon {
            child,
            client: Client::local(Client::socket_path(&self.data())),
        }
    }
}

fn init(dir: PathBuf, folder: PathBuf, name: &str, user_key: &Path) -> Node {
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
        folder,
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
        eprintln!("--- {} log ---\n{}", node.dir.display(), node.log());
    }
    panic!("timed out waiting for {what}");
}

fn is_placeholder(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_attributes() & RECALL_ON_DATA_ACCESS != 0)
}

#[test]
fn a_partial_folder_is_a_sync_root_while_the_daemon_runs() {
    let tmp = tempfile::tempdir().unwrap();
    // Outside the profile's hidden AppData, where Explorer would not show a sync root.
    let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
    let roots = tempfile::Builder::new()
        .prefix("Shoal daemon ")
        .tempdir_in(profile)
        .unwrap();
    let user_key = tmp.path().join("user.key");
    let nas = init(
        tmp.path().join("nas"),
        tmp.path().join("nas-folder"),
        "nas",
        &user_key,
    );
    let laptop = init(
        tmp.path().join("laptop"),
        roots.path().join("Shoal"),
        "laptop",
        &user_key,
    );
    let user = field(
        &shoald(&["id", "--data-dir", nas.data().to_str().unwrap()]),
        "user",
    );
    let folder_id = shoald(&["new-folder-id"]).trim().to_string();
    fs::create_dir_all(nas.folder.join("docs")).unwrap();
    fs::write(nas.folder.join("beach.jpg"), b"sand and sea").unwrap();
    fs::write(nas.folder.join("docs/notes.txt"), b"first draft").unwrap();

    let _nas_daemon = nas.start(&folder_id, &user, &laptop, "");
    let partial = "mode = \"partial\"\nbudget_bytes = 1000000";
    let laptop_daemon = laptop.start(&folder_id, &user, &nas, partial);
    let nodes = [&nas, &laptop];

    // Every file is a placeholder in the folder itself.
    wait_for("placeholders of the NAS's files", &nodes, || {
        is_placeholder(&laptop.folder.join("beach.jpg"))
            && is_placeholder(&laptop.folder.join("docs/notes.txt"))
    });
    assert_eq!(
        fs::symlink_metadata(laptop.folder.join("beach.jpg"))
            .unwrap()
            .len(),
        12
    );

    // Reading one fills it.
    assert_eq!(
        fs::read(laptop.folder.join("beach.jpg")).unwrap(),
        b"sand and sea"
    );
    assert!(!is_placeholder(&laptop.folder.join("beach.jpg")));

    // Changes made there sync.
    fs::write(laptop.folder.join("docs/new.txt"), b"written on the laptop").unwrap();
    wait_for("the NAS has the new file", &nodes, || {
        fs::read(nas.folder.join("docs/new.txt")).ok().as_deref()
            == Some(b"written on the laptop".as_slice())
    });
    fs::remove_file(laptop.folder.join("docs/new.txt")).unwrap();
    wait_for("the NAS deletes it", &nodes, || {
        !nas.folder.join("docs/new.txt").exists()
    });

    // Changes on the NAS show up as placeholders.
    fs::write(nas.folder.join("from-nas.txt"), b"hello laptop").unwrap();
    wait_for("a placeholder of the NAS's new file", &nodes, || {
        is_placeholder(&laptop.folder.join("from-nas.txt"))
    });

    // A partial folder added at runtime is a sync root too, until it is removed.
    let added = block_on(laptop_daemon.client.add_folder(&NewFolder {
        id: None,
        path: roots.path().join("Added").display().to_string(),
        members: Vec::new(),
        retention_days: 30,
        mode: Some(Mode::Partial),
        budget_bytes: Some(1_000_000),
        scan_interval_secs: None,
        sync: Default::default(),
        encryption: None,
    }))
    .unwrap();
    wait_for("the added folder's sync root", &nodes, || {
        laptop.log().matches("sync root connected").count() >= 2
    });
    block_on(laptop_daemon.client.remove_folder(&added.id)).unwrap();
    wait_for("the removed folder's registration to go", &nodes, || {
        laptop.log().contains("sync root unregistered")
    });

    // A clean stop disconnects every root before the engine stops.
    laptop_daemon.stop();
    let log = laptop.log();
    assert!(log.contains("sync root disconnected"), "{log}");
    assert!(!log.contains("cannot unregister"), "{log}");

    // While the daemon is stopped, a placeholder deleted is a delete, found when it starts.
    fs::remove_file(laptop.folder.join("from-nas.txt")).unwrap();
    let laptop_daemon = laptop.start(&folder_id, &user, &nas, partial);
    wait_for("the NAS deletes what went while away", &nodes, || {
        !nas.folder.join("from-nas.txt").exists()
    });
    // And the root is connected again, in the background: a placeholder still fills.
    wait_for("the sync root connected again", &nodes, || {
        laptop.log().matches("sync root connected").count() >= 3
    });
    assert_eq!(
        fs::read(laptop.folder.join("docs/notes.txt")).unwrap(),
        b"first draft"
    );
    laptop_daemon.stop();
}
