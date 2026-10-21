//! The supervisor against a real `tidefelld`: it starts the daemon, pauses and resumes it
//! through its API, starts it again when it stops unasked, and stops it on quitting. Runs where
//! `tidefelld` has been built, as `cargo test --workspace` builds it, or where `TIDEFELLD`
//! names one; skipped otherwise.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidefell_sdk::Client;
use tidefell_tray::supervisor::{supervise, Command as Ask, Paths, Snapshot};
use tidefell_tray::view::Daemon;
use tokio::sync::mpsc;

fn tidefelld() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TIDEFELLD") {
        return Some(path.into());
    }
    // Beside this test's directory, `target/<profile>/deps`.
    let exe = std::env::current_exe().ok()?;
    let path = exe
        .parent()?
        .parent()?
        .join(format!("tidefelld{}", std::env::consts::EXE_SUFFIX));
    path.exists().then_some(path)
}

fn setup(daemon: &Path, dir: &Path) -> Paths {
    let data_dir = dir.join("data");
    let out = Command::new(daemon)
        .args(["init", "--name", "tray-test", "--data-dir"])
        .arg(&data_dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config = dir.join("tidefelld.toml");
    // A TOML literal string keeps a Windows path's backslashes.
    std::fs::write(
        &config,
        format!(
            "data_dir = '{}'\n\n[network]\nrelay = {{ mode = \"disabled\" }}\naddress_lookup = false\nlocal_discovery = false\nbind = \"127.0.0.1:0\"\n",
            data_dir.display()
        ),
    )
    .unwrap();
    Paths {
        daemon: daemon.to_owned(),
        data_dir,
        config,
    }
}

async fn until(
    latest: &Arc<Mutex<Option<Snapshot>>>,
    what: &str,
    check: impl Fn(&Snapshot) -> bool,
) {
    for _ in 0..600 {
        if latest.lock().unwrap().as_ref().is_some_and(&check) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}: {:?}", latest.lock().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tray_runs_the_daemon() {
    let Some(daemon) = tidefelld() else {
        eprintln!("skipping: tidefelld is not built");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let paths = setup(&daemon, dir.path());
    let client = Client::local(Client::socket_path(&paths.data_dir));
    let latest = Arc::new(Mutex::new(None));
    let (commands, receiver) = mpsc::channel(8);
    let supervisor = tokio::spawn({
        let (paths, latest) = (paths.clone(), latest.clone());
        async move {
            supervise(paths, receiver, move |s| *latest.lock().unwrap() = Some(s)).await;
        }
    });

    until(&latest, "the daemon to run", |s| {
        s.daemon == Daemon::Running && s.status.is_some()
    })
    .await;
    commands.send(Ask::Pause).await.unwrap();
    until(&latest, "syncing to pause", |s| {
        s.status.as_ref().is_some_and(|s| s.paused)
    })
    .await;
    commands.send(Ask::Resume).await.unwrap();
    until(&latest, "syncing to resume", |s| {
        s.status.as_ref().is_some_and(|s| !s.paused)
    })
    .await;

    // Stopped unasked, as by a crash: started again.
    client.shutdown().await.unwrap();
    until(&latest, "the daemon to be seen gone", |s| {
        s.status.is_none()
    })
    .await;
    until(&latest, "the daemon to run again", |s| {
        s.daemon == Daemon::Running && s.status.is_some()
    })
    .await;

    // Quitting stops it, and the supervisor.
    commands.send(Ask::Quit).await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), supervisor)
        .await
        .expect("the supervisor quits")
        .unwrap();
    assert!(client.status().await.is_err(), "the daemon stopped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_cannot_start_is_reported() {
    let Some(daemon) = tidefelld() else {
        eprintln!("skipping: tidefelld is not built");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut paths = setup(&daemon, dir.path());
    // Not set up: no configuration.
    paths.config = dir.path().join("missing.toml");
    let latest = Arc::new(Mutex::new(None));
    let (commands, receiver) = mpsc::channel(8);
    let supervisor = tokio::spawn({
        let (paths, latest) = (paths.clone(), latest.clone());
        async move {
            supervise(paths, receiver, move |s| *latest.lock().unwrap() = Some(s)).await;
        }
    });
    until(&latest, "not set up", |s| s.daemon == Daemon::NotSetUp).await;

    // A configuration that is not valid: the daemon says why, and is not started for ever.
    std::fs::write(&paths.config, "not = [valid").unwrap();
    until(&latest, "the failure", |s| {
        matches!(s.daemon, Daemon::Failed(_))
    })
    .await;
    let Some(Snapshot {
        daemon: Daemon::Failed(reason),
        ..
    }) = latest.lock().unwrap().clone()
    else {
        unreachable!()
    };
    assert!(reason.contains("missing.toml"), "{reason}");
    commands.send(Ask::Quit).await.unwrap();
    supervisor.await.unwrap();
}
