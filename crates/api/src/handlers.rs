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
use tidefell_engine::diagnostics;
use tidefell_engine::{
    self as engine, Engine, Error as EngineError, PeerAddr, ReplicaMode, Retention,
};
use tidefell_proto::{DeviceId, EntryId, FolderId, UserId};
use tidefell_sdk::types::{
    ApiError, ApprovePairing, Check, CheckLevel, CopiesRequest, Copy, Crash, ErrorKind, Event,
    FetchRequest, Fetched, File, Folder, FolderPeer, FolderState, KnownPeer, Limits, LogLevel,
    LogSettings, Mode, NewFolder, NewMember, OpenedVersion, Operation, PairRequest, Pairing,
    PathKind, PathProblem, Peer, PeerState, PinRequest, Problem, ProblemKind, RecycleBinSummary,
    Recycled, RemoveHereRequest, ResolveRequest, RetentionUpdate, Status, SyncOptions, Transfer,
    Version, VersionKind, VersionRequest,
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
        pin,
        unpin,
        pins,
        copies,
        versions,
        folder_problems,
        doctor,
        restore_version,
        open_version,
        resolve,
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
        events,
        log_settings,
        set_log_level,
        problems,
        crashes,
        dismiss_crashes,
        limits,
        set_limits,
        pause,
        resume,
        pause_folder,
        resume_folder
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
        ErrorKind,
        LogSettings,
        LogLevel,
        Problem,
        Crash,
        Version,
        VersionKind,
        VersionRequest,
        OpenedVersion,
        ResolveRequest,
        PeerState,
        Check,
        CheckLevel,
        PathKind,
        Transfer,
        FolderState,
        FolderPeer,
        PathProblem,
        Operation,
        ProblemKind,
        Limits,
        PinRequest
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

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
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
            EngineError::Unavailable(_)
            | EngineError::FolderUnavailable(..)
            | EngineError::Net(_)
            | EngineError::Paused(_) => (StatusCode::SERVICE_UNAVAILABLE, ErrorKind::Unavailable),
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
        crash: crashes_on(&engine).into_iter().rfind(|c| !c.seen),
        paused: engine.paused(),
        peers: engine
            .peer_states()
            .into_iter()
            .map(convert::peer_state)
            .collect(),
        transfer: convert::transfer(engine.transfer()),
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

/// Pins a file or directory on a partial replica (ADR 0029): fetched in the background, kept up
/// to date and never evicted, and not counted against the budget.
#[utoipa::path(post, path = "/v1/folders/{id}/pin", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = PinRequest,
    responses((status = 204), (status = 400, body = ApiError), (status = 404, body = ApiError)))]
pub(crate) async fn pin(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<PinRequest>,
) -> ApiResult<StatusCode> {
    engine.pin(parse("folder", &id)?, &request.path).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a pin. What it covered may then be evicted to keep the folder within its budget.
#[utoipa::path(post, path = "/v1/folders/{id}/unpin", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = PinRequest,
    responses((status = 204), (status = 400, body = ApiError), (status = 404, body = ApiError)))]
pub(crate) async fn unpin(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<PinRequest>,
) -> ApiResult<StatusCode> {
    engine.unpin(parse("folder", &id)?, &request.path).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The paths pinned on this device.
#[utoipa::path(get, path = "/v1/folders/{id}/pins", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 200, body = [String]), (status = 404, body = ApiError)))]
pub(crate) async fn pins(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<String>>> {
    Ok(Json(engine.pins(parse("folder", &id)?)?))
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

/// The versions of a file: the current one first, then those kept on this device, newest first
/// (ADR 0029).
#[utoipa::path(post, path = "/v1/folders/{id}/versions", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = VersionRequest,
    responses((status = 200, body = [Version]), (status = 404, body = ApiError)))]
pub(crate) async fn versions(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<VersionRequest>,
) -> ApiResult<Json<Vec<Version>>> {
    Ok(Json(
        engine
            .versions(parse("folder", &id)?, &request.path)?
            .into_iter()
            .map(convert::version)
            .collect(),
    ))
}

/// Makes a previous version the file's content again, as a new version on every device.
#[utoipa::path(post, path = "/v1/folders/{id}/versions/restore", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = VersionRequest,
    responses((status = 204), (status = 400, body = ApiError), (status = 404, body = ApiError)))]
