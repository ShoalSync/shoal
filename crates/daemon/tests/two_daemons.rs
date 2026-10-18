//! The Phase 2 exit criterion: two `tidefelld` processes sync a folder end to end.

use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tidefelld");

fn tidefelld(args: &[&str]) -> String {
    let out = Command::new(BIN).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "tidefelld {args:?} failed: {}",
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

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Node {
    dir: PathBuf,
    port: u16,
    web_port: u16,
    metrics_port: u16,
    device: String,
}

impl Node {
    fn folder(&self) -> PathBuf {
        self.dir.join("folder")
    }

    fn start(&self, folder_id: &str, user: &str, peer: &Node) -> Daemon {
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

[web]
bind = "127.0.0.1:{web_port}"
secure_cookies = false

[metrics]
listen = "127.0.0.1:{metrics_port}"
"#,
            data = self.dir.join("data").display(),
            port = self.port,
            peer_device = peer.device,
            peer_port = peer.port,
            web_port = self.web_port,
            metrics_port = self.metrics_port,
            folder = self.folder().display(),
        );
        let path = self.dir.join("tidefelld.toml");
        fs::write(&path, config).unwrap();
        tidefelld(&["config", "check", path.to_str().unwrap()]);
        // Appended to, so a restarted daemon's log follows the earlier one's.
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("daemon.log"))
            .unwrap();
        Daemon(
            Command::new(BIN)
                .args(["run", "--config", path.to_str().unwrap()])
                .env("RUST_LOG", "info")
                // The CI runner may run under systemd; the log file is tested here.
                .env_remove("JOURNAL_STREAM")
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .unwrap(),
        )
    }
}

