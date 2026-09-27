//! Request handlers. Each is annotated for the generated OpenAPI document.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use tidefell_engine::{
    self as engine, Engine, Error as EngineError, PeerAddr, ReplicaMode, Retention,
};
use tidefell_proto::{DeviceId, EntryId, FolderId, UserId};
use tidefell_sdk::types::{
    ApiError, ApprovePairing, CopiesRequest, Copy, ErrorKind, Event, FetchRequest, Fetched, File,
    Folder, KnownPeer, Mode, NewFolder, NewMember, PairRequest, Pairing, Peer, RecycleBinSummary,
    Recycled, RemoveHereRequest, RetentionUpdate, Status, SyncOptions,
};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use utoipa::OpenApi;

use crate::convert::{self, parse};

type AppState = State<Arc<Engine>>;

/// The generated OpenAPI document.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Tidefell API",
        description = "Control a Tidefell peer: folders, files, the recycle bin and peers.",
        license(name = "MIT OR Apache-2.0 (types and client); AGPL-3.0-only (server)")
    ),
    paths(
        status,
        folders,
        folder,
        add_folder,
        remove_folder,
        set_retention,
        set_sync,
        add_member,
        remove_member,
        scan,
        files,
        fetch,
        remove_here,
        copies,
        recycle_bin,
        empty_recycle_bin,
        discard,
        restore,
        peers,
        known_peers,
        add_peer,
        remove_peer,
        crate::pairing::start,
        crate::pairing::approve,
        crate::pairing::decline,
        events
    ),
    components(schemas(
        Status,
        Folder,
        Mode,
        NewFolder,
        RecycleBinSummary,
        RetentionUpdate,
        NewMember,
        File,
        FetchRequest,
        Fetched,
        CopiesRequest,
        Copy,
        Recycled,
        Peer,
        KnownPeer,
        PairRequest,
        Pairing,
        ApprovePairing,
        Event,
        ApiError,
        ErrorKind
    ))
)]
pub struct ApiDoc;

/// An error response: status code plus `ApiError` body.
pub(crate) struct ApiFailure(pub(crate) StatusCode, pub(crate) ApiError);

impl ApiFailure {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self(
            StatusCode::BAD_REQUEST,
            ApiError {
                kind: ErrorKind::BadRequest,
                message: message.into(),
            },
        )
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self(
            StatusCode::NOT_FOUND,
            ApiError {
                kind: ErrorKind::NotFound,
                message: message.into(),
            },
        )
    }
}

impl From<EngineError> for ApiFailure {
    fn from(e: EngineError) -> Self {
        let (status, kind) = match &e {
            EngineError::UnknownFolder(_) => (StatusCode::NOT_FOUND, ErrorKind::NotFound),
            EngineError::FolderExists(_)
            | EngineError::Configured(_)
            | EngineError::LocalChanges(_)
            | EngineError::PathTaken(_) => (StatusCode::CONFLICT, ErrorKind::Conflict),
            EngineError::Invalid(_) => (StatusCode::BAD_REQUEST, ErrorKind::BadRequest),
            EngineError::NoSuchFile(_) | EngineError::NotRecycled(_) => {
                (StatusCode::NOT_FOUND, ErrorKind::NotFound)
            }
            EngineError::Unavailable(_) | EngineError::Net(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, ErrorKind::Unavailable)
            }
            _ => (StatusCode::INTERNAL_SERVER_ERROR, ErrorKind::Internal),
        };
        Self(
            status,
            ApiError {
                kind,
                message: e.to_string(),
            },
        )
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiFailure>;

fn folder_info(engine: &Engine, id: FolderId) -> ApiResult<Folder> {
    engine
        .folders()
        .into_iter()
        .find(|f| f.id == id)
        .map(convert::folder)
        .ok_or_else(|| ApiFailure::from(EngineError::UnknownFolder(id)))
}

/// This device.
#[utoipa::path(get, path = "/v1/status", tag = "device",
    responses((status = 200, body = Status)))]
pub(crate) async fn status(State(engine): AppState) -> ApiResult<Json<Status>> {
    let certificate = engine.certificate().verify_certificate().map_err(|e| {
        ApiFailure(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError {
                kind: ErrorKind::Internal,
                message: e.to_string(),
            },
        )
    })?;
    Ok(Json(Status {
        device: engine.id().to_string(),
        user: certificate.user.to_string(),
        name: certificate.name,
        version: env!("CARGO_PKG_VERSION").to_string(),
        local_discovery: engine.local_discovery(),
    }))
}

