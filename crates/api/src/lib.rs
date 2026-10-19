//! Tidefell `api` crate: REST surface for CLI, web and apps.
//!
//! Serves the `/v1` REST API (ADR 0009) over an [`Engine`]. Bodies are the `tidefell-sdk`
//! types; the OpenAPI document is generated from them and served at `/v1/openapi.json`.
//! `tidefelld` serves this router on its control endpoint: a Unix socket (ADR 0010), or on
//! Windows a named pipe (ADR 0042).

mod convert;
mod handlers;
mod pairing;

use std::path::Path;
use std::sync::Arc;

use axum::extract::FromRef;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
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
        .route("/v1/folders/{id}/rebuild", post(handlers::rebuild_folder))
        .route(
            "/v1/folders/{id}/deletes",
            get(handlers::held_deletes).post(handlers::decide_deletes),
        )
        .route("/v1/folders/{id}/retention", put(handlers::set_retention))
        .route("/v1/folders/{id}/sync", put(handlers::set_sync))
        .route("/v1/folders/{id}/members", post(handlers::add_member))
        .route(
            "/v1/folders/{id}/members/{user}",
            delete(handlers::remove_member),
        )
        .route(
            "/v1/folders/{id}/storage-peers",
            post(handlers::add_storage_peer),
        )
        .route(
            "/v1/folders/{id}/storage-peers/{device}",
            delete(handlers::remove_storage_peer),
        )
        .route("/v1/folders/{id}/scan", post(handlers::scan))
        .route("/v1/folders/{id}/files", get(handlers::files))
        .route("/v1/folders/{id}/fetch", post(handlers::fetch))
        .route("/v1/folders/{id}/remove-here", post(handlers::remove_here))
        .route("/v1/folders/{id}/rename", post(handlers::rename))
        .route("/v1/folders/{id}/pin", post(handlers::pin))
        .route("/v1/folders/{id}/unpin", post(handlers::unpin))
        .route("/v1/folders/{id}/pins", get(handlers::pins))
        .route("/v1/folders/{id}/copies", post(handlers::copies))
        .route("/v1/folders/{id}/versions", post(handlers::versions))
        .route(
            "/v1/folders/{id}/versions/restore",
            post(handlers::restore_version),
        )
        .route(
            "/v1/folders/{id}/versions/open",
            post(handlers::open_version),
        )
        .route("/v1/folders/{id}/resolve", post(handlers::resolve))
        .route("/v1/folders/{id}/problems", get(handlers::folder_problems))
        .route(
            "/v1/folders/{id}/problems/retry",
            post(handlers::retry_problems),
        )
        .route("/v1/doctor", get(handlers::doctor))
        .route("/v1/folders/{id}/activity", get(handlers::activity))
        .route("/v1/diagnostics", post(handlers::diagnostics))
        .route(
            "/v1/folders/{id}/recycle",
            get(handlers::recycle_bin).delete(handlers::empty_recycle_bin),
        )
        .route(
            "/v1/folders/{id}/recycle/{entry}",
            delete(handlers::discard),
        )
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
        .route("/v1/revocations", get(handlers::revocations))
        .route("/v1/devices/{device}/revoke", post(pairing::revoke))
        .route(
            "/v1/devices/{device}/revoke-preview",
            get(handlers::revoke_preview),
        )
        .route(
            "/v1/folders/{id}/withdrawals",
            get(handlers::held_withdrawals).post(handlers::decide_withdrawals),
        )
        .route("/v1/events", get(handlers::events))
        .route(
            "/v1/log",
            get(handlers::log_settings).put(handlers::set_log_level),
        )
        .route("/v1/log/problems", get(handlers::problems))
        .route("/v1/crashes", get(handlers::crashes))
        .route("/v1/crashes/report", get(handlers::crash_report))
        .route(
            "/v1/limits",
            get(handlers::limits).put(handlers::set_limits),
        )
        .route("/v1/pause", post(handlers::pause))
        .route("/v1/resume", post(handlers::resume))
        .route("/v1/folders/{id}/pause", post(handlers::pause_folder))
        .route("/v1/folders/{id}/resume", post(handlers::resume_folder))
        .route("/v1/crashes/dismiss", post(handlers::dismiss_crashes))
        .route("/v1/openapi.json", get(handlers::openapi))
        .with_state(state)
}

