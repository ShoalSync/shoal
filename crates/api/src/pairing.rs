//! `/v1/pairings`: approving new devices (ADR 0015).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use shoal_engine::pairing::{parse_code, PendingPairing};
use shoal_engine::Engine;
use shoal_identity::UserKey;
use shoal_sdk::types::{ApiError, ApprovePairing, ErrorKind, PairRequest, Pairing, RevokeRequest};

use crate::handlers::ApiFailure;

/// How long a pairing waits for the person to confirm.
const PENDING_TTL: Duration = Duration::from_secs(300);

/// Pairings waiting for confirmation, and the user key that approves them.
pub(crate) struct Pairings {
    user_key: Option<UserKey>,
    pending: Mutex<HashMap<String, (Instant, PendingPairing)>>,
}

impl Pairings {
    pub(crate) fn new(user_key: Option<UserKey>) -> Self {
        Self {
            user_key,
            pending: Mutex::default(),
        }
    }

    fn take(&self, id: &str) -> Result<PendingPairing, ApiFailure> {
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, (started, _)| started.elapsed() < PENDING_TTL);
        pending.remove(id).map(|(_, p)| p).ok_or_else(|| {
            failure(
                StatusCode::NOT_FOUND,
                ErrorKind::NotFound,
                "no such pairing",
            )
        })
    }
}

fn failure(status: StatusCode, kind: ErrorKind, message: impl Into<String>) -> ApiFailure {
    ApiFailure(
        status,
        ApiError {
            kind,
            message: message.into(),
        },
    )
}

/// Starts approving the new device in a pairing code. Returns once the device has accepted the
/// proof; compare the confirmation code with the one it shows, then approve or decline.
#[utoipa::path(post, path = "/v1/pairings", tag = "pairing",
    request_body = PairRequest,
    responses((status = 200, body = Pairing), (status = 400, body = ApiError),
        (status = 409, body = ApiError), (status = 503, body = ApiError)))]
pub(crate) async fn start(
    State(engine): State<Arc<Engine>>,
    State(pairings): State<Arc<Pairings>>,
    Json(request): Json<PairRequest>,
) -> Result<Json<Pairing>, ApiFailure> {
    if pairings.user_key.is_none() {
        return Err(failure(
            StatusCode::CONFLICT,
            ErrorKind::Conflict,
            "this device does not hold the user key, so it cannot approve new devices",
        ));
    }
    let code = parse_code(&request.code)?;
    let pending = engine.pair(&code).await?;
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(|e| ApiFailure::bad_request(e.to_string()))?;
    let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let info = Pairing {
        id: id.clone(),
        device: pending.device().to_string(),
        suggested_name: pending.suggested_name().to_owned(),
        confirmation_code: pending.confirmation_code().to_owned(),
    };
    pairings
        .pending
        .lock()
        .unwrap()
        .insert(id, (Instant::now(), pending));
    Ok(Json(info))
}

/// Certifies the new device and offers it this device's folders.
#[utoipa::path(post, path = "/v1/pairings/{id}/approve", tag = "pairing",
    params(("id" = String, Path, description = "Pairing ID")),
    request_body = ApprovePairing,
    responses((status = 204), (status = 404, body = ApiError), (status = 503, body = ApiError)))]
pub(crate) async fn approve(
    State(pairings): State<Arc<Pairings>>,
    Path(id): Path<String>,
    Json(request): Json<ApprovePairing>,
) -> Result<StatusCode, ApiFailure> {
    let pending = pairings.take(&id)?;
    let name = request
        .name
        .unwrap_or_else(|| pending.suggested_name().to_owned());
    let user = pairings
        .user_key
        .as_ref()
        .expect("checked when the pairing started");
    pending.approve(user, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Declines a pairing.
#[utoipa::path(delete, path = "/v1/pairings/{id}", tag = "pairing",
    params(("id" = String, Path, description = "Pairing ID")),
    responses((status = 204), (status = 404, body = ApiError)))]
pub(crate) async fn decline(
    State(pairings): State<Arc<Pairings>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    pairings.take(&id)?.decline().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Revokes one of the user's devices for good (ADR 0040): every peer that hears of it refuses
/// it from then on. Needs the user key, like approving a pairing.
#[utoipa::path(post, path = "/v1/devices/{device}/revoke", tag = "pairing",
    params(("device" = String, Path, description = "Device ID")),
    request_body = RevokeRequest,
    responses((status = 204), (status = 400, body = ApiError), (status = 409, body = ApiError)))]
pub(crate) async fn revoke(
    State(engine): State<Arc<Engine>>,
    State(pairings): State<Arc<Pairings>>,
    Path(device): Path<String>,
    Json(request): Json<RevokeRequest>,
) -> Result<StatusCode, ApiFailure> {
    let Some(user) = pairings.user_key.as_ref() else {
        return Err(failure(
            StatusCode::CONFLICT,
            ErrorKind::Conflict,
            "this device does not hold the user key, so it cannot revoke devices",
        ));
    };
    let device: shoal_engine::DeviceId = device
        .parse()
        .map_err(|_| ApiFailure::bad_request(format!("invalid device ID {device:?}")))?;
    engine.revoke_device(user, device, request.lost_ms)?;
    Ok(StatusCode::NO_CONTENT)
}
