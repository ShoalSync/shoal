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
use tidefell_engine::FolderInfo;
use tidefell_proto::{EntryId, FolderId, UserId};

use crate::format::{query_value, size, time};
use crate::pages;
use crate::sessions::{same_secret, Session, SESSION_TTL};
use crate::AppState;

type AppStateRef = State<Arc<AppState>>;

const COOKIE: &str = "tidefell_session";

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
    let verified = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || state.accounts.verify(&form.username, &form.password))
            .await
    };
    let account = match verified {
        Ok(Ok(Some(account))) => account,
        Ok(Ok(None)) => {
            // Slow down guessing.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut response = render(pages::Login {
                error: Some("Wrong username or password.".into()),
            });
            *response.status_mut() = StatusCode::UNAUTHORIZED;
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
        HeaderValue::from_static("tidefell_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
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
        })
        .collect();
    render(pages::Folders {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        share_links: state.config.share_links,
        folders: rows,
    })
}

#[derive(Deserialize)]
pub(crate) struct FolderQuery {
    uploaded: Option<usize>,
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
            on_disk: f.on_disk,
            conflicts: f.conflicts,
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
        notice: query
            .uploaded
            .map(|n| format!("Uploaded {n} file(s). They sync to every device.")),
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
        Err(tidefell_engine::Error::Unavailable(_)) => {
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
    if let Err(e) = state.engine.restore(folder.id, entry) {
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

// ---- Account administration ----

/// A rejection page unless the session is an administrator's.
fn admin_only(session: &Session) -> Option<Response> {
    if session.admin {
        None
    } else {
        Some(error(
            StatusCode::FORBIDDEN,
            "Administrators only",
            "Ask an administrator to manage accounts.",
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
