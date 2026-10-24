//! Page and form handlers.

use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use shoal_engine::{FolderInfo, VersionKind};
use shoal_proto::{EntryId, FolderId, UserId};

use crate::format::{query_value, size, time};
use crate::pages;
use crate::sessions::{same_secret, Session, SESSION_TTL};
use crate::AppState;

type AppStateRef = State<Arc<AppState>>;

const COOKIE: &str = "shoal_session";

pub(crate) fn render(page: impl Template) -> Response {
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(crate) fn error(status: StatusCode, title: &str, message: &str) -> Response {
    let mut response = render(pages::Error {
        title: title.to_string(),
        message: message.to_string(),
    });
    *response.status_mut() = status;
    response
}

pub(crate) fn not_found() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "Not found",
        "That folder or file does not exist, or is not shared with you.",
    )
}

pub(crate) fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE).then(|| value.to_string())
        })
}

/// The signed-in session, if the cookie is valid and the account still exists.
pub(crate) fn session(state: &AppState, headers: &HeaderMap) -> Option<(String, Session)> {
    let token = session_token(headers)?;
    let session = state.sessions.get(&token)?;
    match state.accounts.get(&session.username) {
        Ok(Some(account)) if account.user_id() == Some(session.user) => Some((token, session)),
        _ => {
            state.sessions.remove(&token);
            None
        }
    }
}

macro_rules! signed_in {
    ($state:expr, $headers:expr) => {
        match $crate::handlers::session(&$state, &$headers) {
            Some(found) => found,
            None => {
                return axum::response::IntoResponse::into_response(axum::response::Redirect::to(
                    "/login",
                ))
            }
        }
    };
}
pub(crate) use signed_in;

/// A rejection page if the form's CSRF token is wrong.
pub(crate) fn check_csrf(session: &Session, submitted: &str) -> Option<Response> {
    if same_secret(&session.csrf, submitted) {
        None
    } else {
        Some(error(
            StatusCode::FORBIDDEN,
            "Form expired",
            "Please go back, reload the page and try again.",
        ))
    }
}

/// Folders on this server that `user` is a member of.
fn visible_folders(state: &AppState, user: &UserId) -> Vec<FolderInfo> {
    state
        .engine
        .folders()
        .into_iter()
        .filter(|f| f.members.contains(user))
        .collect()
}

pub(crate) fn visible_folder(state: &AppState, session: &Session, id: &str) -> Option<FolderInfo> {
    let id: FolderId = id.parse().ok()?;
    visible_folders(state, &session.user)
        .into_iter()
        .find(|f| f.id == id)
}

pub(crate) fn folder_name(folder: &FolderInfo) -> String {
    folder
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.path.display().to_string())
}

// ---- Sign in and out ----

/// "Try again in 40 seconds."
pub(crate) fn try_again_in(wait: Duration) -> String {
    let seconds = wait.as_secs().max(1);
    format!(
        "Too many attempts. Try again in {seconds} second{}.",
        if seconds == 1 { "" } else { "s" }
    )
}

pub(crate) fn retry_after(response: &mut Response, wait: Duration) {
    if let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
}

/// A page refused because its key must wait: 429, saying how long.
pub(crate) fn too_many_attempts(wait: Duration, page: impl askama::Template) -> Response {
    let mut response = render(page);
    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    retry_after(&mut response, wait);
    response
}

pub(crate) async fn login_page(State(state): AppStateRef, headers: HeaderMap) -> Response {
    if session(&state, &headers).is_some() {
        return Redirect::to("/").into_response();
    }
    render(pages::Login { error: None })
}

#[derive(Deserialize)]
pub(crate) struct LoginForm {
    username: String,
    password: String,
}

pub(crate) async fn login(State(state): AppStateRef, Form(form): Form<LoginForm>) -> Response {
    let throttled = format!("login:{}", form.username.trim().to_lowercase());
    let wait = match state.throttle.begin(&throttled) {
        Ok(wait) => wait,
        Err(wait) => {
            return too_many_attempts(
                wait,
                pages::Login {
                    error: Some(try_again_in(wait)),
                },
            );
        }
    };
    let verified = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || state.accounts.verify(&form.username, &form.password))
            .await
    };
    let account = match verified {
        Ok(Ok(Some(account))) => account,
        Ok(Ok(None)) => {
            // Slow down guessing: a fixed wait, and a growing one once it keeps failing.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut response = render(pages::Login {
                error: Some("Wrong username or password.".into()),
            });
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            if !wait.is_zero() {
                retry_after(&mut response, wait);
            }
            return response;
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "reading accounts failed");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Server error",
                "Accounts are unavailable.",
            );
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some(user) = account.user_id() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Server error",
            "This account is misconfigured.",
        );
    };
    state.throttle.cleared(&throttled);
    let token = state.sessions.create(account.username, user, account.admin);
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        SESSION_TTL.as_secs()
    );
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie is ASCII"),
    );
    response
}

