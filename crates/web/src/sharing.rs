//! Uploads and share links (ADR 0019).

use std::path::PathBuf;
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
use tidefell_engine::Engine;

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
    // Browsers may send a name in any Unicode form; paths are in NFC (ADR 0033).
    let path: String =
        unicode_normalization::UnicodeNormalization::nfc(parts.join("/").as_str()).collect();
    tidefell_proto::sync::is_valid_path(&path).then_some(path)
}

/// `path`, or if that is taken, the first free `name (n).ext` beside it, so an upload through a
/// link never replaces anything. A path is taken if the folder has a file there or below it, or
/// something is on disk there. `None` if no free name is found.
fn free_path(files: &[String], root: &std::path::Path, path: &str) -> Option<String> {
    let taken = |candidate: &str| {
        let below = format!("{candidate}/");
        files
            .iter()
            .any(|f| f == candidate || f.starts_with(&below))
            || root.join(candidate).symlink_metadata().is_ok()
    };
    if !taken(path) {
        return Some(path.to_string());
    }
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name),
        None => (String::new(), path),
    };
    // A leading dot starts a hidden name, not an extension.
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (1..1000)
        .map(|n| format!("{dir}{stem} ({n}){ext}"))
        .find(|candidate| tidefell_proto::sync::is_valid_path(candidate) && !taken(candidate))
}

fn random_suffix() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the OS provides randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why a file could not be staged.
enum StageError {
    /// It went over the byte limit.
    TooLarge,
    /// The drive would go under its free-space floor (ADR 0037).
    NoSpace(String),
    Failed(String),
}

/// Streams one uploaded file to a staging file in the folder's reserved directory, stopping
/// once it goes over `limit` bytes. Returns the staging file and its size.
async fn stage_upload(
    engine: &Engine,
    folder: &FolderInfo,
    field: &mut axum::extract::multipart::Field<'_>,
    limit: Option<u64>,
) -> Result<(PathBuf, u64), StageError> {
    let failed = |e: &dyn std::fmt::Display| StageError::Failed(e.to_string());
    // Room is checked before the first byte and again every so many, so an upload never fills
    // the drive past its floor.
    const ROOM_EVERY: u64 = 16 << 20;
    let room = |bytes: u64| {
        engine
            .has_room(&folder.path, bytes)
            .map_err(StageError::NoSpace)
    };
    room(ROOM_EVERY)?;
    let staging_dir = folder.path.join(".tidefell").join("tmp");
    tokio::fs::create_dir_all(&staging_dir)
        .await
        .map_err(|e| failed(&e))?;
    let staging = staging_dir.join(format!("upload-{}", random_suffix()));
    let written = async {
        let mut out = tokio::fs::File::create(&staging)
            .await
            .map_err(|e| failed(&e))?;
        let mut size = 0u64;
        let mut checked_at = 0u64;
        while let Some(chunk) = field.chunk().await.map_err(|e| failed(&e))? {
            size += chunk.len() as u64;
            if limit.is_some_and(|limit| size > limit) {
                return Err(StageError::TooLarge);
            }
            if size - checked_at >= ROOM_EVERY {
                room(ROOM_EVERY)?;
                checked_at = size;
            }
            out.write_all(&chunk).await.map_err(|e| failed(&e))?;
        }
        out.sync_all().await.map_err(|e| failed(&e))?;
        Ok(size)
    }
    .await;
    match written {
        Ok(size) => Ok((staging, size)),
        Err(e) => {
            let _ = tokio::fs::remove_file(&staging).await;
            Err(e)
        }
    }
}

/// Hands a staged file to the engine, which moves it into place and records the change.
async fn write_staged(
    state: &AppState,
    folder: &FolderInfo,
    path: &str,
    staging: &std::path::Path,
) -> Result<(), String> {
    let stored = state
        .engine
        .write(folder.id, path, staging)
        .await
        .map_err(|e| e.to_string());
    if stored.is_err() {
        let _ = tokio::fs::remove_file(staging).await;
    }
    stored
}