pub(crate) async fn restore_version(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<VersionRequest>,
) -> ApiResult<StatusCode> {
    let version = request
        .version
        .ok_or_else(|| ApiFailure::bad_request("which version to restore is missing"))?;
    engine
        .restore_version(parse("folder", &id)?, &request.path, &version)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Writes a read-only copy of a previous or conflicting version on the daemon's machine, fetching
/// a conflicting version from peers if it is not there (ADR 0029).
#[utoipa::path(post, path = "/v1/folders/{id}/versions/open", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = VersionRequest,
    responses((status = 200, body = OpenedVersion), (status = 400, body = ApiError),
        (status = 404, body = ApiError)))]
pub(crate) async fn open_version(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<VersionRequest>,
) -> ApiResult<Json<OpenedVersion>> {
    let version = request
        .version
        .ok_or_else(|| ApiFailure::bad_request("which version to open is missing"))?;
    let file = engine
        .open_version(parse("folder", &id)?, &request.path, &version)
        .await?;
    Ok(Json(OpenedVersion {
        file: file.display().to_string(),
    }))
}

/// Decides a conflict on a file (ADR 0029): keep one version, whose others go to every device's
/// recycle bin, or keep both, making each other version a file of its own.
#[utoipa::path(post, path = "/v1/folders/{id}/resolve", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    request_body = ResolveRequest,
    responses((status = 204), (status = 400, body = ApiError), (status = 404, body = ApiError)))]
pub(crate) async fn resolve(
    State(engine): AppState,
    Path(id): Path<String>,
    Json(request): Json<ResolveRequest>,
) -> ApiResult<StatusCode> {
    let resolution = if request.keep == "both" {
        engine::Resolution::KeepBoth
    } else {
        engine::Resolution::Keep(request.keep)
    };
    engine
        .resolve(parse("folder", &id)?, &request.path, resolution)
        .await?;
    Ok(StatusCode::NO_CONTENT)
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

fn log_settings_now() -> ApiResult<LogSettings> {
    let level = diagnostics::log_level()
        .ok_or_else(|| ApiFailure::not_found("logging is not started in this process"))?;
    Ok(LogSettings {
        level,
        file: diagnostics::log_file().map(|p| p.display().to_string()),
    })
}

/// How this device logs.
#[utoipa::path(get, path = "/v1/log", tag = "device",
    responses((status = 200, body = LogSettings), (status = 404, body = ApiError)))]
pub(crate) async fn log_settings() -> ApiResult<Json<LogSettings>> {
    Ok(Json(log_settings_now()?))
}

/// Changes the log level while running, until the next restart.
#[utoipa::path(put, path = "/v1/log", tag = "device", request_body = LogLevel,
    responses((status = 200, body = LogSettings), (status = 400, body = ApiError), (status = 404, body = ApiError)))]
pub(crate) async fn set_log_level(Json(update): Json<LogLevel>) -> ApiResult<Json<LogSettings>> {
    log_settings_now()?;
    diagnostics::set_log_level(&update.level)
        .map_err(|e| ApiFailure::bad_request(format!("invalid level {e}")))?;
    Ok(Json(log_settings_now()?))
}

/// The folder's paths that failed to scan, fetch, write or delete, until they succeed
/// (ADR 0030).
#[utoipa::path(get, path = "/v1/folders/{id}/problems", tag = "files",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 200, body = [PathProblem]), (status = 404, body = ApiError)))]
pub(crate) async fn folder_problems(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<PathProblem>>> {
    Ok(Json(
        engine
            .path_problems(parse("folder", &id)?)?
            .into_iter()
            .map(convert::path_problem)
            .collect(),
    ))
}

/// Checks this device's clock against its peers', its free space, its folders, file watching
/// and connectivity, each pass, warning or failure with advice (ADR 0030).
#[utoipa::path(get, path = "/v1/doctor", tag = "device",
    responses((status = 200, body = [Check])))]
pub(crate) async fn doctor(State(engine): AppState) -> Json<Vec<Check>> {
    // It reads the folders' disks, which can be slow or hang on a network mount.
    let checks = tokio::task::spawn_blocking(move || engine.doctor())
        .await
        .unwrap_or_default();
    Json(checks.into_iter().map(convert::check).collect())
}

/// The most recent warnings and errors, oldest first.
#[utoipa::path(get, path = "/v1/log/problems", tag = "device",
    responses((status = 200, body = [Problem])))]
