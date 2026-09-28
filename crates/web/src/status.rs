//! This server's status, for administrators (ADR 0030): the devices it syncs with, what moved,
//! and crashes.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use tidefell_engine::{diagnostics, PathKind};

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

pub(crate) async fn status(State(state): AppStateRef, headers: HeaderMap) -> Response {
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
        crash: unseen_crash(&state).map(|c| pages::CrashNotice {
            time: time(c.time_ms),
            message: c.message,
            file: c.file.display().to_string(),
        }),
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
