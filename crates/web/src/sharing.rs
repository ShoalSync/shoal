//! Uploads and share links (ADR 0019).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use tidefell_engine::{FileInfo, FolderInfo};
use tidefell_proto::FolderId;
use tokio::io::AsyncWriteExt;

use crate::format::{query_value, size, time};
use crate::handlers::{
    check_csrf, error, folder_name, not_found, open_file, render, send_file, signed_in,
    visible_folder, CsrfForm,
};
use crate::pages;
use crate::shares::{NewShare, Share, UNLOCK_TTL};
use crate::AppState;

type AppStateRef = State<Arc<AppState>>;

/// Cookie remembering that this browser entered a link's password. Scoped to the link's path.
const LINK_COOKIE: &str = "tidefell_link";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

// ---- Uploads ----

/// The folder path for an upload named `file_name` into directory `dir`, or `None` if either
/// is not acceptable. Browsers may send a full client path as the file name; only its last
/// component is used.
fn upload_path(dir: &str, file_name: &str) -> Option<String> {
    let name = file_name.rsplit(['/', '\\']).next()?.trim();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in dir.split('/').map(str::trim).filter(|p| !p.is_empty()) {
        if part == "." || part == ".." {
            return None;
        }
        parts.push(part);
    }
    // The engine's own directory at the folder root.
    if parts.first().copied().unwrap_or(name) == ".tidefell" {
        return None;
    }
    parts.push(name);
    let path = parts.join("/");
    tidefell_proto::sync::is_valid_path(&path).then_some(path)
}

fn random_suffix() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the OS provides randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Streams one uploaded file to a staging file in the folder's reserved directory, then hands
/// it to the engine, which moves it into place and records the change.
async fn store_upload(
    state: &AppState,
    folder: &FolderInfo,
    path: &str,
    field: &mut axum::extract::multipart::Field<'_>,
) -> Result<(), String> {
    let staging_dir = folder.path.join(".tidefell").join("tmp");
    tokio::fs::create_dir_all(&staging_dir)
        .await
        .map_err(|e| e.to_string())?;
    let staging = staging_dir.join(format!("upload-{}", random_suffix()));
    let written = async {
        let mut out = tokio::fs::File::create(&staging)
            .await
            .map_err(|e| e.to_string())?;
        while let Some(chunk) = field.chunk().await.map_err(|e| e.to_string())? {
            out.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        out.sync_all().await.map_err(|e| e.to_string())
    }
    .await;
    let stored = match written {
        Ok(()) => state
            .engine
            .write(folder.id, path, &staging)
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    if stored.is_err() {
        let _ = tokio::fs::remove_file(&staging).await;
    }
    stored
}

/// Uploads files into a folder. The form sends `csrf` first, then an optional `dir`
/// (created as needed), then one or more `file` parts.
pub(crate) async fn upload(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let bad_form = || {
        error(
            StatusCode::BAD_REQUEST,
            "Upload failed",
            "The upload was interrupted or malformed. Please try again.",
        )
    };
    let mut csrf_ok = false;
    let mut dir = String::new();
    let mut uploaded = 0usize;
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return bad_form(),
        };
        match field.name() {
            Some("csrf") => {
                let Ok(value) = field.text().await else {
                    return bad_form();
                };
                if let Some(response) = check_csrf(&session, &value) {
                    return response;
                }
                csrf_ok = true;
            }
            Some("dir") => {
                let Ok(value) = field.text().await else {
                    return bad_form();
                };
                dir = value;
            }
            Some("file") => {
                // Nothing is written before the form is known to be genuine.
                if !csrf_ok {
                    return check_csrf(&session, "").unwrap_or_else(bad_form);
                }
                let Some(file_name) = field.file_name().map(str::to_owned) else {
                    continue;
                };
                if file_name.is_empty() {
                    // The form was sent without choosing a file.
                    continue;
                }
                let Some(path) = upload_path(&dir, &file_name) else {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "Upload failed",
                        &format!("{file_name:?} in {dir:?} is not a valid name for a file here."),
                    );
                };
                if let Err(e) = store_upload(&state, &folder, &path, &mut field).await {
                    tracing::warn!(folder = %folder.id, %path, error = %e, "upload failed");
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Upload failed",
                        &format!("Saving {path} failed."),
                    );
                }
                tracing::info!(user = %session.username, folder = %folder.id, %path, "uploaded from the web");
                uploaded += 1;
            }
            _ => {}
        }
    }
    if !csrf_ok {
        return check_csrf(&session, "").unwrap_or_else(bad_form);
    }
    Redirect::to(&format!("/folders/{}?uploaded={uploaded}", folder.id)).into_response()
}

