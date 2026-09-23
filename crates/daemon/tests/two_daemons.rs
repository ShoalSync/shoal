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
"#,
            data = self.dir.join("data").display(),
            port = self.port,
            peer_device = peer.device,
            peer_port = peer.port,
            web_port = self.web_port,
            folder = self.folder().display(),
        );
        let path = self.dir.join("tidefelld.toml");
        fs::write(&path, config).unwrap();
        tidefelld(&["config", "check", path.to_str().unwrap()]);
        let log = fs::File::create(self.dir.join("daemon.log")).unwrap();
        Daemon(
            Command::new(BIN)
                .args(["run", "--config", path.to_str().unwrap()])
                .env("RUST_LOG", "info")
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
    let web_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    Node {
        dir,
        port: free_port(),
        web_port,
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
