//! Tidefell `web` crate: web peer: accounts, sessions, embedded browser UI.
//!
//! The web peer runs inside `tidefelld` (ADR 0010). People sign in with a local account linked
//! to a Tidefell user (ADR 0011) and see the folders that user is a member of: files, downloads
//! and the recycle bin with restore. Administrators also manage accounts. Pages are rendered on
//! the server with askama; htmx is vendored and embedded (ADR 0012).
//!
//! Put the web peer behind a TLS-terminating reverse proxy; session cookies are `Secure` unless
//! [`WebConfig::secure_cookies`] is turned off for local testing.

mod accounts;
mod format;
mod handlers;
mod pages;
mod sessions;

use std::path::Path;
use std::sync::Arc;

use axum::http::{header, HeaderValue};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tidefell_engine::Engine;

pub use accounts::{Account, AccountError, AccountStore};

/// Web peer settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebConfig {
    /// Mark session cookies `Secure` (HTTPS only). Turn off only for local testing.
    pub secure_cookies: bool,
}

pub(crate) struct AppState {
    pub(crate) engine: Arc<Engine>,
    pub(crate) accounts: AccountStore,
    pub(crate) sessions: sessions::Sessions,
    pub(crate) config: WebConfig,
}

async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; \
             frame-ancestors 'none'; form-action 'self'; base-uri 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// The web peer's router.
pub fn router(engine: Arc<Engine>, data_dir: &Path, config: WebConfig) -> Router {
    let state = Arc::new(AppState {
        engine,
        accounts: AccountStore::new(data_dir),
        sessions: sessions::Sessions::default(),
        config,
    });
    Router::new()
        .route("/", get(handlers::folders))
        .route("/login", get(handlers::login_page).post(handlers::login))
        .route("/logout", post(handlers::logout))
        .route("/folders/{id}", get(handlers::folder))
        .route("/folders/{id}/download", get(handlers::download))
        .route("/folders/{id}/recycle", get(handlers::recycle))
        .route(
            "/folders/{id}/recycle/{entry}/restore",
            post(handlers::restore),
        )
        .route(
            "/admin/accounts",
            get(handlers::accounts).post(handlers::save_account),
        )
        .route(
            "/admin/accounts/{username}/delete",
            post(handlers::delete_account),
        )
        .route("/static/style.css", get(handlers::style))
        .route("/static/htmx.min.js", get(handlers::htmx))
        .layer(axum::middleware::map_response(security_headers))
        .with_state(state)
}

/// Serves the web peer on `listener` until it fails.
pub async fn serve(
    listener: tokio::net::TcpListener,
    engine: Arc<Engine>,
    data_dir: &Path,
    config: WebConfig,
) -> std::io::Result<()> {
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(%addr, "web peer listening");
    }
    axum::serve(listener, router(engine, data_dir, config)).await
}
