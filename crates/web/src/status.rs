//! This server's status, for administrators (ADR 0030): the devices it syncs with, what moved,
//! and crashes.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use tidefell_engine::{diagnostics, CheckLevel, PathKind};

use crate::format::{size, time};
use crate::handlers::{admin_only, check_csrf, render, signed_in, CsrfForm};
use crate::pages;
use crate::AppState;

type AppStateRef = State<Arc<AppState>>;

/// A crash of this server not dismissed yet.
pub(crate) fn unseen_crash(state: &AppState) -> Option<diagnostics::Crash> {
    diagnostics::crashes(state.engine.data_dir())
        .into_iter()
        .rfind(|c| !c.seen)
}

#[derive(serde::Deserialize)]
pub(crate) struct StatusQuery {
    /// Run the checks of `tidefell doctor` (ADR 0030).
    #[serde(default)]
    check: bool,
}

pub(crate) async fn status(
    State(state): AppStateRef,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<StatusQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session) {
        return response;
    }
    let peers = state
        .engine
        .peer_states()
        .into_iter()
        .map(|p| {
            let connection = if p.connected {
                let path = match p.path {
                    Some(PathKind::Direct) => "direct",
                    Some(PathKind::Relay) => "through a relay",
                    None => "no path now",
                };
                let rtt = p.rtt_ms.map(|ms| format!(", {ms} ms")).unwrap_or_default();
                format!("connected {path}{rtt}")
            } else {
                "not connected".to_owned()
            };
            pages::PeerRow {
                name: p.name.unwrap_or_else(|| "not seen yet".into()),
                device: p.device.to_string(),
                connected: p.connected,
                connection,
                since: if p.connected {
                    p.connected_since_ms.map(time)
                } else {
                    p.last_seen_ms.map(time)
                }
                .unwrap_or_default(),
                error: p.last_error,
            }
        })
        .collect();
    let t = state.engine.transfer();
    render(pages::Status {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        device: state.engine.id().to_string(),
        peers,
        sent: size(t.sent),
        received: size(t.received),
        send_rate: size(t.send_rate),
        receive_rate: size(t.receive_rate),
        checks: if query.check {
            // It reads the folders' disks, which can be slow or hang on a network mount.
            let engine = state.engine.clone();
            let checks = tokio::task::spawn_blocking(move || engine.doctor())
                .await
                .unwrap_or_default();
            Some(
                checks
                    .into_iter()
                    .map(|c| pages::CheckRow {
                        level: match c.level {
                            CheckLevel::Pass => "pass",
                            CheckLevel::Warning => "warning",
                            CheckLevel::Failure => "failure",
                        },
                        detail: c.detail,
                        advice: c.advice,
                    })
                    .collect(),
            )
        } else {
            None
        },
        crash: unseen_crash(&state).map(|c| pages::CrashNotice {
            id: c.id,
            time: time(c.time_ms),
            message: c.message,
            file: c.file.display().to_string(),
        }),
    })
}

#[derive(serde::Deserialize)]
pub(crate) struct CrashReportQuery {
    id: String,
}

/// A crash of this server written up as a new issue, to review and file (ADR 0030).
pub(crate) async fn crash_report(
    State(state): AppStateRef,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<CrashReportQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session) {
        return response;
    }
    let Some(report) = state.engine.crash_report(&query.id) else {
        return crate::handlers::not_found();
    };
    render(pages::CrashReport {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        title: report.title,
        body: report.body,
        url: report.url,
    })
}

/// Marks this server's crashes as seen.
pub(crate) async fn dismiss_crashes(
    State(state): AppStateRef,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session) {
        return response;
    }
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    if let Err(e) = diagnostics::dismiss_crashes(state.engine.data_dir()) {
        tracing::warn!(error = %e, "dismissing crashes failed");
    }
    Redirect::to("/status").into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct DiagnosticsQuery {
    /// Keep real paths and folder names.
    #[serde(default)]
    include_paths: bool,
}

/// Builds a diagnostics bundle off the async runtime (ADR 0030).
async fn bundle(
    state: &AppState,
    include_paths: bool,
) -> std::io::Result<Vec<tidefell_engine::BundleEntry>> {
    let engine = state.engine.clone();
    let options = tidefell_engine::BundleOptions {
        include_paths,
        app: None,
    };
    tokio::task::spawn_blocking(move || engine.diagnostics(&options))
        .await
        .map_err(std::io::Error::other)?
}

/// What a diagnostics bundle holds, to look at before downloading it.
pub(crate) async fn diagnostics(
    State(state): AppStateRef,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<DiagnosticsQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session) {
        return response;
    }
    const SHOWN: usize = 20_000;
    let entries = match bundle(&state, query.include_paths).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, "building a diagnostics bundle failed");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let entries = entries
        .into_iter()
        .map(|e| {
            let truncated = e.text.len() > SHOWN;
            let mut end = SHOWN.min(e.text.len());
            while !e.text.is_char_boundary(end) {
                end -= 1;
            }
            pages::BundleRow {
                size: size(e.text.len() as u64),
                name: e.name,
                text: e.text[..end].to_owned(),
                truncated,
            }
        })
        .collect();
    render(pages::Diagnostics {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        include_paths: query.include_paths,
        entries,
    })
}

/// Downloads a diagnostics bundle, built afresh.
pub(crate) async fn diagnostics_zip(
    State(state): AppStateRef,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<DiagnosticsQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session) {
        return response;
    }
    match bundle(&state, query.include_paths)
        .await
        .and_then(|entries| tidefell_engine::zip_bundle(&entries))
    {
        Ok(zip) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/zip"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"tidefell-diagnostics.zip\"",
                ),
            ],
            zip,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "building a diagnostics bundle failed");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