/// Streams one uploaded file to a staging file in the folder's reserved directory, then hands
/// it to the engine, which moves it into place and records the change.
async fn store_upload(
    state: &AppState,
    folder: &FolderInfo,
    path: &str,
    field: &mut axum::extract::multipart::Field<'_>,
) -> Result<(), String> {
    let (staging, _) = stage_upload(&state.engine, folder, field, state.config.max_upload_bytes)
        .await
        .map_err(|e| match e {
            StageError::TooLarge => format!(
                "larger than the {} this web peer accepts",
                size(state.config.max_upload_bytes.unwrap_or(0))
            ),
            StageError::NoSpace(why) => format!("not enough space: {why}"),
            StageError::Failed(e) => e,
        })?;
    write_staged(state, folder, path, &staging).await
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
        upload_only: false,
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
    /// A checkbox: present if visitors may only upload.
    #[serde(default)]
    upload_only: Option<String>,
    #[serde(default)]
    max_upload_mib: String,
    #[serde(default)]
    max_uploads: String,
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
        upload_only: false,
        error,
        created,
    };
    let (Ok(expires_days), Ok(max_downloads), Ok(max_upload_mib), Ok(max_uploads)) = (
        optional_count(&form.expires_days),
        optional_count(&form.max_downloads),
        optional_count(&form.max_upload_mib),
        optional_count(&form.max_uploads),
    ) else {
        return render(page(
            Some("Expiry and limits must be whole numbers above zero, or empty.".into()),
            None,
        ));
    };
    let upload_only = form.upload_only.is_some();
    if upload_only && !directory {
        return render(page(
            Some("Only a link to a directory can be upload-only.".into()),
            None,
        ));
    }
    let now = now_ms();
    let new = NewShare {
        folder: folder.id.to_string(),
        path: path.clone(),
        directory,
        created_by: session.username.clone(),
        created_at_ms: now,
        expires_at_ms: expires_days.map(|d| now + i64::from(d) * 86_400_000),
        password: (!form.password.is_empty()).then(|| form.password.clone()),
        // An upload-only link has no downloads to limit.
        max_downloads: max_downloads.filter(|_| !upload_only),
        upload_only,
        max_upload_bytes: max_upload_mib
            .filter(|_| upload_only)
            .map(|mib| u64::from(mib) << 20),
        max_uploads: max_uploads.filter(|_| upload_only),
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
    tracing::info!(link = %share.id, user = %session.username, folder = %folder.id, path = %share.path, upload_only, "share link created");
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let scheme = if state.config.secure_cookies {
        "https"
    } else {
        "http"
    };
    let mut created = page(None, Some(format!("{scheme}://{host}/s/{token}")));
    created.upload_only = upload_only;
    render(created)
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
            downloads: if s.upload_only {
                let files = match s.max_uploads {
                    Some(max) => format!("{} of {max} files", s.uploads),
                    None => format!("{} files", s.uploads),
                };
                match s.max_upload_bytes {
                    Some(max) => format!("{files}, {} of {}", size(s.uploaded_bytes), size(max)),
                    None => format!("{files}, {}", size(s.uploaded_bytes)),
                }
            } else {
                match s.max_downloads {
                    Some(max) => format!("{} of {max}", s.downloads),
                    None => s.downloads.to_string(),
                }
            },
            upload_only: s.upload_only,
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

fn link_title(share: &Share, folder: &FolderInfo) -> String {
    if share.path.is_empty() {
        folder_name(folder)
    } else {
        share
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&share.path)
            .to_string()
    }
}

#[derive(Deserialize)]
pub(crate) struct LinkQuery {
    /// Files uploaded by the form that led here.
    uploaded: Option<u32>,
}

