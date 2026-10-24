//! Conflicts waiting for a decision (ADR 0029).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use shoal_engine::{Resolution, VersionKind};

use crate::format::{query_value, size, time};
use crate::handlers::{
    check_csrf, error, folder_name, not_found, render, send_file, signed_in, visible_folder,
};
use crate::pages;
use crate::AppState;

type AppStateRef = State<Arc<AppState>>;

#[derive(Deserialize)]
pub(crate) struct ConflictQuery {
    path: String,
}

/// A file's conflicting versions, to compare and choose from.
pub(crate) async fn conflict(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<ConflictQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let Ok(versions) = state.engine.versions(folder.id, &query.path) else {
        return not_found();
    };
    let versions: Vec<pages::ConflictRow> = versions
        .into_iter()
        .filter(|v| v.kind != VersionKind::Previous)
        .map(|v| pages::ConflictRow {
            current: v.kind == VersionKind::Current,
            version_query: query_value(&v.id),
            id: v.id,
            device: v.author_name.unwrap_or_default(),
            modified: time(v.modified_ms),
            size: size(v.size),
        })
        .collect();
    if versions.len() < 2 {
        // Decided already, here or on another device.
        return Redirect::to(&format!("/folders/{}", folder.id)).into_response();
    }
    render(pages::Conflict {
        username: session.username,
        admin: session.admin,
        csrf: session.csrf,
        id: folder.id.to_string(),
        name: folder_name(&folder),
        path_query: query_value(&query.path),
        path: query.path,
        versions,
    })
}

#[derive(Deserialize)]
pub(crate) struct OpenQuery {
    path: String,
    version: String,
}

/// Downloads a conflicting version, fetched from another device if this server does not have
/// it.
pub(crate) async fn open(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<OpenQuery>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    if !query.version.starts_with("conflict:") {
        return not_found();
    }
    let copy = match state
        .engine
        .open_version(folder.id, &query.path, &query.version)
        .await
    {
        Ok(copy) => copy,
        Err(shoal_engine::Error::Unavailable(_)) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Not available",
                "No connected device has this version right now.",
            )
        }
        Err(_) => return not_found(),
    };
    match tokio::fs::File::open(&copy).await {
        Ok(file) => send_file(file, &query.path).await,
        Err(_) => not_found(),
    }
}

#[derive(Deserialize)]
pub(crate) struct ResolveForm {
    csrf: String,
    path: String,
    /// The version to keep, or `both`.
    keep: String,
}

/// Decides a conflict from the web.
pub(crate) async fn resolve(
    State(state): AppStateRef,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ResolveForm>,
) -> Response {
    let (_, session) = signed_in!(state, headers);
    if let Some(response) = check_csrf(&session, &form.csrf) {
        return response;
    }
    let Some(folder) = visible_folder(&state, &session, &id) else {
        return not_found();
    };
    let resolution = if form.keep == "both" {
        Resolution::KeepBoth
    } else {
        Resolution::Keep(form.keep.clone())
    };
    if let Err(e) = state
        .engine
        .resolve(folder.id, &form.path, resolution)
        .await
    {
        tracing::warn!(error = %e, "resolving a conflict failed");
        return error(
            StatusCode::BAD_REQUEST,
            "Could not decide",
            &format!("{e}. The conflict may have been decided on another device."),
        );
    }
    tracing::info!(user = %session.username, folder = %folder.id, path = %form.path, keep = %form.keep, "resolved a conflict from the web");
    Redirect::to(&format!(
        "/folders/{}?resolved={}",
        folder.id,
        query_value(&form.path)
    ))
    .into_response()
}
