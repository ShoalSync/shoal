//! The provider against real engines (ADR 0042): a full replica, the "NAS", and a partial one,
//! the "laptop", whose folder is a sync root. Needs a desktop session, as the Windows runner's
//! is, and the folder in the user's profile, where the shell shows sync roots.
#![cfg(windows)]

mod common;
use common::*;
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cloud_filter::placeholder::{Placeholder, UpdateOptions};
use tidefell_engine::Engine;

#[tokio::test(flavor = "multi_thread")]
async fn placeholders_fill_on_reading_and_other_programs_changes_sync() {
    let p = pair(90);
    p.nas.write("a.txt", b"alpha");
    p.nas.write("docs/b.txt", b"bravo");
    p.nas.write("docs/c.txt", b"charlie");
    p.nas.write("d.txt", b"delta");
    let (nas, laptop) = (p.nas.start().await, p.laptop.start().await);
    nas.scan(p.nas.folder).await.unwrap();
    laptop.connect(nas.addr()).await.unwrap();
    let folder = p.laptop.folder;
    eventually("the laptop lists the files", || {
        laptop.files(folder).unwrap().len() == 4
    })
    .await;
    let root = Arc::new(connect_root(&laptop, &p.laptop).await);

    // Every file is a placeholder, recorded, its data not on disk.
    for path in ["a.txt", "docs/b.txt", "docs/c.txt", "d.txt"] {
        assert!(is_placeholder_not_on_disk(&p.laptop.abs(path)), "{path}");
    }
    assert_eq!(laptop.placeholders(folder).unwrap().len(), 4);

    // Reading one fetches it, and it becomes a file on disk.
    let a = p.laptop.abs("a.txt");
    assert_eq!(blocking(move || fs::read(a).unwrap()).await, b"alpha");
    eventually("a.txt is recorded on disk", || {
        on_disk(&laptop, folder, "a.txt")
    })
    .await;
    assert_eq!(laptop.placeholders(folder).unwrap().len(), 3);
    changed(&root).await;

    // Another program deletes a placeholder: deleted everywhere.
    cmd(&["del".as_ref(), p.laptop.abs("docs/b.txt").as_os_str()]);
    eventually("the NAS deletes docs/b.txt", || {
        p.nas.read("docs/b.txt").is_none()
    })
    .await;

    // Another program renames a placeholder: the same entry, moved everywhere.
    let d = entry_at(&laptop, folder, "d.txt").unwrap();
    cmd(&[
        "move".as_ref(),
        p.laptop.abs("d.txt").as_os_str(),
        p.laptop.abs("e.txt").as_os_str(),
    ]);
    eventually("the NAS moves d.txt", || {
        p.nas.read("e.txt").as_deref() == Some(b"delta".as_slice()) && p.nas.read("d.txt").is_none()
    })
    .await;
    assert_eq!(entry_at(&nas, folder, "e.txt"), Some(d));

    // Another program renames a folder holding a file on disk and a placeholder: the same
    // entries, moved, not deleted and made again.
    let c = p.laptop.abs("docs/c.txt");
    assert_eq!(blocking(move || fs::read(c).unwrap()).await, b"charlie");
    eventually("docs/c.txt is on disk", || {
        on_disk(&laptop, folder, "docs/c.txt")
    })
    .await;
    let c_entry = entry_at(&laptop, folder, "docs/c.txt").unwrap();
    cmd(&[
        "move".as_ref(),
        p.laptop.abs("docs").as_os_str(),
        p.laptop.abs("papers").as_os_str(),
    ]);
    eventually("the NAS moves docs", || {
        p.nas.read("papers/c.txt").as_deref() == Some(b"charlie".as_slice())
    })
    .await;
    assert_eq!(entry_at(&nas, folder, "papers/c.txt"), Some(c_entry));
    assert!(on_disk(&laptop, folder, "papers/c.txt"));

    // A new file elsewhere becomes a placeholder here.
    p.nas.write("f.txt", b"foxtrot");
    nas.scan(folder).await.unwrap();
    eventually("the laptop lists f.txt", || {
        entry_at(&laptop, folder, "f.txt").is_some()
    })
    .await;
    changed(&root).await;
    assert!(is_placeholder_not_on_disk(&p.laptop.abs("f.txt")));
    // Nothing was deleted on the NAS along the way, beyond docs/b.txt.
    assert_eq!(nas.files(folder).unwrap().len(), 4);
    let root = Arc::try_unwrap(root).ok().unwrap();
    blocking(move || root.unregister().unwrap()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn freeing_up_space_and_pinning() {
    let p = pair(110);
    p.nas.write("kept.txt", b"kept here");
    p.nas.write("pinned.txt", b"pinned here");
    let (nas, laptop) = (p.nas.start().await, p.laptop.start().await);
    nas.scan(p.nas.folder).await.unwrap();
    laptop.connect(nas.addr()).await.unwrap();
    let folder = p.laptop.folder;
    eventually("the laptop lists the files", || {
        laptop.files(folder).unwrap().len() == 2
    })
    .await;
    let root = Arc::new(connect_root(&laptop, &p.laptop).await);
    let kept = p.laptop.abs("kept.txt");
    assert_eq!(
        blocking({
            let kept = kept.clone();
            move || fs::read(kept).unwrap()
        })
        .await,
        b"kept here"
    );
    eventually("kept.txt is on disk", || {
        on_disk(&laptop, folder, "kept.txt")
    })
    .await;
    changed(&root).await;

    // Freed, as Explorer's "Free up space" does: the file is no longer on this device, its
    // placeholder recorded, and nothing changes elsewhere.
    blocking({
        let kept = kept.clone();
        move || {
            Placeholder::options()
                .write_access()
                .exclusive()
                .open(&kept)
                .unwrap()
                .update(UpdateOptions::default().dehydrate(), None)
                .unwrap();
        }
    })
    .await;
    // From this process Windows reports nothing, as it would for Explorer's: the provider's
    // next pass finds it.
    let attributes = |p: &Path| {
        (
            fs::symlink_metadata(p).map(|m| format!("{:#x}", m.file_attributes())),
            fs::metadata(p).map(|m| format!("{:#x}", m.file_attributes())),
        )
    };
    eprintln!("after freeing: {:?}", attributes(&kept));
    let reconciled = {
        let root = root.clone();
        blocking(move || root.changed().unwrap()).await
    };
    eprintln!(
        "reconciled {reconciled:?}; files {:?}; placeholders {:?}",
        laptop
            .files(folder)
            .unwrap()
            .iter()
            .map(|f| (&f.path, f.on_disk))
            .collect::<Vec<_>>(),
        laptop.placeholders(folder).unwrap()
    );
    eventually("kept.txt is no longer on disk", || {
        !on_disk(&laptop, folder, "kept.txt")
            && laptop
                .placeholders(folder)
                .unwrap()
                .iter()
                .any(|(p, _)| p == "kept.txt")
    })
    .await;
    assert_eq!(
        p.nas.read("kept.txt").as_deref(),
        Some(b"kept here".as_slice())
    );

    // Pinned through the engine: Windows fills the placeholder through the provider, once,
    // and it stays pinned and on disk.
    laptop.pin(folder, "pinned.txt").await.unwrap();
    changed(&root).await;
    eventually("pinned.txt is on disk", || {
        on_disk(&laptop, folder, "pinned.txt")
    })
    .await;
    assert_eq!(
        p.laptop.read("pinned.txt").as_deref(),
        Some(b"pinned here".as_slice())
    );
    assert!(laptop
        .files(folder)
        .unwrap()
        .iter()
        .any(|f| f.path == "pinned.txt" && f.pinned));
    assert!(laptop.path_problems(folder).unwrap().is_empty());
    let root = Arc::try_unwrap(root).ok().unwrap();
    blocking(move || root.unregister().unwrap()).await;
}

/// Over its budget, the engine evicts a file it filled by dehydrating it in place: it stays in
/// the folder, online-only, and its placeholder is recorded (ADR 0042).
#[tokio::test(flavor = "multi_thread")]
async fn eviction_dehydrates_in_place() {
    let p = pair(140);
    // Two files that do not both fit the laptop's 1 MiB budget.
    let (a, b) = (vec![b'a'; 600 << 10], vec![b'b'; 600 << 10]);
    p.nas.write("a.bin", &a);
    p.nas.write("b.bin", &b);
    let (nas, laptop) = (p.nas.start().await, p.laptop.start().await);
    nas.scan(p.nas.folder).await.unwrap();
    laptop.connect(nas.addr()).await.unwrap();
    let folder = p.laptop.folder;
    eventually("the laptop lists the files", || {
        laptop.files(folder).unwrap().len() == 2
    })
    .await;
    let root = Arc::new(connect_root(&laptop, &p.laptop).await);
    laptop.set_freer(folder, Some(Arc::new(tidefell_fs_cfapi::free)));
    for (name, data) in [("a.bin", &a), ("b.bin", &b)] {
        let path = p.laptop.abs(name);
        assert_eq!(&blocking(move || fs::read(path).unwrap()).await, data);
    }

    // One of them is evicted: freed where it is, not removed and made again.
    let freed = |name: &str| {
        !on_disk(&laptop, folder, name)
            && is_placeholder_not_on_disk(&p.laptop.abs(name))
            && laptop
                .placeholders(folder)
                .unwrap()
                .iter()
                .any(|(path, _)| path == name)
    };
    eventually("a file is freed in place", || {
        freed("a.bin") || freed("b.bin")
    })
    .await;
    let (gone, kept) = if freed("a.bin") {
        ("a.bin", "b.bin")
    } else {
        ("b.bin", "a.bin")
    };
    assert!(on_disk(&laptop, folder, kept));
    let reconciled = {
        let root = root.clone();
        blocking(move || root.changed().unwrap()).await
    };
    assert_eq!(reconciled.made, 0, "the placeholder was not made again");
    assert!(laptop.path_problems(folder).unwrap().is_empty());

    // Read again, it fills again.
    let path = p.laptop.abs(gone);
    let data = blocking(move || fs::read(path).unwrap()).await;
    assert_eq!(&data, if gone == "a.bin" { &a } else { &b });
    laptop.set_freer(folder, None);
    let root = Arc::try_unwrap(root).ok().unwrap();
    blocking(move || root.unregister().unwrap()).await;
}

/// A file of the engine's is made in sync only while it is the file the engine recorded: one
/// moved over it with the same size and modification time is not (ADR 0042).
#[tokio::test(flavor = "multi_thread")]
async fn a_file_moved_over_one_recorded_is_not_made_in_sync() {
    let p = pair(150);
    let (nas, laptop) = (p.nas.start().await, p.laptop.start().await);
    laptop.connect(nas.addr()).await.unwrap();
    let folder = p.laptop.folder;
    let root = Arc::new(connect_root(&laptop, &p.laptop).await);
    p.laptop.write("mine.txt", b"one version");
    laptop.scan(folder).await.unwrap();
    assert!(on_disk(&laptop, folder, "mine.txt"));

    let abs = p.laptop.abs("mine.txt");
    let modified = fs::metadata(&abs).unwrap().modified().unwrap();
    let other = p.laptop.abs("other.tmp");
    fs::write(&other, b"two version").unwrap();
    fs::File::options()
        .write(true)
        .open(&other)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    fs::rename(&other, &abs).unwrap();
    let reconciled = {
        let root = root.clone();
        blocking(move || root.changed().unwrap()).await
    };
    assert_eq!(reconciled.in_sync, 0, "not the file recorded");

    // Once scanned, it is.
    laptop.scan(folder).await.unwrap();
    let reconciled = {
        let root = root.clone();
        blocking(move || root.changed().unwrap()).await
    };
    assert_eq!(reconciled.in_sync, 1);
    let root = Arc::try_unwrap(root).ok().unwrap();
    blocking(move || root.unregister().unwrap()).await;
}

/// A sync root dropped on the runtime's own thread, as a daemon drops one when a folder is
/// removed, while another root of the process is filling: the fill finishes and the engines
/// stay connected (ADR 0042).
#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_root_holds_up_nothing() {
    let (p, q) = (pair(120), pair(130));
    p.nas.write("busy.txt", b"busy");
    q.nas.write("idle.txt", b"idle");
    let (nas_p, laptop_p) = (p.nas.start().await, p.laptop.start().await);
    let (nas_q, laptop_q) = (q.nas.start().await, q.laptop.start().await);
    nas_p.scan(p.nas.folder).await.unwrap();
    nas_q.scan(q.nas.folder).await.unwrap();
    laptop_p.connect(nas_p.addr()).await.unwrap();
    laptop_q.connect(nas_q.addr()).await.unwrap();
    eventually("both laptops list their files", || {
        laptop_p.files(p.laptop.folder).unwrap().len() == 1
            && laptop_q.files(q.laptop.folder).unwrap().len() == 1
    })
    .await;
    let root_p = connect_root(&laptop_p, &p.laptop).await;
    let root_q = connect_root(&laptop_q, &q.laptop).await;

    let busy = p.laptop.abs("busy.txt");
    let read = tokio::task::spawn_blocking(move || fs::read(busy));
    // Dropped here, on the runtime's thread.
    drop(root_q);
    let read = tokio::time::timeout(Duration::from_secs(20), read)
        .await
        .expect("the fill was held up")
        .unwrap();
    assert_eq!(read.unwrap(), b"busy");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(laptop_p.peer_states().iter().all(|s| s.connected));
    assert!(laptop_q.peer_states().iter().all(|s| s.connected));
    blocking(move || root_p.unregister().unwrap()).await;
    let _ = (nas_q, laptop_q);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_while_away_and_a_version_changed_before_reading() {
    let p = pair(100);
    p.nas.write("gone.txt", b"gone");
    p.nas.write("h.txt", b"v1");
    let (nas, laptop) = (p.nas.start().await, p.laptop.start().await);
    nas.scan(p.nas.folder).await.unwrap();
    laptop.connect(nas.addr()).await.unwrap();
    let folder = p.laptop.folder;
    eventually("the laptop lists the files", || {
        laptop.files(folder).unwrap().len() == 2
    })
    .await;
    let root = connect_root(&laptop, &p.laptop).await;

    // Deleted while disconnected, which Windows does not report: found when connected again.
    // Disconnecting blocks: never on the runtime's own threads.
    blocking(move || root.disconnect()).await;
    fs::remove_file(p.laptop.abs("gone.txt")).unwrap();
    let root = Arc::new(connect_root(&laptop, &p.laptop).await);
    eventually("the NAS deletes gone.txt", || {
        p.nas.read("gone.txt").is_none()
    })
    .await;
    assert!(
        fs::symlink_metadata(p.laptop.abs("gone.txt")).is_err(),
        "not made again"
    );

    // Changed elsewhere after the placeholder was made: reading it never gives the old size
    // with new bytes. Before the placeholder is updated, the read fails; after, it gives the
    // new version.
    p.nas.write("h.txt", b"version two");
    nas.scan(folder).await.unwrap();
    for _ in 0..1000 {
        if laptop
            .files(folder)
            .unwrap()
            .iter()
            .any(|f| f.path == "h.txt" && f.size == 11)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let sizes = |e: &Engine| {
        e.files(folder)
            .unwrap()
            .into_iter()
            .map(|f| (f.path, f.size, f.on_disk))
            .collect::<Vec<_>>()
    };
    assert!(
        laptop
            .files(folder)
            .unwrap()
            .iter()
            .any(|f| f.path == "h.txt" && f.size == 11),
        "the laptop has the new version: laptop {:?} nas {:?} peers {:?} problems {:?}",
        sizes(&laptop),
        sizes(&nas),
        laptop
            .peer_states()
            .iter()
            .map(|p| p.connected)
            .collect::<Vec<_>>(),
        laptop.path_problems(folder).unwrap()
    );
    let h = p.laptop.abs("h.txt");
    let early = blocking({
        let h = h.clone();
        move || fs::read(h)
    })
    .await;
    assert!(
        early.is_err(),
        "the old placeholder is not filled: {early:?}"
    );
    changed(&root).await;
    assert_eq!(blocking(move || fs::read(h).unwrap()).await, b"version two");
    let root = Arc::try_unwrap(root).ok().unwrap();
    blocking(move || root.unregister().unwrap()).await;
}