fn init(root: &Path, name: &str, user_key: &Path) -> Node {
    let dir = root.join(name);
    let out = tidefelld(&[
        "init",
        "--data-dir",
        dir.join("data").to_str().unwrap(),
        "--name",
        name,
        "--user-key",
        user_key.to_str().unwrap(),
    ]);
    let tcp_port = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    Node {
        dir,
        port: free_port(),
        web_port: tcp_port(),
        metrics_port: tcp_port(),
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

/// The response to a plain HTTP GET, head and body.
fn http_get(port: u16, path: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn two_daemons_sync_a_folder() {
    let root = tempfile::tempdir().unwrap();
    let user_key = root.path().join("user.key");
    let a = init(root.path(), "a", &user_key);
    let b = init(root.path(), "b", &user_key);
    let user = field(
        &tidefelld(&["id", "--data-dir", a.dir.join("data").to_str().unwrap()]),
        "user",
    );
    let folder_id = tidefelld(&["new-folder-id"]).trim().to_string();

    fs::create_dir_all(a.folder()).unwrap();
    fs::write(a.folder().join("hello.txt"), b"hello from a").unwrap();

    let _da = a.start(&folder_id, &user, &b);
    let _db = b.start(&folder_id, &user, &a);

    wait_for("a's file on b", &[&a, &b], || {
        fs::read(b.folder().join("hello.txt")).ok().as_deref() == Some(b"hello from a".as_slice())
    });
    // Each daemon keeps JSON lines in its data directory (ADR 0030).
    let log = fs::read_to_string(a.dir.join("data/logs/tidefell.log")).unwrap();
    assert!(
        log.lines().any(|l| l.contains("\"tidefelld running\"")),
        "{log}"
    );

    // Prometheus metrics count the sync (ADR 0030).
    let response = http_get(b.metrics_port, "/metrics");
    let (head, page) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/openmetrics-text; version=1.0.0"),
        "{head}"
    );
    assert!(page.ends_with("# EOF\n"), "{page}");
    let received = page
        .lines()
        .find_map(|l| l.strip_prefix(r#"tidefell_content_received_bytes_total{network="local"} "#))
        .unwrap();
    assert!(received.parse::<u64>().unwrap() >= 12, "{page}");

    fs::create_dir_all(b.folder().join("sub")).unwrap();
    fs::write(b.folder().join("sub/reply.txt"), b"reply from b").unwrap();
    wait_for("b's file on a", &[&a, &b], || {
        fs::read(a.folder().join("sub/reply.txt")).ok().as_deref()
            == Some(b"reply from b".as_slice())
    });

    fs::remove_file(a.folder().join("hello.txt")).unwrap();
    wait_for("delete on b", &[&a, &b], || {
        !b.folder().join("hello.txt").exists()
    });

    // Each daemon serves the control API on its socket.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let client = tidefell_sdk::Client::unix(b.dir.join("data/control.sock"));
        let status = client.status().await.unwrap();
        assert_eq!(status.device, b.device);
        assert_eq!(status.user, user);
        let folders = client.folders().await.unwrap();
        assert_eq!(folders.len(), 1);
        assert!(folders[0].configured);
        let recycled = client.recycle_bin(&folder_id).await.unwrap();
        assert_eq!(recycled[0].path, "hello.txt");
    });

    // The web peer: create an account with the CLI, then sign in over HTTP.
    let mut add = Command::new(BIN)
        .args([
            "web",
            "account",
            "add",
            "--data-dir",
            b.dir.join("data").to_str().unwrap(),
            "--username",
            "alice",
            "--admin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    add.stdin
        .take()
        .unwrap()
        .write_all(b"a long enough password\n")
        .unwrap();
    let out = add.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(&user));

    let login_page = http(
        b.web_port,
        "GET /login HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(login_page.starts_with("HTTP/1.1 200"), "{login_page}");
    let body = "username=alice&password=a+long+enough+password";
    let signed_in = http(
        b.web_port,
        &format!(
            "POST /login HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(signed_in.starts_with("HTTP/1.1 303"), "{signed_in}");
    assert!(signed_in
        .to_lowercase()
        .contains("set-cookie: tidefell_session="));
}

/// Sends one raw HTTP/1.1 request and returns the whole response.
fn http(port: u16, request: &str) -> String {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// Phase 4.3: a new daemon pairs with a running one that holds the user key, then joins its
/// folder as a partial replica and fetches a file.
#[test]
fn a_new_daemon_pairs_then_syncs() {
    use std::io::{BufRead as _, BufReader};

    let root = tempfile::tempdir().unwrap();
    // `init` writes the user key into the approver's data directory, so it can approve.
    let a_dir = root.path().join("a");
    let out = tidefelld(&[
        "init",
        "--data-dir",
        a_dir.join("data").to_str().unwrap(),
        "--name",
        "laptop",
    ]);
    let a = Node {
        dir: a_dir.clone(),
        port: free_port(),
        web_port: 0,
        metrics_port: 0,
        device: field(&out, "device"),
    };
    let user = field(
        &tidefelld(&["id", "--data-dir", a.dir.join("data").to_str().unwrap()]),
        "user",
    );
    let folder_id = tidefelld(&["new-folder-id"]).trim().to_string();
    fs::create_dir_all(a.folder()).unwrap();
    fs::write(a.folder().join("beach.jpg"), b"sand and sea").unwrap();
    let config = format!(
        r#"
data_dir = "{data}"

[network]
relay = {{ mode = "disabled" }}
address_lookup = false
local_discovery = false
bind = "127.0.0.1:{port}"

[[folders]]
id = "{folder_id}"
path = "{folder}"
members = ["{user}"]
retention_days = 30
scan_interval_secs = 1
"#,
        data = a.dir.join("data").display(),
        port = a.port,
        folder = a.folder().display(),
    );
    fs::write(a.dir.join("tidefelld.toml"), config).unwrap();
    let log = fs::File::create(a.dir.join("daemon.log")).unwrap();
    let _da = Daemon(
        Command::new(BIN)
            .args([
                "run",
                "--config",
                a.dir.join("tidefelld.toml").to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );

    // The new device prints a code and waits.
    let b_dir = root.path().join("b");
    let mut pairing = Command::new(BIN)
        .args([
            "pair",
            "--data-dir",
            b_dir.join("data").to_str().unwrap(),
            "--name",
            "phone",
            "--no-relay",
            "--bind",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(pairing.stdout.take().unwrap()).lines();
    let mut next_line = || lines.next().unwrap().unwrap();
    let b_device = next_line().strip_prefix("device: ").unwrap().to_string();
    let code = next_line().strip_prefix("code: ").unwrap().to_string();
    assert!(code.starts_with("TFP1-"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = tidefell_sdk::Client::unix(a.dir.join("data/control.sock"));
    wait_for("the approver's control socket", &[&a], || {
        a.dir.join("data/control.sock").exists()
    });
    let pending = runtime.block_on(client.pair(&code)).unwrap();
    assert_eq!(pending.suggested_name, "phone");
    assert_eq!(pending.device, b_device);
    let confirmation = next_line();
    assert_eq!(
        confirmation,
        format!("confirmation: {}", pending.confirmation_code)
    );
    runtime
        .block_on(client.approve_pairing(&pending.id, Some("Alice's phone")))
        .unwrap();
    assert_eq!(next_line(), format!("user: {user}"));
    assert_eq!(next_line(), "name: Alice's phone");
    assert_eq!(next_line(), format!("approver: {}", a.device));
    assert_eq!(next_line(), format!("folder: {folder_id} folder"));
    assert!(pairing.wait().unwrap().success());

    // The paired device runs, keeps the approver connected, joins and fetches.
    let b = Node {
        dir: b_dir,
        port: free_port(),
        web_port: 0,
        metrics_port: 0,
        device: b_device,
    };
    let config = format!(
        r#"
data_dir = "{data}"

[network]
relay = {{ mode = "disabled" }}
address_lookup = false
local_discovery = false
bind = "127.0.0.1:{port}"
"#,
        data = b.dir.join("data").display(),
        port = b.port,
    );
    fs::write(b.dir.join("tidefelld.toml"), config).unwrap();
    let log = fs::File::create(b.dir.join("daemon.log")).unwrap();
    let _db = Daemon(
        Command::new(BIN)
            .args([
                "run",
                "--config",
                b.dir.join("tidefelld.toml").to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    wait_for("the new device's control socket", &[&a, &b], || {
        b.dir.join("data/control.sock").exists()
    });
    let phone = tidefell_sdk::Client::unix(b.dir.join("data/control.sock"));
    runtime.block_on(async {
        let known = phone.known_peers().await.unwrap();
        assert_eq!(known[0].device, a.device);
        phone
            .add_folder(&tidefell_sdk::types::NewFolder {
                id: Some(folder_id.clone()),
                path: b.folder().display().to_string(),
                members: Vec::new(),
                retention_days: 30,
                mode: Some(tidefell_sdk::types::Mode::Partial),
                budget_bytes: Some(1_000_000),
                scan_interval_secs: Some(1),
                sync: Default::default(),
                encryption: None,
            })
            .await
            .unwrap();
    });
    wait_for("the phone lists the file", &[&a, &b], || {
        runtime
            .block_on(phone.files(&folder_id))
            .is_ok_and(|f| f.len() == 1)
    });
    assert!(!b.folder().join("beach.jpg").exists(), "a placeholder");
    let fetched = runtime
        .block_on(phone.fetch(&folder_id, "beach.jpg"))
        .unwrap();
    assert_eq!(fs::read(fetched.local_path).unwrap(), b"sand and sea");
}

/// The files at `dir` and below, by path relative to it.
fn files_under(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut found = std::collections::BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".tidefell") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(data) = fs::read(&path) {
                let rel = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                found.insert(rel, data);
            }
        }
    }
    found
}

/// ADR 0034's exit criterion: killing tidefelld during a first sync loses nothing when it
/// restarts.
#[test]
fn killed_during_a_first_sync_loses_nothing() {
    const FILES: usize = 1500;
    let root = tempfile::tempdir().unwrap();
    let user_key = root.path().join("user.key");
    let a = init(root.path(), "a", &user_key);
    let b = init(root.path(), "b", &user_key);
    let user = field(
        &tidefelld(&["id", "--data-dir", a.dir.join("data").to_str().unwrap()]),
        "user",
    );
    let folder_id = tidefelld(&["new-folder-id"]).trim().to_string();
    let mut expected = std::collections::BTreeMap::new();
    for n in 0..FILES {
        let rel = format!("d{}/file-{n:04}.txt", n % 20);
        let data = format!("content of file {n}, {}", "x".repeat(n % 97)).into_bytes();
        let path = a.folder().join(&rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &data).unwrap();
        expected.insert(rel, data);
    }
    let _da = a.start(&folder_id, &user, &b);

    // Killed twice: early in the first sync, and well into it.
    for at_least in [50, FILES / 2] {
        let db = b.start(&folder_id, &user, &a);
        wait_for("part of the first sync", &[&a, &b], || {
            files_under(&b.folder()).len() >= at_least
        });
        drop(db);
    }
    let _db = b.start(&folder_id, &user, &a);
    wait_for("every file on b after restarting", &[&a, &b], || {
        files_under(&b.folder()) == expected
    });
    // Nothing doubled, no conflicts, and a's files unchanged.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(files_under(&b.folder()), expected);
    assert_eq!(files_under(&a.folder()), expected);
}

/// ADR 0034: a daemon whose folder database is damaged starts, rebuilds the folder from the
/// files on disk and its peer's index, and carries on rebuilding if killed meanwhile: every file
/// once, on both sides.
#[test]
fn a_damaged_database_is_rebuilt_even_when_killed_meanwhile() {
    const FILES: usize = 600;
    let root = tempfile::tempdir().unwrap();
    let user_key = root.path().join("user.key");
    let a = init(root.path(), "a", &user_key);
    let b = init(root.path(), "b", &user_key);
    let user = field(
        &tidefelld(&["id", "--data-dir", a.dir.join("data").to_str().unwrap()]),
        "user",
    );
    let folder_id = tidefelld(&["new-folder-id"]).trim().to_string();
    let mut expected = std::collections::BTreeMap::new();
    for n in 0..FILES {
        let rel = format!("d{}/file-{n:04}.txt", n % 20);
        let data = format!("content of file {n}, {}", "x".repeat(n % 97)).into_bytes();
        let path = a.folder().join(&rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &data).unwrap();
        expected.insert(rel, data);
    }
    let da = a.start(&folder_id, &user, &b);
    let db = b.start(&folder_id, &user, &a);
    wait_for("the first sync", &[&a, &b], || {
        files_under(&b.folder()) == expected
    });
    // Let the last commits land, then stop both and damage b's database.
    std::thread::sleep(Duration::from_secs(2));
    drop(db);
    drop(da);
    let database = b
        .dir
        .join("data/metadata")
        .join(format!("{folder_id}.redb"));
    let length = fs::metadata(&database).unwrap().len() as usize;
    fs::write(&database, vec![0xab; length]).unwrap();

    // With a stopped, b's rebuild waits for it; b is killed meanwhile.
    let log = || fs::read_to_string(b.dir.join("daemon.log")).unwrap_or_default();
    let db = b.start(&folder_id, &user, &a);
    wait_for("the rebuild to start", &[&a, &b], || {
        log().contains("rebuilding the folder")
    });
    std::thread::sleep(Duration::from_secs(2));
    drop(db);
    assert!(
        !log().contains("folder rebuilt"),
        "nothing to rebuild from yet"
    );

    // The rebuild carries on after the restart, and ends with a's index.
    let _da = a.start(&folder_id, &user, &b);
    let _db = b.start(&folder_id, &user, &a);
    wait_for("the rebuild to finish", &[&a, &b], || {
        log().contains("folder rebuilt")
    });
    std::thread::sleep(Duration::from_secs(3));
    // Every file once, none fetched again: each was matched to its entry where it was.
    assert_eq!(files_under(&b.folder()), expected);
    assert_eq!(files_under(&a.folder()), expected);
    let log = log();
    let since = &log[log.find("rebuilding the folder").unwrap()..];
    assert!(
        !since.contains("updated"),
        "files were written again:\n{since}"
    );
    let a_log = fs::read_to_string(a.dir.join("daemon.log")).unwrap_or_default();
    assert!(!a_log.contains("conflict"), "{a_log}");
}
