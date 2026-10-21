//! The tray's icon and menu come up in a desktop session, as the Windows runner's is, and go
//! when it quits (ADR 0042's amendment).
#![cfg(windows)]

use std::time::{Duration, Instant};

use tidefell_tray::supervisor::{Command, Paths};

#[test]
fn the_tray_shows_and_quits() {
    let dir = tempfile::tempdir().unwrap();
    // Not set up: nothing to start.
    let paths = Paths {
        daemon: dir.path().join("tidefelld.exe"),
        data_dir: dir.path().join("data"),
        config: dir.path().join("tidefelld.toml"),
    };
    let began = Instant::now();
    tidefell_tray::ui::run_with(paths, |commands| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            commands.blocking_send(Command::Quit).unwrap();
        });
    })
    .unwrap();
    assert!(began.elapsed() < Duration::from_secs(30));
}
