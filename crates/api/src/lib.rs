//! Tidefell `api` crate: REST + gRPC surface for CLI, web and apps.
//!
//! Serves the `/v1` REST API (ADR 0009) over an [`Engine`]. Bodies are the `tidefell-sdk`
//! types; the OpenAPI document is generated from them and served at `/v1/openapi.json`.
//! `tidefelld` serves this router on its Unix control socket (ADR 0010).

mod convert;
mod handlers;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use axum::routing::{delete, get, post};
use axum::Router;
use tidefell_engine::Engine;
use utoipa::OpenApi;

pub use handlers::ApiDoc;

/// The `/v1` router over `engine`.
pub fn router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/v1/status", get(handlers::status))
        .route(
            "/v1/folders",
            get(handlers::folders).post(handlers::add_folder),
        )
        .route(
            "/v1/folders/{id}",
            get(handlers::folder).delete(handlers::remove_folder),
        )
        .route("/v1/folders/{id}/members", post(handlers::add_member))
        .route(
            "/v1/folders/{id}/members/{user}",
            delete(handlers::remove_member),
        )
        .route("/v1/folders/{id}/scan", post(handlers::scan))
        .route("/v1/folders/{id}/files", get(handlers::files))
        .route("/v1/folders/{id}/fetch", post(handlers::fetch))
        .route("/v1/folders/{id}/recycle", get(handlers::recycle_bin))
        .route(
            "/v1/folders/{id}/recycle/{entry}/restore",
            post(handlers::restore),
        )
        .route("/v1/peers", get(handlers::peers))
        .route(
            "/v1/peers/known",
            get(handlers::known_peers).post(handlers::add_peer),
        )
        .route("/v1/peers/known/{device}", delete(handlers::remove_peer))
        .route("/v1/events", get(handlers::events))
        .route("/v1/openapi.json", get(handlers::openapi))
        .with_state(engine)
}

/// The OpenAPI document for `/v1`.
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// Serves the API on a Unix socket at `path`, readable and writable only by the daemon's
/// user. Replaces a stale socket file. Runs until the listener fails.
pub async fn serve_unix(engine: Arc<Engine>, path: &Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %path.display(), "control API listening");
    axum::serve(listener, router(engine)).await
}
