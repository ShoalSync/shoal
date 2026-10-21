//! Setting a device up from the tray, against a real `tidefelld`: one as the first of your
//! devices, and another joining it by pairing, approved through the first one's API.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

mod common;
use common::{local_config, tidefelld};
use tidefell_sdk::Client;
use tidefell_tray::approve::{Approval, Approving};
use tidefell_tray::setup::{first_device, Join, Joining};
use tidefell_tray::supervisor::Paths;

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn paths(daemon: &Path, dir: &Path) -> Paths {
    let data_dir = dir.join("data");
    Paths {
        daemon: daemon.to_owned(),
        config: data_dir.join("tidefelld.toml"),
        data_dir,
    }
}

fn next(events: &mpsc::Receiver<Joining>) -> Joining {
    events
        .recv_timeout(Duration::from_secs(60))
        .expect("pairing went quiet")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_device_then_another_joining_it() {
    let Some(daemon) = tidefelld() else {
        eprintln!("skipping: tidefelld is not built");
        return;
    };
    let dir = tempfile::tempdir().unwrap();

    // The first device: its user key is made, and its configuration written.
    let first = paths(&daemon, &dir.path().join("first"));
    first_device(&first, "desktop").unwrap();
    assert!(first.data_dir.join("user.key").exists());
    let check = Command::new(&daemon)
        .args(["config", "check"])
        .arg(&first.config)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(
        first_device(&first, "desktop").is_err(),
        "a device is set up once"
    );

    // Run with only direct connections, for the test.
    std::fs::write(&first.config, local_config(&first.data_dir)).unwrap();
    let _running = Daemon(
        Command::new(&daemon)
            .arg("run")
            .arg("--config")
            .arg(&first.config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = Client::local(Client::socket_path(&first.data_dir));
    for _ in 0..300 {
        if client.status().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Another joins: it shows a code, the first approves it, and both show the same
    // confirmation code.
    let second = paths(&daemon, &dir.path().join("second"));
    let (sender, events) = mpsc::channel();
    let _join = Join::start_with(
        &second,
        "laptop",
        &["--no-relay", "--bind", "127.0.0.1:0"],
        move |e| drop(sender.send(e)),
    )
    .unwrap();
    let events = tokio::task::spawn_blocking(move || {
        let code = next(&events);
        (code, events)
    });
    let (code, events) = events.await.unwrap();
    let Joining::Code(code) = code else {
        panic!("not a code: {code:?}")
    };
    // Approved from the first device's tray.
    let (approver_sender, approving) = mpsc::channel();
    let approval = Approval::start(&first.data_dir, &code, move |e| {
        drop(approver_sender.send(e))
    });
    let (asked, approving) = tokio::task::spawn_blocking(move || {
        (
            approving.recv_timeout(Duration::from_secs(60)).unwrap(),
            approving,
        )
    })
    .await
    .unwrap();
    let Approving::Confirm {
        name, confirmation, ..
    } = asked
    else {
        panic!("not asked to confirm: {asked:?}")
    };
    assert_eq!(name, "laptop");
    let (confirm, events) = tokio::task::spawn_blocking(move || (next(&events), events))
        .await
        .unwrap();
    assert_eq!(confirm, Joining::Confirm(confirmation));
    approval.approve();
    let approved = tokio::task::spawn_blocking(move || {
        approving.recv_timeout(Duration::from_secs(60)).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(approved, Approving::Approved);
    let joined = tokio::task::spawn_blocking(move || next(&events))
        .await
        .unwrap();
    assert_eq!(joined, Joining::Joined);
    assert!(second.config.exists());
    assert!(
        !second.data_dir.join("user.key").exists(),
        "only the first holds it"
    );
}

#[test]
fn approving_needs_a_running_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, events) = mpsc::channel();
    let _approval = Approval::start(dir.path(), "TFP1-nothing", move |e| drop(sender.send(e)));
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(30)).unwrap(),
        Approving::Failed(_)
    ));
}

#[test]
fn a_join_that_fails_says_why() {
    let Some(daemon) = tidefelld() else {
        eprintln!("skipping: tidefelld is not built");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let set_up = paths(&daemon, dir.path());
    first_device(&set_up, "desktop").unwrap();
    // Already set up: pairing refuses.
    let (sender, events) = mpsc::channel();
    let _join = Join::start(&set_up, "desktop", move |e| drop(sender.send(e))).unwrap();
    match next(&events) {
        Joining::Failed(reason) => assert!(reason.contains("already"), "{reason}"),
        other => panic!("not a failure: {other:?}"),
    }
}
