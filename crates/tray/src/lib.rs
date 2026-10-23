//! Tidefell `tray` crate: binary `tidefell-tray`, the Windows notification-area app (ADR 0042).
//! It starts and supervises `tidefelld`, shows what it is doing, opens folders, ranks what an
//! encrypted folder's storage peers keep, and pauses and resumes syncing. What it shows and how
//! it supervises are platform-neutral, and tested everywhere; the icon, menu and windows are
//! Windows only.

pub mod approve;
pub mod setup;
pub mod storage;
pub mod supervisor;
#[cfg(windows)]
pub mod ui;
pub mod view;