/// All folders.
#[utoipa::path(get, path = "/v1/folders", tag = "folders",
    responses((status = 200, body = [Folder])))]
pub(crate) async fn folders(State(engine): AppState) -> Json<Vec<Folder>> {
    Json(engine.folders().into_iter().map(convert::folder).collect())
}

/// One folder.
#[utoipa::path(get, path = "/v1/folders/{id}", tag = "folders",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 200, body = Folder), (status = 404, body = ApiError)))]
pub(crate) async fn folder(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<Json<Folder>> {
    Ok(Json(folder_info(&engine, parse("folder", &id)?)?))
}

/// Creates a folder, or joins one shared from another device.
#[utoipa::path(post, path = "/v1/folders", tag = "folders",
    request_body = NewFolder,
    responses((status = 201, body = Folder), (status = 400, body = ApiError), (status = 409, body = ApiError)))]
pub(crate) async fn add_folder(
    State(engine): AppState,
    Json(new): Json<NewFolder>,
) -> ApiResult<(StatusCode, Json<Folder>)> {
    let id = new
        .id
        .as_deref()
        .map(|id| parse::<FolderId>("folder", id))
        .transpose()?;
    let members = new
        .members
        .iter()
        .map(|m| parse::<UserId>("member", m))
        .collect::<ApiResult<_>>()?;
    let mode = match (new.mode.unwrap_or(Mode::Full), new.budget_bytes) {
        (Mode::Full, _) => ReplicaMode::Full,
        (Mode::Partial, Some(budget_bytes)) => ReplicaMode::Partial { budget_bytes },
        (Mode::Partial, None) => {
            return Err(ApiFailure::bad_request("partial folders need budget_bytes"))
        }
    };
    if new.path.trim().is_empty() {
        return Err(ApiFailure::bad_request("path must not be empty"));
    }
    let folder = engine.create_folder(engine::NewFolder {
        id,
        path: PathBuf::from(&new.path),
        mode,
        members,
        retention: Retention::from_days(new.retention_days),
        scan_interval: Duration::from_secs(new.scan_interval_secs.unwrap_or(10).max(1)),
        sync: convert::engine_sync_options(new.sync),
    })?;
    Ok((StatusCode::CREATED, Json(convert::folder(folder))))
}

/// Stops syncing a folder added at runtime. Files on disk are left in place.
#[utoipa::path(delete, path = "/v1/folders/{id}", tag = "folders",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 204), (status = 404, body = ApiError), (status = 409, body = ApiError)))]
pub(crate) async fn remove_folder(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    engine.remove_folder(parse("folder", &id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Changes how long this device keeps the folder's deleted files. Items past a shortened
/// retention are purged at the next hourly purge, not immediately.
#[utoipa::path(put, path = "/v1/folders/{id}/retention", tag = "folders",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = RetentionUpdate,
    responses((status = 200, body = Folder), (status = 404, body = ApiError), (status = 409, body = ApiError)))]
pub(crate) async fn set_retention(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(update): Json<RetentionUpdate>,
) -> ApiResult<Json<Folder>> {
    let id = parse("folder", &id)?;
    let retention = update
        .retention_days
        .map_or(Retention::Forever, Retention::from_days);
    engine.set_retention(id, retention)?;
    Ok(Json(folder_info(&engine, id)?))
}

/// Changes how the folder syncs on this device: whether other devices' changes are written here,
/// and whether deleting a file here deletes it everywhere. Files already dropped from this
/// device stay dropped.
#[utoipa::path(put, path = "/v1/folders/{id}/sync", tag = "folders",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = SyncOptions,
    responses((status = 200, body = Folder), (status = 404, body = ApiError), (status = 409, body = ApiError)))]
pub(crate) async fn set_sync(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(sync): Json<SyncOptions>,
) -> ApiResult<Json<Folder>> {
    let id = parse("folder", &id)?;
    engine.set_sync_options(id, convert::engine_sync_options(sync))?;
    Ok(Json(folder_info(&engine, id)?))
}

/// Lets a user's devices sync the folder.
#[utoipa::path(post, path = "/v1/folders/{id}/members", tag = "folders",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = NewMember,
    responses((status = 200, body = Folder), (status = 404, body = ApiError), (status = 409, body = ApiError)))]
pub(crate) async fn add_member(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(member): Json<NewMember>,
) -> ApiResult<Json<Folder>> {
    let id = parse("folder", &id)?;
    engine.add_member(id, parse("member", &member.user)?)?;
    Ok(Json(folder_info(&engine, id)?))
}