// ---- Creating and managing links ----

/// Whether `path` in `files` is a file (`Some(false)`), a directory (`Some(true)`, including
/// "" for the whole folder), or neither.
fn target_kind(files: &[FileInfo], path: &str) -> Option<bool> {
    if files.iter().any(|f| f.path == path) {
        return Some(false);
    }
    let prefix = format!("{path}/");
    (path.is_empty() || files.iter().any(|f| f.path.starts_with(&prefix))).then_some(true)
}

fn links_disabled() -> Response {
    not_found()
}

#[derive(Deserialize)]
pub(crate) struct ShareQuery {
    #[serde(default)]
    path: String,
}

pub(crate) async fn new_share(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<ShareQuery>,
) -> Response {
    if !state.config.share_links {
        return links_disabled();
    }
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let files = state.engine.files(folder.id).unwrap_or_default();
    let path = query.path.trim_matches('/').to_string();
    let Some(directory) = target_kind(&files, &path) else {
        return not_found();
    };
    render(pages::ShareNew {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        id: folder.id.to_string(),
        name: folder_name(&folder),
        path,
        directory,
        error: None,
        created: None,
    })
}

#[derive(Deserialize)]
pub(crate) struct ShareForm {
    csrf: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    expires_days: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    max_downloads: String,
}

/// Parses an optional positive whole number from a form field.
fn optional_count(value: &str) -> Result<Option<u32>, ()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse::<u32>() {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => Err(()),
    }
}

pub(crate) async fn create_share(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ShareForm>,
) -> Response {
    if !state.config.share_links {
        return links_disabled();
    }
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let files = state.engine.files(folder.id).unwrap_or_default();
    let path = form.path.trim_matches('/').to_string();
    let Some(directory) = target_kind(&files, &path) else {
        return not_found();
    };
    let page = |error: Option<String>, created: Option<String>| pages::ShareNew {
        username: session.username.clone(),
        admin: session.admin,
        csrf: session.csrf.clone(),
        id: folder.id.to_string(),
        name: folder_name(&folder),
        path: path.clone(),
        directory,
        error,
        created,
    };
    let (Ok(expires_days), Ok(max_downloads)) = (
        optional_count(&form.expires_days),
        optional_count(&form.max_downloads),
    ) else {
        return render(page(
            Some("Expiry and download limit must be whole numbers above zero, or empty.".into()),
            None,
        ));
    };
    let now = now_ms();
    let new = NewShare {
        folder: folder.id.to_string(),
        path: path.clone(),
        directory,
        created_by: session.username.clone(),
        created_at_ms: now,
        expires_at_ms: expires_days.map(|d| now + i64::from(d) * 86_400_000),
        password: (!form.password.is_empty()).then(|| form.password.clone()),
        max_downloads,
    };
    let created = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || state.shares.create(new)).await
    };
    let (share, token) = match created {
        Ok(Ok(created)) => created,
        Ok(Err(e)) => return render(page(Some(e.to_string()), None)),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    tracing::info!(link = %share.id, user = %session.username, folder = %folder.id, path = %share.path, "share link created");
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let scheme = if state.config.secure_cookies {
        "https"
    } else {
        "http"
    };
    render(page(None, Some(format!("{scheme}://{host}/s/{token}"))))
}

