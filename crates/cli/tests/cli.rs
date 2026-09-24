//! The `tidefell` binary against two in-process engines serving their control sockets: the
//! Phase 3 flow of creating a folder, joining it from another device, and restoring a
//! deleted file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tidefell_engine::{Engine, EngineConfig, RelayConfig};
use tidefell_identity::{DeviceKey, UserKey};

const BIN: &str = env!("CARGO_BIN_EXE_tidefell");

struct Device {
    data: PathBuf,
    engine: Arc<Engine>,
}

impl Device {
    async fn start(root: &Path, name: &str, seed: u8, user: &UserKey) -> Self {
        Self::start_with(root, name, seed, user, None).await
    }

    async fn start_with(
        root: &Path,
        name: &str,
        seed: u8,
        user: &UserKey,
        user_key: Option<UserKey>,
    ) -> Self {
        let data = root.join(name).join("data");
        let key = DeviceKey::from_seed([seed; 32]);
        let engine = Arc::new(
            Engine::start(
                EngineConfig {
                    data_dir: data.clone(),
                    relay: RelayConfig::Disabled,
                    address_lookup: false,
                    bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
                    peers: Vec::new(),
                    folders: Vec::new(),
                },
                key.clone(),
                user.certify(key.id(), name, 0),
            )
            .await
            .unwrap(),
        );
        let socket = data.join("control.sock");
        tokio::spawn({
            let (engine, socket) = (engine.clone(), socket.clone());
            let options = tidefell_api::Options { user_key };
            async move { tidefell_api::serve_unix_with(engine, &socket, options).await }
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self { data, engine }
    }

    /// Runs `tidefell` against this device and returns stdout, failing on a non-zero exit.
    fn run(&self, args: &[&str]) -> String {
        let out = self.try_run(args);
        assert!(
            out.status.success(),
            "tidefell {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn try_run(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN)
            .arg("--data-dir")
            .arg(&self.data)
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.run(&all)).unwrap()
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn create_join_delete_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let laptop = Device::start(tmp.path(), "laptop", 10, &alice).await;
    let nas = Device::start(tmp.path(), "nas", 11, &alice).await;

    let status = laptop.json(&["status"]);
    assert_eq!(status["user"], alice.id().to_string());
    assert!(laptop.run(&["folder", "list"]).contains("no folders"));

    // Create on the laptop; the output tells the user how to join elsewhere.
    let laptop_root = tmp.path().join("laptop/photos");
    let created = laptop.run(&["folder", "create", laptop_root.to_str().unwrap()]);
    assert!(created.contains("tidefell folder join"));
    let folders = laptop.json(&["folder", "list"]);
    let id = folders[0]["id"].as_str().unwrap().to_string();
    std::fs::write(laptop_root.join("beach.jpg"), b"sand and sea").unwrap();
    laptop.run(&["folder", "scan", &id]);

    // The NAS joins and keeps the laptop connected.
    let nas_root = tmp.path().join("nas/photos");
    nas.run(&[
        "folder",
        "join",
        &id,
        nas_root.to_str().unwrap(),
        "--retention-days",
        "90",
    ]);
    let laptop_addr = laptop.engine.addr();
    let addr = laptop_addr.ip_addrs().next().unwrap().to_string();
    nas.run(&[
        "peer",
        "add",
        &laptop.engine.id().to_string(),
        "--addr",
        &addr,
    ]);

    eventually("photo reaches the NAS", || {
        std::fs::read(nas_root.join("beach.jpg")).ok().as_deref()
            == Some(b"sand and sea".as_slice())
    })
    .await;
    let files = nas.run(&["files", &id]);
    assert!(files.contains("beach.jpg"), "{files}");
    let peers = nas.json(&["peer", "list"]);
    assert_eq!(peers["connected"].as_array().unwrap().len(), 1);

    // Delete on the laptop, restore by path from the NAS.
    std::fs::remove_file(laptop_root.join("beach.jpg")).unwrap();
    laptop.run(&["folder", "scan", &id]);
    eventually("delete reaches the NAS", || {
        !nas_root.join("beach.jpg").exists()
    })
    .await;
    let bin = nas.run(&["recycle", "list", &id]);
    assert!(bin.contains("beach.jpg"), "{bin}");
    assert!(nas
        .run(&["recycle", "restore", &id, "beach.jpg"])
        .contains("restored beach.jpg"));
    eventually("restored on the laptop", || {
        std::fs::read(laptop_root.join("beach.jpg")).ok().as_deref()
            == Some(b"sand and sea".as_slice())
    })
    .await;

    // Errors are reported on stderr with a non-zero exit.
    let out = nas.try_run(&["recycle", "restore", &id, "nope.txt"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not in the recycle bin"));
    let out = nas.try_run(&["files", "not-an-id"]);
    assert!(!out.status.success());

    // A partial replica on a third device keeps the photo as a placeholder until fetched.
    let phone = Device::start(tmp.path(), "phone", 12, &alice).await;
    let phone_root = tmp.path().join("phone/photos");
    phone.run(&[
        "folder",
        "join",
        &id,
        phone_root.to_str().unwrap(),
        "--partial",
        "1000000",
    ]);
    phone.engine.connect(nas.engine.addr()).await.unwrap();
    eventually("the phone lists the photo", || {
        phone.run(&["files", &id]).contains("beach.jpg")
    })
    .await;
    assert!(!phone_root.join("beach.jpg").exists());
    assert_eq!(phone.json(&["files", &id])[0]["on_disk"], false);
    let fetched = phone.run(&["fetch", &id, "beach.jpg"]);
    assert_eq!(
        fetched.trim(),
        phone_root.join("beach.jpg").to_str().unwrap()
    );
    assert_eq!(
        std::fs::read(phone_root.join("beach.jpg")).unwrap(),
        b"sand and sea"
    );
    let out = phone.try_run(&["fetch", &id, "nope.jpg"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such file"));

    // Sharing with another user, then removing the folder.
    let bob = UserKey::from_seed([2; 32]).id().to_string();
    nas.run(&["folder", "share", &id, &bob]);
    assert!(nas.json(&["folder", "list"])[0]["members"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == &serde_json::Value::String(bob.clone())));
    nas.run(&["folder", "remove", &id]);
    assert!(nas.run(&["folder", "list"]).contains("no folders"));
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_approves_a_new_device() {
    use tidefell_engine::pairing::{self, PairingEvent};

    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let laptop = Device::start_with(tmp.path(), "laptop", 20, &alice, Some(alice.clone())).await;
    let nas = Device::start(tmp.path(), "nas", 21, &alice).await;

    let phone_key = DeviceKey::from_seed([22; 32]);
    let mut server = pairing::start_pairing(
        &phone_key,
        RelayConfig::Disabled,
        false,
        Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        "Pixel",
    )
    .await
    .unwrap();
    let code = server.code().to_text();

    // Without the user key, a daemon refuses.
    let out = nas.try_run(&["pair", "--yes", &code]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("user key"));

    let run = {
        let (bin, data, code) = (BIN, laptop.data.clone(), code.clone());
        tokio::task::spawn_blocking(move || {
            Command::new(bin)
                .arg("--data-dir")
                .arg(data)
                .args(["pair", "--yes", "--name", "Alice's phone", &code])
                .output()
                .unwrap()
        })
    };
    let mut confirmation = None;
    let paired = loop {
        match tokio::time::timeout(Duration::from_secs(10), server.next_event())
            .await
            .unwrap()
            .unwrap()
        {
            PairingEvent::Confirming {
                confirmation_code, ..
            } => confirmation = Some(confirmation_code),
            PairingEvent::Paired(paired) => break paired,
            PairingEvent::Failed { reason } => panic!("pairing failed: {reason}"),
        }
    };
    let out = run.await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let confirmation = confirmation.unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("{} {}", &confirmation[..3], &confirmation[3..])),
        "{stderr}"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("added Alice's phone"));
    let certificate = paired.certificate.verify_certificate().unwrap();
    assert_eq!(certificate.device, phone_key.id());
    assert_eq!(certificate.name, "Alice's phone");
    assert_eq!(paired.approver.device, laptop.engine.id());
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_and_emptying_the_recycle_bin() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let laptop = Device::start(tmp.path(), "laptop", 20, &alice).await;
    let root = tmp.path().join("laptop/docs");
    laptop.run(&["folder", "create", root.to_str().unwrap()]);
    let id = laptop.json(&["folder", "list"])[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    for (name, body) in [("a.txt", "aaaa"), ("b.txt", "bbbbbbbb")] {
        std::fs::write(root.join(name), body).unwrap();
    }
    laptop.run(&["folder", "scan", &id]);
    for name in ["a.txt", "b.txt"] {
        std::fs::remove_file(root.join(name)).unwrap();
    }
    laptop.run(&["folder", "scan", &id]);

    let status = laptop.run(&["status"]);
    assert!(status.contains("recycle bin 2 file(s), 12 B"), "{status}");
    assert!(
        status.contains("keeps deleted files for 30 days"),
        "{status}"
    );
    let status = laptop.json(&["status"]);
    assert_eq!(status["user"], alice.id().to_string());
    assert_eq!(status["folders"][0]["recycle_bin"]["items"], 2);

    let changed = laptop.run(&["folder", "retention", &id, "7"]);
    assert!(
        changed.contains("keeps deleted files for 7 days"),
        "{changed}"
    );
    let changed = laptop.run(&["folder", "retention", &id, "forever"]);
    assert!(changed.contains("keeps deleted files forever"), "{changed}");
    assert!(laptop.run(&["folder", "list"]).contains("kept forever"));
    assert!(!laptop
        .try_run(&["folder", "retention", &id, "soon"])
        .status
        .success());

    // Without --yes, emptying asks first; no answer means no.
    let out = laptop.try_run(&["recycle", "empty", &id]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("2 file(s), 12 B"));
    assert_eq!(
        laptop.json(&["folder", "list"])[0]["recycle_bin"]["items"],
        2
    );

    let one = laptop.run(&["recycle", "empty", &id, "a.txt", "--yes"]);
    assert!(one.contains("permanently deleted a.txt"), "{one}");
    let rest = laptop.run(&["recycle", "empty", &id, "--yes"]);
    assert!(rest.contains("permanently deleted b.txt"), "{rest}");
    assert!(laptop
        .run(&["recycle", "list", &id])
        .contains("the recycle bin is empty"));
    let out = laptop.try_run(&["recycle", "empty", &id, "a.txt", "--yes"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not in the recycle bin"));
}

#[test]
fn missing_daemon_is_a_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .arg("--data-dir")
        .arg(tmp.path())
        .arg("status")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot connect"));
}
