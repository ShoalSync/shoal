//! What the tray's tests share.
#![allow(dead_code)]

use std::path::PathBuf;

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
pub fn local_config(data_dir: &std::path::Path) -> String {
    // A TOML literal string keeps a Windows path's backslashes.
    format!(
        "data_dir = '{}'\n\n[network]\nrelay = {{ mode = \"disabled\" }}\naddress_lookup = false\nlocal_discovery = false\nbind = \"127.0.0.1:0\"\n",
        data_dir.display()
    )
}