/// The OpenAPI document for `/v1`.
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// Serves the API on a Unix socket at `path`, readable and writable only by the daemon's
/// user. Replaces a stale socket file. Runs until the listener fails.
#[cfg(unix)]
pub async fn serve_unix(engine: Arc<Engine>, path: &Path) -> std::io::Result<()> {
    serve_unix_with(engine, path, Options::default()).await
}

/// [`serve_unix`] with [`Options`].
#[cfg(unix)]
pub async fn serve_unix_with(
    engine: Arc<Engine>,
    path: &Path,
    options: Options,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %path.display(), "control API listening");
    axum::serve(listener, router_with(engine, options)).await
}

/// Serves the API on this machine's control endpoint `endpoint`, as
/// `tidefell_sdk::Client::socket_path` names it: a Unix socket, or on Windows a named pipe.
/// Runs until the listener fails.
pub async fn serve_local(engine: Arc<Engine>, endpoint: &Path) -> std::io::Result<()> {
    serve_local_with(engine, endpoint, Options::default()).await
}

/// [`serve_local`] with [`Options`].
#[cfg(unix)]
pub async fn serve_local_with(
    engine: Arc<Engine>,
    endpoint: &Path,
    options: Options,
) -> std::io::Result<()> {
    serve_unix_with(engine, endpoint, options).await
}

/// [`serve_local`] with [`Options`]: a named pipe (ADR 0042). Its default security lets only
/// the daemon's user, SYSTEM and administrators write to it, and it refuses remote clients, so
/// no one else can make a request. Fails if another daemon already serves the pipe.
#[cfg(windows)]
pub async fn serve_local_with(
    engine: Arc<Engine>,
    endpoint: &Path,
    options: Options,
) -> std::io::Result<()> {
    let listener = pipe::PipeListener::bind(endpoint)?;
    tracing::info!(pipe = %endpoint.display(), "control API listening");
    axum::serve(listener, router_with(engine, options)).await
}

#[cfg(windows)]
mod pipe {
    use std::path::Path;
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    /// Connections to a named pipe, for `axum::serve`: each accepted instance is handed over
    /// and a new one made to wait for the next client.
    pub(crate) struct PipeListener {
        name: std::ffi::OsString,
        next: NamedPipeServer,
    }

    impl PipeListener {
        pub(crate) fn bind(name: &Path) -> std::io::Result<Self> {
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(name)?;
            Ok(Self {
                name: name.as_os_str().to_owned(),
                next,
            })
        }

        fn instance(&self) -> std::io::Result<NamedPipeServer> {
            ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)
        }
    }

    impl axum::serve::Listener for PipeListener {
        type Io = NamedPipeServer;
        type Addr = std::ffi::OsString;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                if let Err(e) = self.next.connect().await {
                    tracing::warn!(error = %e, "accepting on the control pipe failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                // A client is connected: make the next instance before handing this one over.
                let next = loop {
                    match self.instance() {
                        Ok(next) => break next,
                        Err(e) => {
                            tracing::warn!(error = %e, "making a control pipe instance failed");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                };
                return (std::mem::replace(&mut self.next, next), self.name.clone());
            }
        }

        fn local_addr(&self) -> std::io::Result<Self::Addr> {
            Ok(self.name.clone())
        }
    }
}

/// The content type of [`serve_metrics`]'s page.
pub const METRICS_CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// A router serving the engine's metrics at `GET /metrics` for Prometheus (ADR 0030). It has no
/// authentication: bind it to loopback, or to a network only the scraper can reach.
pub fn metrics_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(
                |axum::extract::State(engine): axum::extract::State<Arc<Engine>>| async move {
                    // Building the page reads every folder's state.
                    match tokio::task::spawn_blocking(move || engine.metrics()).await {
                        Ok(page) => (
                            [(axum::http::header::CONTENT_TYPE, METRICS_CONTENT_TYPE)],
                            page,
                        )
                            .into_response(),
                        Err(e) => {
                            tracing::warn!(error = %e, "building the metrics failed");
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        }
                    }
                },
            ),
        )
        .with_state(engine)
}

/// Serves [`metrics_router`] on `listener` until it fails.
pub async fn serve_metrics(
    listener: tokio::net::TcpListener,
    engine: Arc<Engine>,
) -> std::io::Result<()> {
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(%addr, "metrics listening");
    }
    axum::serve(listener, metrics_router(engine)).await
}
