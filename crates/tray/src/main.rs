//! `shoal-tray`: see the crate's documentation.
// No console window of its own.
#![windows_subsystem = "windows"]

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use native_windows_gui as nwg;

    match shoal_tray::ui::run(shoal_tray::supervisor::Paths::installed()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            nwg::error_message("Shoal", &format!("Shoal cannot start: {e}"));
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("shoal-tray is for Windows; elsewhere run shoald under a service manager");
    std::process::ExitCode::FAILURE
}