/// Removes a user from the folder.
#[utoipa::path(delete, path = "/v1/folders/{id}/members/{user}", tag = "folders",
    params(("id" = String, Path, description = "Folder ID"), ("user" = String, Path, description = "User ID")),
    responses((status = 200, body = Folder), (status = 400, body = ApiError), (status = 404, body = ApiError)))]
pub(crate) async fn remove_member(
    State(engine): AppState,
    Path((id, user)): Path<(String, String)>,
) -> ApiResult<Json<Folder>> {
    let id = parse("folder", &id)?;
    engine.remove_member(id, parse("member", &user)?)?;
    Ok(Json(folder_info(&engine, id)?))
}

/// Scans the folder for local changes now.
#[utoipa::path(post, path = "/v1/folders/{id}/scan", tag = "folders",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 204), (status = 404, body = ApiError)))]
pub(crate) async fn scan(State(engine): AppState, Path(id): Path<String>) -> ApiResult<StatusCode> {
    engine.scan(parse("folder", &id)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Live files in the folder, sorted by path.
#[utoipa::path(get, path = "/v1/folders/{id}/files", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 200, body = [File]), (status = 404, body = ApiError)))]
pub(crate) async fn files(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<File>>> {
    Ok(Json(
        engine
            .files(parse("folder", &id)?)?
            .into_iter()
            .map(convert::file)
            .collect(),
    ))
}

/// Makes a live file available on this device's disk, fetching it from connected peers if
/// needed (a placeholder in a partial replica), and returns its local path.
#[utoipa::path(post, path = "/v1/folders/{id}/fetch", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = FetchRequest,
    responses((status = 200, body = Fetched), (status = 404, body = ApiError),
        (status = 503, body = ApiError)))]
pub(crate) async fn fetch(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<FetchRequest>,
) -> ApiResult<Json<Fetched>> {
    let folder: FolderId = parse("folder", &id)?;
    let local = engine.open(folder, &request.path).await?;
    engine.close(folder, &request.path)?;
    Ok(Json(Fetched {
        local_path: local.display().to_string(),
    }))
}

/// Removes a file from this device only. It moves to this device's recycle bin, stays on every
/// other device, and is not downloaded again until fetched or restored.
#[utoipa::path(post, path = "/v1/folders/{id}/remove-here", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = RemoveHereRequest,
    responses((status = 204), (status = 404, body = ApiError), (status = 409, body = ApiError)))]
pub(crate) async fn remove_here(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<RemoveHereRequest>,
) -> ApiResult<StatusCode> {
    engine
        .remove_here(parse("folder", &id)?, &request.path)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The full replicas that confirmed holding a file made on this device (ADR 0022). Empty
/// for files made on other devices, which are not tracked.
#[utoipa::path(post, path = "/v1/folders/{id}/copies", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = CopiesRequest,
    responses((status = 200, body = [Copy]), (status = 404, body = ApiError)))]
pub(crate) async fn copies(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<CopiesRequest>,
) -> ApiResult<Json<Vec<Copy>>> {
    Ok(Json(
        engine
            .copies(parse("folder", &id)?, &request.path)?
            .into_iter()
            .map(convert::copy)
            .collect(),
    ))
}

/// The folder's recycle bin, most recently deleted first.
#[utoipa::path(get, path = "/v1/folders/{id}/recycle", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 200, body = [Recycled]), (status = 404, body = ApiError)))]
pub(crate) async fn recycle_bin(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<Recycled>>> {
    Ok(Json(
        engine
            .recycle_bin(parse("folder", &id)?)?
            .into_iter()
            .map(convert::recycled)
            .collect(),
    ))
}

/// Permanently removes every item from this device's recycle bin for the folder, and
/// returns what was removed. Other devices keep their copies.
#[utoipa::path(delete, path = "/v1/folders/{id}/recycle", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 200, body = [Recycled]), (status = 404, body = ApiError)))]
pub(crate) async fn empty_recycle_bin(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<Recycled>>> {
    Ok(Json(
        engine
            .empty_recycle_bin(parse("folder", &id)?, None)?
            .into_iter()
            .map(convert::recycled)
            .collect(),
    ))
}

/// Permanently removes one entry from this device's recycle bin, and returns it. Other
/// devices keep their copies.
#[utoipa::path(delete, path = "/v1/folders/{id}/recycle/{entry}", tag = "files",
    params(("id" = String, Path, description = "Folder ID"), ("entry" = String, Path, description = "Entry ID")),
    responses((status = 200, body = Recycled), (status = 404, body = ApiError)))]
