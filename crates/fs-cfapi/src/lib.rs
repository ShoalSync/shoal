//! Tidefell `fs-cfapi` crate: Windows placeholders through the Cloud Files API (ADR 0042).
//!
//! A partial replica's folder is a sync root: files not on this device are placeholders that
//! Explorer shows, and reading one fetches it through the engine. Windows only: elsewhere the
//! crate is empty, so the workspace builds everywhere. For now it holds Phase 8's spike, in
//! `tests/spike.rs`.
#![cfg(windows)]
