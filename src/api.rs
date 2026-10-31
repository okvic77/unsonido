//! HTTP control API.
//!
//!   POST   /alerts          JSON alert (text via TTS, url, or chime only)
//!   POST   /alerts/audio    raw audio body; options in the query string
//!   DELETE /alerts/current  stop the alert that is playing
//!   DELETE /alerts          stop it and clear the queue
//!   GET    /status
//!   PUT    /volume          {"volume": 0.0..1.0}
//!   PUT    /background      {"url": "..."} or {"file": "..."}, optional "gain_db"
//!   DELETE /background
//!   PUT    /airplay/{name}          {"volume": 0.0..1.0, "muted": bool} local trim per speaker
//!   DELETE /airplay/{name}/session  disconnect the sender
//!   GET    /tts/providers
//!   GET    /                web control page

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::airplay::{AirPlay, ReceiverUpdate};
use crate::alerts::{AlertError, AlertRequest, AlertService, Content};
use crate::engine::AlertPriority;
use crate::inputs::{Background, BackgroundRequest};

pub struct Services {
    pub alerts: Arc<AlertService>,
    pub background: Arc<Background>,
    pub airplay: Arc<AirPlay>,
}

#[derive(Clone)]
struct AppState {
    alerts: Arc<AlertService>,
    background: Arc<Background>,
    airplay: Arc<AirPlay>,
    token: Option<Arc<str>>,
}

pub fn router(services: Services, token: Option<String>, max_body: usize) -> Router {
    let state = AppState {
        alerts: services.alerts,
        background: services.background,
        airplay: services.airplay,
        token: token.map(Into::into),
    };
    Router::new()
        .route("/alerts", post(create_alert).delete(clear_alerts))
        .route("/alerts/audio", post(upload_alert))
        .route("/alerts/current", delete(skip_alert))
        .route("/status", get(status))
        .route("/volume", put(set_volume))
        .route("/background", put(play_background).delete(stop_background))
        .route("/airplay/{name}", put(update_receiver))
        .route("/airplay/{name}/session", delete(disconnect_receiver))
        .route("/tts/providers", get(tts_providers))
        .route("/", get(ui))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state)
}

async fn require_token(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // The page itself is public; it sends the token with its API calls.
    if let Some(expected) = state.token.as_ref().filter(|_| request.uri().path() != "/") {
        let provided = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if provided != Some(expected.as_ref()) {
            return error(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
        }
    }
    next.run(request).await
}

async fn create_alert(
    State(state): State<AppState>,
    Json(request): Json<AlertRequest>,
) -> Response {
    respond(state.alerts.submit(request).await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadParams {
    id: Option<String>,
    #[serde(default = "notification")]
    priority: AlertPriority,
    repeat: Option<u32>,
    #[serde(default)]
    chime: bool,
    /// Decoding hint such as "mp3" or "wav"; taken from Content-Type when omitted.
    format: Option<String>,
}

fn notification() -> AlertPriority {
    AlertPriority::Notification
}

async fn upload_alert(
    State(state): State<AppState>,
    Query(params): Query<UploadParams>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.is_empty() {
        return error(StatusCode::BAD_REQUEST, "request body is empty");
    }
    let extension = params.format.or_else(|| {
        let mime = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
        extension_for_mime(mime).map(String::from)
    });
    let content = Content::Audio {
        bytes: body.to_vec(),
        extension,
    };
    respond(
        state
            .alerts
            .submit_content(
                params.id,
                params.priority,
                content,
                params.repeat,
                params.chime,
            )
            .await,
    )
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    Some(match mime.split(';').next()?.trim() {
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/aiff" | "audio/x-aiff" => "aiff",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/mp4" | "audio/aac" | "audio/x-m4a" => "m4a",
        _ => return None,
    })
}

async fn skip_alert(State(state): State<AppState>) -> StatusCode {
    state.alerts.skip();
    StatusCode::NO_CONTENT
}

async fn clear_alerts(State(state): State<AppState>) -> StatusCode {
    state.alerts.clear();
    StatusCode::NO_CONTENT
}

async fn ui() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("ui.html"))
}

async fn status(State(state): State<AppState>) -> Response {
    let mut status = serde_json::to_value(state.alerts.status()).unwrap_or_default();
    status["background"] = json!(state.background.status());
    status["airplay"] = json!(state.airplay.status());
    Json(status).into_response()
}

async fn play_background(
    State(state): State<AppState>,
    Json(request): Json<BackgroundRequest>,
) -> Response {
    match state.background.play(request) {
        Ok(()) => Json(state.background.status()).into_response(),
        Err(err) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    }
}

async fn update_receiver(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(update): Json<ReceiverUpdate>,
) -> Response {
    match state.airplay.receiver(&name) {
        Some(receiver) => {
            receiver.update(&update).await;
            Json(receiver.status()).into_response()
        }
        None => error(StatusCode::NOT_FOUND, "no such AirPlay speaker"),
    }
}

async fn disconnect_receiver(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.airplay.receiver(&name) {
        Some(receiver) if receiver.disconnect() => StatusCode::NO_CONTENT.into_response(),
        Some(_) => error(StatusCode::CONFLICT, "nothing is connected"),
        None => error(StatusCode::NOT_FOUND, "no such AirPlay speaker"),
    }
}

async fn stop_background(State(state): State<AppState>) -> StatusCode {
    state.background.stop();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct VolumeRequest {
    volume: f32,
}

async fn set_volume(State(state): State<AppState>, Json(request): Json<VolumeRequest>) -> Response {
    if !(0.0..=1.0).contains(&request.volume) {
        return error(
            StatusCode::BAD_REQUEST,
            "volume must be between 0.0 and 1.0",
        );
    }
    state.alerts.set_volume(request.volume);
    Json(json!({ "volume": request.volume })).into_response()
}

async fn tts_providers(State(state): State<AppState>) -> Response {
    Json(state.alerts.tts().providers()).into_response()
}

fn respond(result: Result<crate::alerts::Outcome, AlertError>) -> Response {
    match result {
        Ok(outcome @ crate::alerts::Outcome::Queued { .. }) => {
            (StatusCode::ACCEPTED, Json(outcome)).into_response()
        }
        Ok(outcome) => (StatusCode::OK, Json(outcome)).into_response(),
        Err(AlertError::Invalid(message)) => error(StatusCode::BAD_REQUEST, &message),
        Err(AlertError::QueueFull) => error(StatusCode::TOO_MANY_REQUESTS, "alert queue is full"),
        Err(AlertError::Failed(err)) => {
            tracing::warn!("alert failed: {err:#}");
            error(StatusCode::BAD_GATEWAY, &format!("{err:#}"))
        }
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
