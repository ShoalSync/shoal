//! Tidefell `tray` crate: binary `tidefell-tray`, the Windows notification-area app (ADR 0042).
//! It starts and supervises `tidefelld`, shows what it is doing, opens folders, and pauses and
//! resumes syncing. What it shows and how it supervises are platform-neutral, and tested
//! everywhere; the icon and menu are Windows only.

pub mod supervisor;
#[cfg(windows)]
pub mod ui;
pub mod view;
