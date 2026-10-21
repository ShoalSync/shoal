//! `tidefell-tray`: see the crate's documentation.
// No console window of its own.
#![windows_subsystem = "windows"]

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use native_windows_gui as nwg;

    match tidefell_tray::ui::run(tidefell_tray::supervisor::Paths::installed()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            nwg::error_message("Tidefell", &format!("Tidefell cannot start: {e}"));
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("tidefell-tray is for Windows; elsewhere run tidefelld under a service manager");
    std::process::ExitCode::FAILURE
}
