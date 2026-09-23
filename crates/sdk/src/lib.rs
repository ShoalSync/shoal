//! Tidefell `sdk` crate: public Rust client API for third parties.
//!
//! - [`types`]: the request and response bodies of the `/v1` REST API, with their OpenAPI
//!   schemas. The server (`tidefell-api`) and every client use these same types.
//! - [`Client`]: a small typed client for the API, over a daemon's Unix control socket or
//!   over HTTP.
//!
//! This crate is MIT OR Apache-2.0 and never depends on AGPL code, so third-party clients can
//! use it freely.

mod client;
pub mod types;

pub use client::{Client, Error};