#[derive(Deserialize)]
pub(crate) struct CsrfForm {
    pub(crate) csrf: String,
}

pub(crate) async fn logout(
    State(state): AppStateRef,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let (token, session) = signed_in!(state, headers);
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    state.sessions.remove(&token);
    let mut response = Redirect::to("/login").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("shoal_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    response
}

// ---- Folders and files ----

pub(crate) async fn folders(State(state): AppStateRef, headers: HeaderMap) -> Response {
    let (_, session) = signed_in!(state, headers);
    let rows = visible_folders(&state, &session.user)
        .into_iter()
        .map(|f| pages::FolderRow {
            id: f.id.to_string(),
            name: folder_name(&f),
            files: state.engine.files(f.id).map(|v| v.len()).unwrap_or(0),
            recycled: state.engine.recycle_bin(f.id).map(|v| v.len()).unwrap_or(0),
            state: folder_state(&f),
            problems: f.problems,
        })
        .collect();
    render(pages::Folders {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        share_links: state.config.share_links,
        paused: state.engine.paused(),
        crashed: session.admin && crate::status::unseen_crash(&state).is_some(),
        folders: rows,
    })
}

/// What failed and why, such as "write: permission denied".
fn problem_what(p: &shoal_engine::PathProblem) -> String {
    use shoal_engine::{Operation, ProblemKind};
    let operation = match p.operation {
        Operation::Scan => "read",
        Operation::Fetch => "fetch",
        Operation::Write => "write",
        Operation::Delete => "delete",
    };
    let kind = match p.kind {
        ProblemKind::PermissionDenied => "permission denied",
        ProblemKind::DiskFull => "disk full",
        ProblemKind::ContentUnavailable => "no connected device has it",
        ProblemKind::NameNotAllowed => "name not allowed here",
        ProblemKind::ChangedWhileWriting => "changed while in use",
        ProblemKind::Other => "failed",
    };
    format!("{operation}: {kind}")
}

/// What a folder is doing, in a few words: "syncing: 3 files, 2.0 MiB to fetch" (ADR 0030).
pub(crate) fn folder_state(f: &FolderInfo) -> String {
    use shoal_engine::FolderState;
    let mut line = match f.state {
        FolderState::Idle => "up to date".to_owned(),
        FolderState::Scanning => "looking for changes".to_owned(),
        FolderState::Syncing => "syncing".to_owned(),
        FolderState::Paused => "paused".to_owned(),
        FolderState::Rebuilding => {
            let waiting = f
                .rebuild
                .as_ref()
                .map(|r| r.waiting_for.len())
                .unwrap_or_default();
            match waiting {
                0 => "rebuilding: waiting for a device sharing the folder".to_owned(),
                n => format!("rebuilding: waiting for {n} device(s) to send their files"),
            }
        }
        FolderState::Error => format!("not syncing: {}", f.error.as_deref().unwrap_or("an error")),
    };
    if f.held_deletes > 0 {
        line = format!(
            "{line}: {} deleted files wait for a decision",
            f.held_deletes
        );
    }
    if f.pending_files > 0 && f.state != FolderState::Error {
        line = format!(
            "{line}: {} file(s), {} to fetch",
            f.pending_files,
            size(f.pending_bytes)
        );
    }
    line
}

#[derive(Deserialize)]
pub(crate) struct FolderQuery {
    uploaded: Option<usize>,
    /// A file whose conflict was just decided.
    resolved: Option<String>,
}

pub(crate) async fn folder(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<FolderQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let files = state
        .engine
        .files(folder.id)
        .unwrap_or_default()
        .into_iter()
        .map(|f| pages::FileRow {
            path_query: query_value(&f.path),
            path: f.path,
            size: size(f.size),
            modified: time(f.modified_ms),
            by: if f.author == state.engine.id() {
                "this server".to_owned()
            } else {
                f.author_name
            },
            on_disk: f.on_disk,
            conflicts: f.conflicts,
            only_here: f.only_here,
            pinned: f.pinned,
        })
        .collect();
    render(pages::Folder {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        id: folder.id.to_string(),
        name: folder_name(&folder),
        recycled: state
            .engine
            .recycle_bin(folder.id)
            .map(|v| v.len())
            .unwrap_or(0),
        share_links: state.config.share_links,
        paused: folder.paused,
        device_paused: state.engine.paused(),
        held_deletes: folder.held_deletes,
        held_withdrawals: held_withdrawals(&state, folder.id),
        low_space: folder.low_space.clone(),
        partial: matches!(folder.mode, shoal_engine::ReplicaMode::Partial { .. }),
        pinned_size: size(folder.pinned_bytes),
        conflicts: folder.conflicts,
        state: {
            let mut line = folder_state(&folder);
            if let Some(at) = folder.last_scan_ms {
                line = format!("{line}. Last looked for changes {}.", time(at));
            }
            // When and why its metadata was last started over (ADR 0034).
            if let Some(r) = &folder.rebuild {
                let when = match r.finished_ms {
                    Some(at) => format!("Rebuilt {}", time(at)),
                    None => format!("Rebuilding since {}", time(r.started_ms)),
                };
                line = format!("{line} {when}, as {}.", r.reason);
            }
            line
        },
        problems: state
            .engine
            .path_problems(folder.id)
            .unwrap_or_default()
            .into_iter()
            .map(|p| pages::ProblemRow {
                what: problem_what(&p),
                // A name this server's drive cannot store can be renamed everywhere (ADR 0033).
                rename: p.kind == shoal_engine::ProblemKind::NameNotAllowed,
                path: p.path,
                message: p.message,
                since: time(p.since_ms),
                attempts: p.attempts,
            })
            .collect(),
        notice: query
            .uploaded
            .map(|n| format!("Uploaded {n} file(s). They sync to every device."))
            .or(query
                .resolved
                .map(|p| format!("Decided {p}. Every device follows."))),
        files,
    })
}

#[derive(Deserialize)]
pub(crate) struct DownloadQuery {
    path: String,
}

pub(crate) async fn download(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<DownloadQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    match open_file(&state, folder.id, &query.path).await {
        Ok(file) => send_file(file, &query.path).await,
        Err(response) => *response,
    }
}

/// Opens a live file of `folder` for download. Only live files known to the folder's metadata
/// are served, from where the engine put them. A placeholder is fetched from peers first. The
/// open file keeps its data even if the engine evicts it once unpinned.
pub(crate) async fn open_file(
    state: &AppState,
    folder: FolderId,
    path: &str,
) -> Result<tokio::fs::File, Box<Response>> {
    let local = match state.engine.open(folder, path).await {
        Ok(local) => local,
        Err(shoal_engine::Error::Unavailable(_)) => {
            return Err(Box::new(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "This file is not available right now: no connected device has it.",
                )
                    .into_response(),
            ))
        }
        Err(_) => return Err(Box::new(not_found())),
    };
    let opened = tokio::fs::File::open(&local).await;
    let _ = state.engine.close(folder, path);
    opened.map_err(|_| Box::new(not_found()))
}