pub(crate) async fn discard(
    State(engine): AppState,
    Path((id, entry)): Path<(String, String)>,
) -> ApiResult<Json<Recycled>> {
    let folder: FolderId = parse("folder", &id)?;
    let entry = parse_entry(&entry)?;
    engine
        .empty_recycle_bin(folder, Some(entry))?
        .into_iter()
        .next()
        .map(|r| Json(convert::recycled(r)))
        .ok_or_else(|| ApiFailure::not_found("entry is not in the recycle bin"))
}

fn parse_entry(entry: &str) -> ApiResult<EntryId> {
    entry.parse().map_err(|_| {
        ApiFailure::bad_request(format!("entry {entry:?} is not 32 lowercase hex digits"))
    })
}

/// Restores an entry from the recycle bin. A file deleted everywhere reappears on every
/// replica; a file removed from this device only is put back here.
#[utoipa::path(post, path = "/v1/folders/{id}/recycle/{entry}/restore", tag = "files",
    params(("id" = String, Path, description = "Folder ID"), ("entry" = String, Path, description = "Entry ID")),
    responses((status = 204), (status = 404, body = ApiError)))]
pub(crate) async fn restore(
    State(engine): AppState,
    Path((id, entry)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let folder: FolderId = parse("folder", &id)?;
    let entry = parse_entry(&entry)?;
    if !engine.recycle_bin(folder)?.iter().any(|r| r.entry == entry) {
        return Err(ApiFailure::not_found("entry is not in the recycle bin"));
    }
    engine.restore(folder, entry).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Peers with an active sync session.
#[utoipa::path(get, path = "/v1/peers", tag = "peers",
    responses((status = 200, body = [Peer])))]
pub(crate) async fn peers(State(engine): AppState) -> Json<Vec<Peer>> {
    Json(
        engine
            .connected_peers()
            .into_iter()
            .map(convert::peer)
            .collect(),
    )
}

/// Peers this device keeps connected.
#[utoipa::path(get, path = "/v1/peers/known", tag = "peers",
    responses((status = 200, body = [KnownPeer])))]
pub(crate) async fn known_peers(State(engine): AppState) -> Json<Vec<KnownPeer>> {
    Json(
        engine
            .known_peers()
            .into_iter()
            .map(convert::known_peer)
            .collect(),
    )
}

/// Remembers a peer to keep connected.
#[utoipa::path(post, path = "/v1/peers/known", tag = "peers",
    request_body = KnownPeer,
    responses((status = 204), (status = 400, body = ApiError)))]
pub(crate) async fn add_peer(
    State(engine): AppState,
    Json(peer): Json<KnownPeer>,
) -> ApiResult<StatusCode> {
    let device: DeviceId = parse("device", &peer.device)?;
    let addrs = peer
        .addrs
        .iter()
        .map(|a| {
            a.parse()
                .map_err(|_| ApiFailure::bad_request(format!("invalid address {a:?}")))
        })
        .collect::<ApiResult<_>>()?;
    let relay = peer
        .relay
        .as_deref()
        .map(|r| {
            r.parse()
                .map_err(|_| ApiFailure::bad_request(format!("invalid relay URL {r:?}")))
        })
        .transpose()?;
    engine.add_peer(PeerAddr {
        device,
        addrs,
        relay,
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// Forgets a peer added at runtime.
#[utoipa::path(delete, path = "/v1/peers/known/{device}", tag = "peers",
    params(("device" = String, Path, description = "Device ID")),
    responses((status = 204), (status = 404, body = ApiError)))]
pub(crate) async fn remove_peer(
    State(engine): AppState,
    Path(device): Path<String>,
) -> ApiResult<StatusCode> {
    if engine.remove_peer(parse("device", &device)?)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiFailure::not_found("peer was not added at runtime"))
    }
}

/// Server-sent events: one JSON `Event` per `data:` line.
#[utoipa::path(get, path = "/v1/events", tag = "device",
    responses((status = 200, description = "text/event-stream of Event", body = Event, content_type = "text/event-stream")))]
pub(crate) async fn events(
    State(engine): AppState,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(engine.events()).filter_map(|event| {
        // A lagging subscriber skips missed events rather than failing.
        let event = event.ok()?;
        SseEvent::default()
            .json_data(convert::event(event))
            .ok()
            .map(Ok)
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub(crate) async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}