fn share_rows(state: &AppState, shares: Vec<Share>) -> Vec<pages::ShareRow> {
    let folders = state.engine.folders();
    shares
        .into_iter()
        .map(|s| pages::ShareRow {
            folder: folders
                .iter()
                .find(|f| f.id.to_string() == s.folder)
                .map_or_else(|| s.folder.clone(), folder_name),
            path: if s.path.is_empty() {
                "(whole folder)".into()
            } else if s.directory {
                format!("{}/", s.path)
            } else {
                s.path.clone()
            },
            created_by: s.created_by.clone(),
            created: time(s.created_at_ms),
            expires: s.expires_at_ms.map_or("never".into(), time),
            downloads: match s.max_downloads {
                Some(max) => format!("{} of {max}", s.downloads),
                None => s.downloads.to_string(),
            },
            password: s.password_hash.is_some(),
            id: s.id,
        })
        .collect()
}

fn shares_page(
    state: &AppState,
    session: crate::sessions::Session,
    notice: Option<String>,
) -> Response {
    let shares = match state.shares.list() {
        Ok(shares) => shares,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Server error",
                &e.to_string(),
            )
        }
    };
    // Administrators see every link; everyone else sees their own.
    let shares = shares
        .into_iter()
        .filter(|s| session.admin || s.created_by == session.username)
        .collect();
    render(pages::Shares {
        shares: share_rows(state, shares),
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        notice,
    })
}

pub(crate) async fn shares(State(state): AppStateRef, headers: HeaderMap) -> Response {
    if !state.config.share_links {
        return links_disabled();
    }
    let (_, session) = signed_in!(state, headers);
    shares_page(&state, session, None)
}

pub(crate) async fn revoke_share(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(link): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !state.config.share_links {
        return links_disabled();
    }
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    let owned = state
        .shares
        .list()
        .unwrap_or_default()
        .into_iter()
        .any(|s| s.id == link && (session.admin || s.created_by == session.username));
    if !owned {
        return not_found();
    }
    let notice = match state.shares.revoke(&link) {
        Ok(Some(share)) => {
            tracing::info!(link = %share.id, user = %session.username, "share link revoked");
            "Revoked the link. It no longer works.".to_string()
        }
        Ok(None) => "That link was already revoked.".to_string(),
        Err(e) => e.to_string(),
    };
    shares_page(&state, session, Some(notice))
}

// ---- Link pages ----

fn link_gone(status: StatusCode, message: &str) -> Response {
    error(status, "Link unavailable", message)
}

/// The link for `token` and its folder, if the link works: sharing is on, the link exists and
/// has not expired, and its creator still has access to the folder.
fn working_link(state: &AppState, token: &str) -> Result<(Share, FolderInfo), Box<Response>> {
    let missing = || {
        link_gone(
            StatusCode::NOT_FOUND,
            "This link does not exist or has been revoked.",
        )
    };
    if !state.config.share_links {
        return Err(Box::new(missing()));
    }
    let share = match state.shares.find(token) {
        Ok(Some(share)) => share,
        Ok(None) => return Err(Box::new(missing())),
        Err(e) => {
            tracing::error!(error = %e, "reading share links failed");
            return Err(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
        }
    };
    if share.expired(now_ms()) {
        tracing::info!(link = %share.id, "expired share link used");
        return Err(Box::new(link_gone(
            StatusCode::GONE,
            "This link has expired.",
        )));
    }
    let creator = state
        .accounts
        .get(&share.created_by)
        .ok()
        .flatten()
        .and_then(|a| a.user_id());
    let folder_id: Option<FolderId> = share.folder.parse().ok();
    let folder =
        state.engine.folders().into_iter().find(|f| {
            Some(f.id) == folder_id && creator.is_some_and(|user| f.members.contains(&user))
        });
    match folder {
        Some(folder) => Ok((share, folder)),
        None => Err(Box::new(missing())),
    }
}

fn link_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == LINK_COOKIE).then(|| value.to_string())
        })
}

fn unlocked(state: &AppState, share: &Share, headers: &HeaderMap) -> bool {
    share.password_hash.is_none()
        || link_cookie(headers).is_some_and(|c| state.shares.is_unlocked(&share.id, &c))
}

/// Live files the link covers, with their path relative to the shared directory.
fn link_files(state: &AppState, share: &Share, folder: &FolderInfo) -> Vec<(String, FileInfo)> {
    let files = state.engine.files(folder.id).unwrap_or_default();
    if !share.directory {
        return files
            .into_iter()
            .filter(|f| f.path == share.path)
            .map(|f| (f.path.rsplit('/').next().unwrap_or("").to_string(), f))
            .collect();
    }
    let prefix = if share.path.is_empty() {
        String::new()
    } else {
        format!("{}/", share.path)
    };
    files
        .into_iter()
        .filter_map(|f| {
            let relative = f.path.strip_prefix(&prefix)?.to_string();
            Some((relative, f))
        })
        .collect()
}