/// Streams an opened file as an attachment named after the last component of `path`.
pub(crate) async fn send_file(file: tokio::fs::File, path: &str) -> Response {
    let name = FsPath::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().replace(['"', '\\', '\r', '\n'], "_"))
        .unwrap_or_else(|| "download".into());
    let length = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, length.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        body,
    )
        .into_response()
}

// ---- Recycle bin ----

#[derive(Deserialize)]
pub(crate) struct NoticeQuery {
    restored: Option<String>,
}

pub(crate) async fn recycle(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<NoticeQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let items = state
        .engine
        .recycle_bin(folder.id)
        .unwrap_or_default()
        .into_iter()
        .map(|r| pages::RecycledRow {
            conflict: r.conflict,
            entry: r.entry.to_string(),
            path: r.path,
            size: size(r.size),
            deleted: time(r.deleted_at_ms),
            expires: r.expires_at_ms.map_or("never".into(), time),
        })
        .collect();
    render(pages::Recycle {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        id: folder.id.to_string(),
        name: folder_name(&folder),
        notice: query
            .restored
            .map(|p| format!("Restored {p}. It will reappear on every device.")),
        items,
    })
}

/// Changes this server applied to the folder, newest first.
const ACTIVITY_SHOWN: usize = 500;

pub(crate) async fn activity(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let items = state
        .engine
        .activity(folder.id, ACTIVITY_SHOWN)
        .unwrap_or_default()
        .into_iter()
        .map(|a| {
            use shoal_engine::ChangeKind;
            pages::ActivityRow {
                time: time(a.time_ms),
                what: match a.kind {
                    ChangeKind::Added => "added",
                    ChangeKind::Changed => "changed",
                    ChangeKind::Deleted => "deleted",
                    ChangeKind::Renamed => "renamed",
                    ChangeKind::Restored => "restored",
                    ChangeKind::Conflict => "conflict",
                },
                path: a.path,
                old_path: a.old_path,
                by: if a.local {
                    "this server".to_owned()
                } else {
                    a.device_name
                },
            }
        })
        .collect();
    render(pages::Activity {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        id: folder.id.to_string(),
        name: folder_name(&folder),
        items,
    })
}