pub(crate) async fn link(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(token): Path<String>,
    Query(query): Query<LinkQuery>,
) -> Response {
    let (share, folder) = match working_link(&state, &token) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    if !unlocked(&state, &share, &headers) {
        return render(pages::LinkPassword { token, error: None });
    }
    tracing::info!(link = %share.id, "share link opened");
    if share.upload_only {
        return render(pages::LinkUpload {
            token,
            title: link_title(&share, &folder),
            exhausted: share.exhausted(),
            expires: share.expires_at_ms.map(time),
            files_left: share
                .max_uploads
                .map(|max| max.saturating_sub(share.uploads)),
            bytes_left: share.remaining_bytes().map(size),
            uploaded: query.uploaded,
        });
    }
    let title = link_title(&share, &folder);
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
    let throttled = format!("link:{}", share.id);
    let wait = match state.throttle.begin(&throttled) {
        Ok(wait) => wait,
        Err(wait) => {
            return crate::handlers::too_many_attempts(
                wait,
                pages::LinkPassword {
                    token,
                    error: Some(crate::handlers::try_again_in(wait)),
                },
            );
        }
    };
    let checked = {
        let share = share.clone();
        tokio::task::spawn_blocking(move || share.password_ok(&form.password)).await
    };
    if !matches!(checked, Ok(true)) {
        tracing::info!(link = %share.id, "wrong share link password");
        // Slow down guessing: a fixed wait, and a growing one once it keeps failing.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut response = render(pages::LinkPassword {
            token,
            error: Some("Wrong password.".into()),
        });
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        if !wait.is_zero() {
            crate::handlers::retry_after(&mut response, wait);
        }
        return response;
    }
    state.throttle.cleared(&throttled);
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
    if share.upload_only {
        return not_found();
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

/// Uploads files through an upload-only link. Each file lands in the shared directory under a
/// free name, within the link's limits.
pub(crate) async fn link_upload(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(token): Path<String>,
    mut multipart: Multipart,
) -> Response {
    let (share, folder) = match working_link(&state, &token) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    if !share.upload_only {
        return not_found();
    }
    if !unlocked(&state, &share, &headers) {
        return Redirect::to(&format!("/s/{token}")).into_response();
    }
    let failed = |status, message: &str| error(status, "Upload failed", message);
    let full = || link_gone(StatusCode::GONE, "This link has reached its upload limit.");
    let mut uploaded = 0u32;
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => {
                return failed(
                    StatusCode::BAD_REQUEST,
                    "The upload was interrupted or malformed. Please try again.",
                )
            }
        };
        if field.name() != Some("file") {
            continue;
        }
        let Some(file_name) = field.file_name().map(str::to_owned) else {
            continue;
        };
        if file_name.is_empty() {
            continue;
        }
        let Some(path) = upload_path(&share.path, &file_name) else {
            return failed(
                StatusCode::BAD_REQUEST,
                &format!("{file_name:?} is not a valid name for a file here."),
            );
        };
        // The link's current counts, including this request's earlier files.
        let current = match state.shares.find(&token) {
            Ok(Some(current)) => current,
            _ => return not_found(),
        };
        if current.exhausted() {
            return full();
        }
        // The link's own limit, if it has one, within the web peer's.
        let limit = match (current.remaining_bytes(), state.config.max_upload_bytes) {
            (Some(link), Some(peer)) => Some(link.min(peer)),
            (link, peer) => link.or(peer),
        };
        let (staging, bytes) = match stage_upload(&state.engine, &folder, &mut field, limit).await {
            Ok(staged) => staged,
            Err(StageError::NoSpace(why)) => {
                tracing::warn!(link = %share.id, %why, "share link upload refused: no space");
                return failed(
                    StatusCode::INSUFFICIENT_STORAGE,
                    "There is no space for it.",
                );
            }
            Err(StageError::TooLarge) => {
                tracing::info!(link = %share.id, "share link upload over the size limit");
                return link_gone(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "That is more than this link accepts.",
                );
            }
            Err(StageError::Failed(e)) => {
                tracing::warn!(link = %share.id, error = %e, "share link upload failed");
                return failed(StatusCode::INTERNAL_SERVER_ERROR, "Saving the file failed.");
            }
        };
        // One upload through a link at a time from choosing a name to writing, so two uploads
        // of the same name do not pick the same free name.
        let _guard = state.link_uploads.lock().await;
        let files: Vec<String> = state
            .engine
            .files(folder.id)
            .unwrap_or_default()
            .into_iter()
            .map(|f| f.path)
            .collect();
        let stored = match free_path(&files, &folder.path, &path) {
            None => Err("no free name".to_string()),
            Some(path) => match state.shares.record_upload(&share.id, bytes) {
                Ok(true) => write_staged(&state, &folder, &path, &staging)
                    .await
                    .map(|()| path),
                Ok(false) => {
                    let _ = tokio::fs::remove_file(&staging).await;
                    return full();
                }
                Err(e) => Err(e.to_string()),
            },
        };
        match stored {
            Ok(path) => {
                tracing::info!(link = %share.id, folder = %folder.id, %path, bytes, "share link upload");
                uploaded += 1;
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&staging).await;
                tracing::warn!(link = %share.id, error = %e, "share link upload failed");
                return failed(StatusCode::INTERNAL_SERVER_ERROR, "Saving the file failed.");
            }
        }
    }
    Redirect::to(&format!("/s/{token}?uploaded={uploaded}")).into_response()
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

    #[test]
    fn free_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("on-disk.txt"), b"").unwrap();
        let files = [
            "in/a.txt",
            "in/a (1).txt",
            "in/.env",
            "in/dir/x",
            "in/notes",
        ]
        .map(String::from);
        let free = |path| free_path(&files, dir.path(), path);
        assert_eq!(free("in/b.txt").as_deref(), Some("in/b.txt"));
        assert_eq!(free("in/a.txt").as_deref(), Some("in/a (2).txt"));
        assert_eq!(free("in/.env").as_deref(), Some("in/.env (1)"));
        assert_eq!(free("in/dir").as_deref(), Some("in/dir (1)"));
        assert_eq!(free("in/notes").as_deref(), Some("in/notes (1)"));
        assert_eq!(free("on-disk.txt").as_deref(), Some("on-disk (1).txt"));
    }
}
