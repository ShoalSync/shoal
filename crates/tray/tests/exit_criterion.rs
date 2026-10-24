//! The Windows half of Phase 8's exit criterion (ADR 0042), with `scripts/exit-criterion/linux.sh`
//! on a Linux runner as the other: this device, set up as the first of the user's with a partial
//! folder, approves the Linux daemon as a new device through the tray's own code; the shell's
//! listing of the folder (Explorer's) shows the files it brings as placeholders; opening one
//! fetches its content; and the two stay connected, directly or through a relay: the runners
//! are separate virtual machines, which iroh may not punch through. Run by the exit-criterion
//! workflow, which sets `SHOAL_EXIT_RUN` and the rest; ignored otherwise.
#![cfg(windows)]

use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

mod common;
use common::shoald;
use shoal_sdk::types::{Mode, NewFolder, PathKind};
use shoal_sdk::Client;
use shoal_tray::approve::{Approval, Approving};
use shoal_tray::setup::first_device;
use shoal_tray::supervisor::Paths;

const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

/// The value the Linux half last posted as `name`, if it has: a commit status's target URL,
/// read through GitHub's REST API.
fn mailbox(name: &str) -> Option<String> {
    let url = format!(
        "{}/repos/{}/commits/{}/statuses?per_page=100",
        env("GITHUB_API_URL"),
        env("GITHUB_REPOSITORY"),
        env("GITHUB_SHA")
    );
    // The token on standard input, not the command line.
    let mut curl = Command::new("curl.exe")
        .args([
            "-sSf",
            "-H",
            "@-",
            "-H",
            "Accept: application/vnd.github+json",
            &url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    writeln!(
        curl.stdin.take()?,
        "Authorization: Bearer {}",
        env("SHOAL_EXIT_TOKEN")
    )
    .ok()?;
    let out = curl.wait_with_output().ok()?;
    let statuses: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let context = format!("exit-criterion/{}/{name}", env("SHOAL_EXIT_RUN"));
    statuses
        .as_array()?
        .iter()
        .filter(|s| s["context"] == context.as_str())
        .max_by_key(|s| s["id"].as_u64())
        .and_then(|s| s["target_url"].as_str())
        .and_then(|u| u.strip_prefix("https://mailbox.invalid/"))
        .map(str::to_owned)
}

fn wait<T>(what: &str, limit: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn is_placeholder(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_attributes() & RECALL_ON_DATA_ACCESS != 0)
}

/// The shell's listing of `dir`, as Explorer shows it: each item's file name, the name shown
/// (without a known extension, by default), and its availability.
fn shell_listing(dir: &Path) -> String {
    let script = format!(
        r#"$shell = New-Object -ComObject Shell.Application
$folder = $shell.NameSpace('{}')
$column = -1
for ($i = 0; $i -lt 400; $i++) {{ if ($folder.GetDetailsOf($null, $i) -eq 'Availability status') {{ $column = $i; break }} }}
foreach ($item in $folder.Items()) {{
  $status = if ($column -ge 0) {{ $folder.GetDetailsOf($item, $column) }} else {{ '' }}
  "$([System.IO.Path]::GetFileName($item.Path))`t$($item.Name)`t$status"
}}"#,
        dir.display()
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "the exit criterion, run with its Linux half by its own workflow"]
fn a_windows_partial_replica_with_a_linux_daemon() {
    let run = env("SHOAL_EXIT_RUN");
    let daemon = shoald().expect("shoald is built");
    let work = tempfile::tempdir().unwrap();
    let data_dir = work.path().join("data");
    let paths = Paths {
        daemon: daemon.clone(),
        config: data_dir.join("shoald.toml"),
        data_dir: data_dir.clone(),
    };
    // In the profile, where the shell shows a sync root.
    let root: PathBuf = PathBuf::from(env("USERPROFILE")).join(format!("Shoal exit {run}"));
    let _ = std::fs::remove_dir_all(&root);

    // The first of the user's devices, as the tray sets one up, with relays and discovery on.
    first_device(&paths, "Windows runner").unwrap();
    let log = std::fs::File::create(work.path().join("daemon.log")).unwrap();
    let _running = Daemon(
        Command::new(&daemon)
            .arg("run")
            .arg("--config")
            .arg(&paths.config)
            .env("RUST_LOG", "info,shoal_fs_cfapi=debug")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let client = Client::local(Client::socket_path(&data_dir));
    wait("the daemon", Duration::from_secs(60), || {
        block_on(client.status()).ok()
    });
    // A partial folder, so a sync root, added before pairing, so the new device is offered it.
    let folder = block_on(client.add_folder(&NewFolder {
        id: None,
        path: root.display().to_string(),
        members: Vec::new(),
        retention_days: 30,
        mode: Some(Mode::Partial),
        budget_bytes: Some(1 << 30),
        scan_interval_secs: Some(5),
        sync: Default::default(),
        encryption: None,
    }))
    .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check(&run, &client, &data_dir, &root)
    }));
    // Removed, so the runner keeps no sync root registered from it.
    let _ = block_on(client.remove_folder(&folder.id));
    let _ = block_on(client.shutdown());
    if let Err(panic) = result {
        eprintln!(
            "--- daemon log ---\n{}",
            std::fs::read_to_string(work.path().join("daemon.log")).unwrap_or_default()
        );
        std::panic::resume_unwind(panic);
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn check(run: &str, client: &Client, data_dir: &Path, root: &Path) {
    // Approve the Linux device, with a fresh code if one expires first.
    let mut tried = None;
    let linux = loop {
        let code = wait("a pairing code", Duration::from_secs(150 * 60), || {
            mailbox("code").filter(|c| Some(c) != tried.as_ref())
        });
        let (sender, events) = mpsc::channel();
        let approval = Approval::start(data_dir, &code, move |e| drop(sender.send(e)));
        tried = Some(code);
        match events.recv_timeout(Duration::from_secs(120)) {
            Ok(Approving::Confirm {
                name,
                device,
                confirmation,
            }) => {
                eprintln!("approving {name} ({device}), confirmation code {confirmation}");
                approval.approve();
                match events.recv_timeout(Duration::from_secs(120)) {
                    Ok(Approving::Approved) => break device,
                    other => eprintln!("not approved: {other:?}"),
                }
            }
            other => eprintln!("the code did not lead to a device: {other:?}"),
        }
    };

    // Its files show up as placeholders: there, with no data on disk.
    let hello = root.join("hello.txt");
    let big = root.join("docs").join("big.bin");
    wait(
        "placeholders of the Linux files",
        Duration::from_secs(10 * 60),
        || (is_placeholder(&hello) && is_placeholder(&big)).then_some(()),
    );
    let listing = shell_listing(root);
    eprintln!("the shell lists:\n{listing}");
    // As Explorer's "Status" column says it of an online-only file.
    assert!(
        listing
            .lines()
            .any(|l| l.starts_with("hello.txt\t") && l.ends_with("Available when online")),
        "{listing}"
    );
    assert!(
        listing.lines().any(|l| l.starts_with("hello.txt\t")),
        "{listing}"
    );
    assert!(
        listing.lines().any(|l| l.starts_with("docs\t")),
        "{listing}"
    );
    let docs = shell_listing(&root.join("docs"));
    eprintln!("and in docs:\n{docs}");
    assert!(docs.lines().any(|l| l.starts_with("big.bin\t")), "{docs}");

    // Opening one fetches it.
    assert_eq!(
        std::fs::read_to_string(&hello).unwrap(),
        format!("hello from Linux, run {run}\n")
    );
    let content = std::fs::read(&big).unwrap();
    assert_eq!(content.len(), 3 << 20);
    assert!(content
        .iter()
        .enumerate()
        .all(|(i, b)| *b == ((i * 7) % 251) as u8));
    assert!(!is_placeholder(&hello) && !is_placeholder(&big));
    eprintln!("after opening:\n{}", shell_listing(root));

    // Connected, directly if iroh punched through, through a relay if not: the runners are
    // separate virtual machines with only the internet between them.
    let peer = wait(
        "a path to the Linux device",
        Duration::from_secs(120),
        || {
            let status = block_on(client.status()).ok()?;
            let peer = status.peers.into_iter().find(|p| p.device == linux)?;
            eprintln!("the Linux device: {peer:?}");
            (peer.connected && peer.path.is_some()).then_some(peer)
        },
    );
    let direct = peer.path == Some(PathKind::Direct);
    eprintln!(
        "connected, {}: {peer:?}",
        if direct {
            "directly"
        } else {
            "through a relay"
        }
    );

    // And back: a file made here reaches the Linux daemon, which says so.
    std::fs::write(root.join("done.txt"), format!("run {run}\n")).unwrap();
    wait(
        "the Linux half to finish",
        Duration::from_secs(10 * 60),
        || mailbox("finished"),
    );
}