pub(crate) async fn restore(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path((id, entry)): Path<(String, String)>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let Ok(entry) = entry.parse::<EntryId>() else {
        return not_found();
    };
    let bin = state.engine.recycle_bin(folder.id).unwrap_or_default();
    let Some(item) = bin.iter().find(|r| r.entry == entry) else {
        return not_found();
    };
    if let Err(e) = state.engine.restore(folder.id, entry).await {
        tracing::warn!(error = %e, "restore failed");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Restore failed",
            &e.to_string(),
        );
    }
    tracing::info!(user = %session.username, folder = %folder.id, path = %item.path, "restored from the web");
    Redirect::to(&format!(
        "/folders/{}/recycle?restored={}",
        folder.id,
        query_value(&item.path)
    ))
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct HistoryQuery {
    path: String,
    #[serde(default)]
    restored: bool,
}

/// A file's versions: the current one and those this server kept when it was replaced.
pub(crate) async fn history(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let Ok(versions) = state.engine.versions(folder.id, &query.path) else {
        return not_found();
    };
    // Conflicting versions have a screen of their own.
    let versions = versions
        .into_iter()
        .filter(|v| v.kind != VersionKind::Conflict)
        .map(|v| pages::VersionRow {
            current: v.kind == VersionKind::Current,
            id: v.id,
            size: size(v.size),
            modified: time(v.modified_ms),
            kept: v.kept_ms.map_or(String::new(), time),
            expires: v.expires_ms.map_or("never".into(), time),
        })
        .collect();
    render(pages::History {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        id: folder.id.to_string(),
        name: folder_name(&folder),
        notice: query
            .restored
            .then(|| "Restored. The file changes back on every device.".to_owned()),
        path: query.path,
        versions,
    })
}

#[derive(Deserialize)]
pub(crate) struct RestoreVersionForm {
    csrf: String,
    path: String,
    version: String,
}

pub(crate) async fn restore_version(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RestoreVersionForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    if let Err(e) = state
        .engine
        .restore_version(folder.id, &form.path, &form.version)
        .await
    {
        tracing::warn!(error = %e, "restoring a version failed");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Restore failed",
            &e.to_string(),
        );
    }
    tracing::info!(user = %session.username, folder = %folder.id, path = %form.path, "restored a version from the web");
    Redirect::to(&format!(
        "/folders/{}/history?path={}&restored=true",
        folder.id,
        query_value(&form.path)
    ))
    .into_response()
}

// ---- Pins (ADR 0029) ----

#[derive(Deserialize)]
pub(crate) struct RenameForm {
    csrf: String,
    from: String,
    to: String,
}

/// Renames a file on every device, as offered for a name this server's drive cannot store
/// (ADR 0033).
pub(crate) async fn rename(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RenameForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    if let Err(e) = state.engine.rename(folder.id, &form.from, &form.to).await {
        return error(
            StatusCode::BAD_REQUEST,
            "Could not rename it",
            &e.to_string(),
        );
    }
    Redirect::to(&format!("/folders/{}", folder.id)).into_response()
}