pub(crate) async fn link(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    let (share, folder) = match working_link(&state, &token) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    if !unlocked(&state, &share, &headers) {
        return render(pages::LinkPassword { token, error: None });
    }
    tracing::info!(link = %share.id, "share link opened");
    let title = if share.path.is_empty() {
        folder_name(&folder)
    } else {
        share
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&share.path)
            .to_string()
    };
    let files = link_files(&state, &share, &folder)
        .into_iter()
        .map(|(relative, f)| pages::LinkFile {
            query: query_value(&relative),
            name: relative,
            size: size(f.size),
        })
        .collect();
    render(pages::Link {
        token,
        title,
        directory: share.directory,
        exhausted: share.exhausted(),
        expires: share.expires_at_ms.map(time),
        files,
    })
}

#[derive(Deserialize)]
pub(crate) struct PasswordForm {
    password: String,
}

pub(crate) async fn unlock_link(
    State(state): AppStateRef,
    Path(token): Path<String>,
    Form(form): Form<PasswordForm>,
) -> Response {
    let (share, _) = match working_link(&state, &token) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let checked = {
        let share = share.clone();
        tokio::task::spawn_blocking(move || share.password_ok(&form.password)).await
    };
    if !matches!(checked, Ok(true)) {
        tracing::info!(link = %share.id, "wrong share link password");
        // Slow down guessing.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut response = render(pages::LinkPassword {
            token,
            error: Some("Wrong password.".into()),
        });
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        return response;
    }
    let cookie = state.shares.unlock(&share.id);
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{LINK_COOKIE}={cookie}; Path=/s/{token}; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        UNLOCK_TTL.as_secs()
    );
    let mut response = Redirect::to(&format!("/s/{token}")).into_response();
    match HeaderValue::from_str(&cookie) {
        Ok(value) => {
            response.headers_mut().insert(header::SET_COOKIE, value);
            response
        }
        Err(_) => not_found(),
    }
}

#[derive(Deserialize)]
pub(crate) struct LinkDownload {
    #[serde(default)]
    path: String,
}

pub(crate) async fn link_download(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(token): Path<String>,
    Query(query): Query<LinkDownload>,
) -> Response {
    let (share, folder) = match working_link(&state, &token) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    if !unlocked(&state, &share, &headers) {
        return Redirect::to(&format!("/s/{token}")).into_response();
    }
    // Only files the link covers, matched exactly against the folder's live files.
    let Some((_, file)) = link_files(&state, &share, &folder)
        .into_iter()
        .find(|(relative, _)| !share.directory || *relative == query.path)
    else {
        return not_found();
    };
    let opened = match open_file(&state, folder.id, &file.path).await {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    match state.shares.record_download(&share.id) {
        Ok(true) => {}
        Ok(false) => {
            return link_gone(
                StatusCode::GONE,
                "This link has reached its download limit.",
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "counting a share link download failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    tracing::info!(link = %share.id, path = %file.path, "share link download");
    send_file(opened, &file.path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_paths() {
        assert_eq!(upload_path("", "a.txt").as_deref(), Some("a.txt"));
        assert_eq!(
            upload_path("/photos/2026/", "C:\\Users\\me\\beach.jpg").as_deref(),
            Some("photos/2026/beach.jpg")
        );
        assert_eq!(upload_path("a//b", "c").as_deref(), Some("a/b/c"));
        assert_eq!(upload_path("../etc", "passwd"), None);
        assert_eq!(upload_path("", ".."), None);
        assert_eq!(upload_path("", ""), None);
        assert_eq!(upload_path(".tidefell/tmp", "x"), None);
        assert_eq!(upload_path("", ".tidefell"), None);
        assert_eq!(
            upload_path("docs/.tidefell", "x").as_deref(),
            Some("docs/.tidefell/x")
        );
    }
}
