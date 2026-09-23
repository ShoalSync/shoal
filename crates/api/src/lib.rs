//! Tidefell `api` crate: REST surface for CLI, web and apps.
//!
//! Serves the `/v1` REST API (ADR 0009) over an [`Engine`]. Bodies are the `tidefell-sdk`
//! types; the OpenAPI document is generated from them and served at `/v1/openapi.json`.
//! `tidefelld` serves this router on its Unix control socket (ADR 0010).

mod convert;
mod handlers;
mod pairing;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use axum::extract::FromRef;
use axum::routing::{delete, get, post};
use axum::Router;
use tidefell_engine::Engine;
use tidefell_identity::UserKey;
use utoipa::OpenApi;

pub use handlers::ApiDoc;

/// What the API may do beyond the engine itself.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The user key, if this device holds it; required to approve pairings (ADR 0015).
    pub user_key: Option<UserKey>,
}

#[derive(Clone)]
pub(crate) struct ApiState {
    engine: Arc<Engine>,
    pairings: Arc<pairing::Pairings>,
}

impl FromRef<ApiState> for Arc<Engine> {
    fn from_ref(state: &ApiState) -> Self {
        state.engine.clone()
    }
}

impl FromRef<ApiState> for Arc<pairing::Pairings> {
    fn from_ref(state: &ApiState) -> Self {
        state.pairings.clone()
    }
}

/// The `/v1` router over `engine`, without a user key.
pub fn router(engine: Arc<Engine>) -> Router {
    router_with(engine, Options::default())
}

/// The `/v1` router over `engine`.
pub fn router_with(engine: Arc<Engine>, options: Options) -> Router {
    let state = ApiState {
        engine,
        pairings: Arc::new(pairing::Pairings::new(options.user_key)),
    };
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
        .route("/v1/pairings", post(pairing::start))
        .route("/v1/pairings/{id}/approve", post(pairing::approve))
        .route("/v1/pairings/{id}", delete(pairing::decline))
        .route("/v1/events", get(handlers::events))
        .route("/v1/openapi.json", get(handlers::openapi))
        .with_state(state)
}

/// The OpenAPI document for `/v1`.
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// Serves the API on a Unix socket at `path`, readable and writable only by the daemon's
/// user. Replaces a stale socket file. Runs until the listener fails.
pub async fn serve_unix(engine: Arc<Engine>, path: &Path) -> std::io::Result<()> {
    serve_unix_with(engine, path, Options::default()).await
}

/// [`serve_unix`] with [`Options`].
pub async fn serve_unix_with(
    engine: Arc<Engine>,
    path: &Path,
    options: Options,
) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %path.display(), "control API listening");
    axum::serve(listener, router_with(engine, options)).await
}