#[derive(Deserialize)]
pub(crate) struct PinForm {
    csrf: String,
    path: String,
}

async fn set_pin(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    form: &PinForm,
    pin: bool,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    let Some(folder) = visible_folder(state, &session, id) else {
        return not_found();
    };
    let result = if pin {
        state.engine.pin(folder.id, &form.path).await
    } else {
        state.engine.unpin(folder.id, &form.path).await
    };
    if let Err(e) = result {
        return error(
            StatusCode::BAD_REQUEST,
            "Could not change the pin",
            &e.to_string(),
        );
    }
    Redirect::to(&format!("/folders/{}", folder.id)).into_response()
}

pub(crate) async fn pin(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<PinForm>,
) -> Response {
    set_pin(&state, &headers, &id, &form, true).await
}

pub(crate) async fn unpin(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<PinForm>,
) -> Response {
    set_pin(&state, &headers, &id, &form, false).await
}

// ---- Held withdrawals (ADR 0040) ----

/// The revoked devices whose changes after their loss wait for a decision in `folder`.
fn held_withdrawals(
    state: &AppState,
    folder: shoal_engine::FolderId,
) -> Vec<pages::HeldWithdrawalRow> {
    const SHOWN: usize = 10;
    let revocations = state.engine.revocations();
    state
        .engine
        .held_withdrawals(folder)
        .into_iter()
        .map(|(device, paths)| pages::HeldWithdrawalRow {
            name: revocations
                .iter()
                .find(|(r, _)| r.device == device)
                .and_then(|(_, name)| name.clone())
                .unwrap_or_else(|| "A revoked device".into()),
            device: device.to_string(),
            count: paths.len(),
            more: paths.len().saturating_sub(SHOWN),
            paths: paths.into_iter().take(SHOWN).collect(),
        })
        .collect()
}

#[derive(Deserialize)]
pub(crate) struct WithdrawalsForm {
    csrf: String,
    device: String,
    /// `withdraw` or `keep`.
    action: String,
}

/// Withdraws what a revoked device changed after its loss, or keeps it as if made before.
/// Administrators only, since what is withdrawn goes from every device.
pub(crate) async fn decide_withdrawals(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<WithdrawalsForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session).or_else(|| check_csrf(&session, &form.csrf)) {
        return response;
    }
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let Ok(device) = form.device.parse::<shoal_engine::DeviceId>() else {
        return error(StatusCode::BAD_REQUEST, "Unknown device", &form.device);
    };
    let withdraw = match form.action.as_str() {
        "withdraw" => true,
        "keep" => false,
        _ => return error(StatusCode::BAD_REQUEST, "Unknown action", &form.action),
    };
    if let Err(e) = state.engine.decide_withdrawals(folder.id, device, withdraw) {
        return error(
            StatusCode::BAD_REQUEST,
            "Could not decide on the withdrawals",
            &e.to_string(),
        );
    }
    tracing::info!(user = %session.username, folder = %folder.id, %device, action = %form.action, "held withdrawals decided from the web");
    Redirect::to(&format!("/folders/{}", folder.id)).into_response()
}

// ---- Held deletes (ADR 0037) ----

#[derive(Deserialize)]
pub(crate) struct DeletesForm {
    csrf: String,
    /// `confirm` or `restore`.
    action: String,
}

/// Confirms the deletes a scan held, or brings the files back. Administrators only, since the
/// deletes reach every device.
pub(crate) async fn decide_deletes(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DeletesForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session).or_else(|| check_csrf(&session, &form.csrf)) {
        return response;
    }
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let result = match form.action.as_str() {
        "confirm" => state.engine.confirm_deletes(folder.id).await,
        "restore" => state.engine.restore_deleted(folder.id).await,
        _ => return error(StatusCode::BAD_REQUEST, "Unknown action", &form.action),
    };
    if let Err(e) = result {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not decide on the deletes",
            &e.to_string(),
        );
    }
    tracing::info!(user = %session.username, folder = %folder.id, action = %form.action, "held deletes decided from the web");
    Redirect::to(&format!("/folders/{}", folder.id)).into_response()
}

// ---- Pausing (ADR 0029) ----

