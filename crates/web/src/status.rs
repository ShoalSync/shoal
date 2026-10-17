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
    let revocations = state.engine.revocations();
    let own_user = state
        .engine
        .certificate()
        .verify_certificate()
        .ok()
        .map(|c| c.user);
    let revoked_ids: Vec<_> = revocations.iter().map(|(r, _)| r.device).collect();
    let certificates = state.engine.certificates();
    let peers = state
        .engine
        .peer_states()
        .into_iter()
        .map(|p| {
            // Only the user's own devices: the key cannot revoke another user's.
            let revocable = state.user_key.is_some()
                && !revoked_ids.contains(&p.device)
                && user_of(&certificates, p.device).is_some_and(|u| Some(u) == own_user);
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
                revocable,
            }
        })
        .collect();
    let revoked = revocations
        .into_iter()
        .map(|(r, name)| pages::RevokedRow {
            name: name.unwrap_or_else(|| "a device".into()),
            device: r.device.to_string(),
            when: time(r.revoked_ms),
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
        revoked,
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

/// The user a device belongs to, by the certificate this server keeps for it.
fn user_of(
    certificates: &[tidefell_engine::Signed<tidefell_engine::DeviceCertificate>],
    device: tidefell_engine::DeviceId,
) -> Option<tidefell_engine::UserId> {
    certificates
        .iter()
        .filter_map(|c| c.verify_certificate().ok())
        .find(|c| c.device == device)
        .map(|c| c.user)
}

#[derive(serde::Deserialize)]
pub(crate) struct RevokeQuery {
    device: String,
    /// When the device was lost, as a form's date and time, and its offset from UTC: given, the
    /// page shows what revoking with it would withdraw (ADR 0040).
    #[serde(default)]
    lost_at: String,
    #[serde(default)]
    offset: String,
}

/// What revoking `device` as lost at `lost_ms` would withdraw here, folder by folder.
fn loss_preview(
    state: &AppState,
    device: tidefell_engine::DeviceId,
    lost_ms: i64,
) -> pages::LossPreview {
    // A page lists this many files of a folder at most.
    const LISTED: usize = 200;
    let folders = state.engine.folders();
    let name = |id| {
        folders
            .iter()
            .find(|f| f.id == id)
            .map_or_else(|| id.to_string(), crate::handlers::folder_name)
    };
    pages::LossPreview {
        lost_ms,
        when: time(lost_ms),
        folders: state
            .engine
            .revoke_preview(device, lost_ms)
            .into_iter()
            .map(|p| pages::LossFolderRow {
                name: name(p.folder),
                moved_to: p.moved.then(|| time(p.lost_ms)),
                more: p.withdrawn.len().saturating_sub(LISTED),
                withdrawn: p
                    .withdrawn
                    .into_iter()
                    .take(LISTED)
                    .map(|w| pages::WithdrawnRow {
                        path: w.path,
                        size: size(w.size),
                        received: w.received_ms.map_or_else(|| "not recorded".into(), time),
                        put_back: w.put_back,
                    })
                    .collect(),
                unknown: p.unknown,
            })
            .collect(),
    }
}

/// Which peer `device` names, if this server may revoke it: it holds the user key, and the
/// device is the user's and not revoked yet.
fn revocable(state: &AppState, device: &str) -> Option<(tidefell_engine::DeviceId, String)> {
    state.user_key.as_ref()?;
    let device: tidefell_engine::DeviceId = device.parse().ok()?;
    if device == state.engine.id()
        || state
            .engine
            .revocations()
            .iter()
            .any(|(r, _)| r.device == device)
    {
        return None;
    }
    let own_user = state.engine.certificate().verify_certificate().ok()?.user;
    let peer = state
        .engine
        .peer_states()
        .into_iter()
        .find(|p| p.device == device)?;
    (user_of(&state.engine.certificates(), device) == Some(own_user))
        .then(|| (device, peer.name.unwrap_or_else(|| "this device".into())))
}

/// Asks before revoking a device (ADR 0040).
pub(crate) async fn revoke_page(
    State(state): AppStateRef,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RevokeQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session) {
        return response;
    }
    let Some((device, name)) = revocable(&state, &query.device) else {
        return Redirect::to("/status").into_response();
    };
    let offset = match query.offset.as_str() {
        "" => "+00:00".to_owned(),
        o => o.to_owned(),
    };
    let (preview, error) = match query.lost_at.trim() {
        "" => (None, None),
        at => match crate::format::parse_time(at, &offset) {
            Some(lost_ms) if lost_ms > state.engine.now_ms() => {
                (None, Some("That time has not come yet.".to_owned()))
            }
            Some(lost_ms) => (Some(loss_preview(&state, device, lost_ms)), None),
            None => (None, Some(format!("{at:?} is not a date and time."))),
        },
    };
    render(pages::Revoke {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        name,
        device: device.to_string(),
        now: time(state.engine.now_ms()),
        lost_at: query.lost_at,
        offset,
        offsets: crate::format::offsets(),
        error,
        preview,
    })
}

#[derive(serde::Deserialize)]
pub(crate) struct RevokeForm {
    csrf: String,
    device: String,
    /// The time of loss the page previewed, in milliseconds, if one was given.
    #[serde(default)]
    lost_ms: Option<i64>,
}

/// Revokes one of the user's devices for good (ADR 0040).
pub(crate) async fn revoke(
    State(state): AppStateRef,
    headers: HeaderMap,
    Form(form): Form<RevokeForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session).or_else(|| check_csrf(&session, &form.csrf)) {
        return response;
    }
    if let (Some((device, name)), Some(user)) = (revocable(&state, &form.device), &state.user_key) {
        // With a time of loss, what the page previewed is withdrawn (ADR 0040).
        match state.engine.revoke_device(user, device, form.lost_ms) {
            Ok(()) => {
                tracing::warn!(%device, %name, by = %session.username, "device revoked from the web peer")
            }
            Err(e) => tracing::warn!(%device, error = %e, "revoking a device failed"),
        }
    }
    Redirect::to("/status").into_response()
}
