//! The Storage window against a real `tidefelld` (ADR 0023's amendment, its section of
//! 2026-10-23): it loads an encrypted folder's priorities and what its storage peers keep, and
//! sets and removes rules through the daemon. On Windows the window itself opens, in the
//! runner's desktop session, and with `TIDEFELL_SCREENSHOT` set the test prints a picture of it
//! (`SHOT` lines, base64 PNG). Runs where `tidefelld` has been built; skipped otherwise.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::{set_up, tidefelld};
use tidefell_sdk::types::{NewEncryption, NewFolder};
use tidefell_sdk::Client;
use tidefell_tray::storage::{pattern_for, rows, Answer, Request, Session};
use tidefell_tray::supervisor::{supervise, Command as Ask};
use tokio::sync::mpsc as channel;

fn loaded(answers: &mpsc::Receiver<Answer>) -> (Vec<tidefell_tray::storage::Row>, usize) {
    match answers.recv_timeout(Duration::from_secs(60)).unwrap() {
        Answer::Loaded(priorities, reports) => (rows(&priorities), reports.len()),
        other => panic!("not loaded: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_storage_window_ranks_through_the_daemon() {
    let Some(daemon) = tidefelld() else {
        eprintln!("skipping: tidefelld is not built");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let paths = set_up(&daemon, dir.path());
    let client = Client::local(Client::socket_path(&paths.data_dir));
    let latest = Arc::new(Mutex::new(None));
    let (commands, receiver) = channel::channel(8);
    let supervisor = tokio::spawn({
        let (paths, latest) = (paths.clone(), latest.clone());
        async move {
            supervise(paths, receiver, move |s| *latest.lock().unwrap() = Some(s)).await;
        }
    });
    for _ in 0..600 {
        if client.status().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join("Photos")).unwrap();
    std::fs::write(root.join("notes.txt"), "notes").unwrap();
    std::fs::write(root.join("Photos").join("a.jpg"), "photo").unwrap();
    let folder = client
        .add_folder(&NewFolder {
            id: None,
            path: root.display().to_string(),
            members: Vec::new(),
            retention_days: 7,
            mode: None,
            budget_bytes: None,
            scan_interval_secs: Some(3600),
            sync: Default::default(),
            encryption: Some(NewEncryption {
                // A storage peer never connected: the window says so.
                storage_peers: vec!["d".repeat(64)],
            }),
        })
        .await
        .unwrap();
    client.scan(&folder.id).await.unwrap();

    let (sender, answers) = mpsc::channel();
    let session = Session::start(&paths.data_dir, &folder.id, move |a| {
        let _ = sender.send(a);
    });
    session.ask(Request::Load);
    let (shown, peers) = loaded(&answers);
    assert_eq!(peers, 1);
    let lines: Vec<(&str, u8, &str)> = shown
        .iter()
        .map(|r| (r.path.as_str(), r.priority, r.source.as_str()))
        .collect();
    assert_eq!(
        lines,
        [
            ("Photos", 128, "the default"),
            ("Photos/a.jpg", 128, "the default"),
            ("notes.txt", 128, "the default"),
        ]
    );

    session.ask(Request::Set {
        pattern: pattern_for("Photos", true),
        priority: Some(200),
    });
    let (shown, _) = loaded(&answers);
    let photos = shown.iter().find(|r| r.path == "Photos").unwrap();
    assert!(photos.set_here && photos.priority == 200, "{photos:?}");
    let photo = shown.iter().find(|r| r.path == "Photos/a.jpg").unwrap();
    assert_eq!(
        (photo.priority, photo.source.as_str()),
        (200, "from the rule for /Photos/")
    );
    assert!(
        std::fs::read_to_string(root.join(".tidefellpriorities"))
            .unwrap()
            .contains("200 /Photos/"),
        "the rule is in the folder's file"
    );

    session.ask(Request::Set {
        pattern: pattern_for("notes.txt", false),
        priority: Some(50),
    });
    let (shown, _) = loaded(&answers);
    let notes = shown.iter().find(|r| r.path == "notes.txt").unwrap();
    assert!(notes.set_here && notes.priority == 50, "{notes:?}");
    session.ask(Request::Set {
        pattern: pattern_for("notes.txt", false),
        priority: None,
    });
    let (shown, _) = loaded(&answers);
    let notes = shown.iter().find(|r| r.path == "notes.txt").unwrap();
    assert!(!notes.set_here && notes.priority == 128, "{notes:?}");

    // Last: quitting the tray stops the daemon.
    #[cfg(windows)]
    show_the_window(&paths, &folder.id);

    commands.send(Ask::Quit).await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), supervisor)
        .await
        .expect("the supervisor quits")
        .unwrap();
}

/// Opens the folder's Storage window, and with `TIDEFELL_SCREENSHOT` set prints what it shows.
#[cfg(windows)]
fn show_the_window(paths: &tidefell_tray::supervisor::Paths, folder: &str) {
    // Drawn by the window itself, so whatever covers it on the desktop is left out.
    const CAPTURE: &str = r#"
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Shot {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    // The class as a pointer: PowerShell passes $null for a string as "", which no class is.
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(IntPtr cls, string title);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out Rect r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
}
'@
$h = [IntPtr]::Zero
for ($i = 0; $i -lt 100 -and $h -eq [IntPtr]::Zero; $i++) { $h = [Shot]::FindWindow([IntPtr]::Zero, 'Storage: vault'); Start-Sleep -Milliseconds 100 }
if ($h -eq [IntPtr]::Zero) { Write-Output 'NO WINDOW'; exit 1 }
Start-Sleep 3
$r = New-Object Shot+Rect
[void][Shot]::GetWindowRect($h, [ref]$r)
$bitmap = New-Object Drawing.Bitmap ($r.Right - $r.Left), ($r.Bottom - $r.Top)
$g = [Drawing.Graphics]::FromImage($bitmap)
$dc = $g.GetHdc(); [void][Shot]::PrintWindow($h, $dc, 2); $g.ReleaseHdc($dc)
$stream = New-Object IO.MemoryStream
$bitmap.Save($stream, [Drawing.Imaging.ImageFormat]::Png)
$text = [Convert]::ToBase64String($stream.ToArray())
for ($i = 0; $i -lt $text.Length; $i += 4000) { Write-Output "SHOT storage $($text.Substring($i, [Math]::Min(4000, $text.Length - $i)))" }
"#;
    let shown = Arc::new(Mutex::new(None));
    tidefell_tray::ui::run_opening(paths.clone(), Some((folder, "vault")), |commands| {
        let shown = shown.clone();
        std::thread::spawn(move || {
            let out = std::process::Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", CAPTURE])
                .output();
            *shown.lock().unwrap() = Some(out);
            let _ = commands.blocking_send(tidefell_tray::supervisor::Command::Quit);
        });
    })
    .unwrap();
    let out = shown.lock().unwrap().take().unwrap().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("SHOT storage "),
        "the window shows: {text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    if std::env::var_os("TIDEFELL_SCREENSHOT").is_some() {
        println!("{text}");
    }
}
