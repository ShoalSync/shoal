//! What the tray's tests share.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tidefell_tray::supervisor::Paths;

/// The `tidefelld` the tests run: `TIDEFELLD`, else the one built beside them, as
/// `cargo test --workspace` builds it. None if neither: the test is skipped.
pub fn tidefelld() -> Option<PathBuf> {
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

/// A configuration for `data_dir` with only direct connections on this machine, and no
/// discovery.
pub fn local_config(data_dir: &Path) -> String {
    // A TOML literal string keeps a Windows path's backslashes.
    format!(
        "data_dir = '{}'\n\n[network]\nrelay = {{ mode = \"disabled\" }}\naddress_lookup = false\nlocal_discovery = false\nbind = \"127.0.0.1:0\"\n",
        data_dir.display()
    )
}

/// Sets up a device in `dir` with `daemon`, as the first of its user's, its configuration
/// [`local_config`].
pub fn set_up(daemon: &Path, dir: &Path) -> Paths {
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
    std::fs::write(&config, local_config(&data_dir)).unwrap();
    Paths {
        daemon: daemon.to_owned(),
        data_dir,
        config,
    }
}