pub(crate) async fn problems() -> Json<Vec<Problem>> {
    Json(
        diagnostics::recent_problems()
            .into_iter()
            .map(|p| Problem {
                time_ms: p.time_ms,
                level: p.level,
                target: p.target,
                message: p.message,
                fields: p.fields,
            })
            .collect(),
    )
}

fn crashes_on(engine: &Engine) -> Vec<Crash> {
    diagnostics::crashes(engine.data_dir())
        .into_iter()
        .map(|c| Crash {
            file: c.file.display().to_string(),
            time_ms: c.time_ms,
            message: c.message,
            seen: c.seen,
        })
        .collect()
}

/// Crashes recorded on this device, oldest first.
#[utoipa::path(get, path = "/v1/crashes", tag = "device",
    responses((status = 200, body = [Crash])))]
pub(crate) async fn crashes(State(engine): AppState) -> Json<Vec<Crash>> {
    Json(crashes_on(&engine))
}

/// Marks every crash recorded so far as seen, so status stops reporting it.
#[utoipa::path(post, path = "/v1/crashes/dismiss", tag = "device",
    responses((status = 204), (status = 500, body = ApiError)))]
pub(crate) async fn dismiss_crashes(State(engine): AppState) -> ApiResult<StatusCode> {
    diagnostics::dismiss_crashes(engine.data_dir()).map_err(|e| {
        ApiFailure(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError {
                kind: ErrorKind::Internal,
                message: e.to_string(),
            },
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}

fn limits_view(l: tidefell_engine::Limits) -> Limits {
    Limits {
        upload_kib_per_sec: l.upload.map(|b| b / 1024),
        download_kib_per_sec: l.download.map(|b| b / 1024),
        limit_local: l.limit_local,
    }
}

/// The upload and download limits in force.
#[utoipa::path(get, path = "/v1/limits", tag = "device",
    responses((status = 200, body = Limits)))]
pub(crate) async fn limits(State(engine): AppState) -> Json<Limits> {
    Json(limits_view(engine.limits()))
}

/// Changes the upload and download limits until the daemon restarts. Limits below 64 KiB/s are
/// raised to it.
#[utoipa::path(put, path = "/v1/limits", tag = "device", request_body = Limits,
    responses((status = 200, body = Limits)))]
pub(crate) async fn set_limits(
    State(engine): AppState,
    Json(limits): Json<Limits>,
) -> Json<Limits> {
    engine.set_limits(tidefell_engine::Limits {
        upload: limits.upload_kib_per_sec.map(|k| k * 1024),
        download: limits.download_kib_per_sec.map(|k| k * 1024),
        limit_local: limits.limit_local,
    });
    Json(limits_view(engine.limits()))
}

/// Pauses syncing of the whole device (ADR 0029). Folders still scan; nothing is sent or
/// received until it resumes, across restarts.
#[utoipa::path(post, path = "/v1/pause", tag = "device", responses((status = 204)))]
pub(crate) async fn pause(State(engine): AppState) -> ApiResult<StatusCode> {
    engine.pause(None)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resumes syncing of the whole device. Folders paused on their own stay paused.
#[utoipa::path(post, path = "/v1/resume", tag = "device", responses((status = 204)))]
pub(crate) async fn resume(State(engine): AppState) -> ApiResult<StatusCode> {
    engine.resume(None)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Pauses syncing of one folder. It still scans; nothing is sent or received for it.
#[utoipa::path(post, path = "/v1/folders/{id}/pause", tag = "folders",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 200, body = Folder), (status = 404, body = ApiError)))]
pub(crate) async fn pause_folder(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<Json<Folder>> {
    let id = parse("folder", &id)?;
    engine.pause(Some(id))?;
    Ok(Json(folder_info(&engine, id)?))
}

/// Resumes syncing of one folder.
#[utoipa::path(post, path = "/v1/folders/{id}/resume", tag = "folders",
    params(("id" = String, Path, description = "Folder ID")),
    responses((status = 200, body = Folder), (status = 404, body = ApiError)))]
pub(crate) async fn resume_folder(
    State(engine): AppState,
    Path(id): Path<String>,
) -> ApiResult<Json<Folder>> {
    let id = parse("folder", &id)?;
    engine.resume(Some(id))?;
    Ok(Json(folder_info(&engine, id)?))
}