/// Pauses or resumes syncing of a folder, or of the whole server. Administrators only, since it
/// affects everyone using this server.
async fn set_paused(
    state: &AppState,
    headers: &HeaderMap,
    folder: Option<&str>,
    form: &CsrfForm,
    paused: bool,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session).or_else(|| check_csrf(&session, &form.csrf)) {
        return response;
    }
    let target = match folder {
        Some(id) => match visible_folder(state, &session, id) {
            Some(folder) => Some(folder.id),
            None => return not_found(),
        },
        None => None,
    };
    let result = if paused {
        state.engine.pause(target)
    } else {
        state.engine.resume(target)
    };
    if let Err(e) = result {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not change syncing",
            &e.to_string(),
        );
    }
    tracing::info!(user = %session.username, folder = ?target, paused, "sync pause changed from the web");
    match target {
        Some(id) => Redirect::to(&format!("/folders/{id}")).into_response(),
        None => Redirect::to("/").into_response(),
    }
}

pub(crate) async fn pause_folder(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    set_paused(&state, &headers, Some(&id), &form, true).await
}

pub(crate) async fn resume_folder(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    set_paused(&state, &headers, Some(&id), &form, false).await
}

pub(crate) async fn pause_device(
    State(state): AppStateRef,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    set_paused(&state, &headers, None, &form, true).await
}

pub(crate) async fn resume_device(
    State(state): AppStateRef,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    set_paused(&state, &headers, None, &form, false).await
}

// ---- Account administration ----

/// A rejection page unless the session is an administrator's.
pub(crate) fn admin_only(session: &Session) -> Option<Response> {
    if session.admin {
        None
    } else {
        Some(error(
            StatusCode::FORBIDDEN,
            "Administrators only",
            "Only an administrator can see this page.",
        ))
    }
}

fn accounts_page(state: &AppState, session: Session, notice: Option<String>) -> Response {
    let accounts = match state.accounts.list() {
        Ok(accounts) => accounts,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Server error",
                &e.to_string(),
            )
        }
    };
    render(pages::Accounts {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        notice,
        accounts: accounts
            .into_iter()
            .map(|a| pages::AccountRow {
                username: a.username,
                user: a.user,
                admin: a.admin,
            })
            .collect(),
    })
}

pub(crate) async fn accounts(State(state): AppStateRef, headers: HeaderMap) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session) {
        return response;
    }
    accounts_page(&state, session, None)
}

#[derive(Deserialize)]
pub(crate) struct AccountForm {
    csrf: String,
    username: String,
    password: String,
    user: String,
    #[serde(default)]
    admin: Option<String>,
}

pub(crate) async fn save_account(
    State(state): AppStateRef,
    headers: HeaderMap,
    Form(form): Form<AccountForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session).or_else(|| check_csrf(&session, &form.csrf)) {
        return response;
    }
    let Ok(user) = form.user.parse::<UserId>() else {
        return accounts_page(
            &state,
            session,
            Some("The user ID must be 64 lowercase hex digits.".into()),
        );
    };
    let saved = {
        let state = state.clone();
        let (name, password, admin) = (form.username.clone(), form.password, form.admin.is_some());
        tokio::task::spawn_blocking(move || state.accounts.upsert(&name, &password, user, admin))
            .await
    };
    let notice = match saved {
        Ok(Ok(())) => {
            // A changed password or role takes effect at the next sign-in.
            if form.username != session.username {
                state.sessions.remove_user(&form.username);
            }
            format!("Saved account {}.", form.username)
        }
        Ok(Err(e)) => e.to_string(),
        Err(_) => "Saving the account failed.".into(),
    };
    accounts_page(&state, session, Some(notice))
}

pub(crate) async fn delete_account(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(username): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = admin_only(&session).or_else(|| check_csrf(&session, &form.csrf)) {
        return response;
    }
    if username == session.username {
        return accounts_page(
            &state,
            session,
            Some("You cannot remove your own account.".into()),
        );
    }
    let notice = match state.accounts.remove(&username) {
        Ok(true) => {
            state.sessions.remove_user(&username);
            format!("Removed account {username}.")
        }
        Ok(false) => format!("There is no account {username}."),
        Err(e) => e.to_string(),
    };
    accounts_page(&state, session, Some(notice))
}

// ---- Static assets ----

pub(crate) async fn style() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("../static/style.css"),
    )
        .into_response()
}

pub(crate) async fn htmx() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../static/htmx.min.js"),
    )
        .into_response()
}
