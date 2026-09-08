/*
 * MicYou — local Web-first dashboard control plane.
 * Bound to 127.0.0.1 only so control APIs are never exposed to the LAN.
 */

use axum::{extract::State, http::StatusCode, response::{Html, IntoResponse}, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, RwLock};
use tokio::sync::Mutex;

use crate::{audio_output::AudioOutputHandle, plugins::PluginHost, stats::NetworkStats};

pub const DASHBOARD_PORT: u16 = 19527;
const DASHBOARD_HTML: &str = include_str!("../resources/dashboard_local.html");

#[derive(Clone)]
pub struct DashboardState {
    pub dsp_settings: Arc<RwLock<micyou_audio::dsp::AudioDspSettings>>,
    pub is_monitoring: Arc<AtomicBool>,
    pub network_stats: Arc<NetworkStats>,
    pub audio_output: Arc<AudioOutputHandle>,
    pub plugins: Arc<PluginHost>,
    #[cfg(feature = "web-server")]
    pub web_server: Arc<Mutex<Option<crate::web_server::WebServer>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardStatus {
    online: bool,
    muted: bool,
    monitoring: bool,
    web_client_count: usize,
    queued_samples: usize,
    sample_rate: u32,
    bitrate: u32,
    network_latency_ms: i64,
    jitter_ms: f64,
    packet_loss_rate: f64,
    dsp: Value,
}

async fn index() -> Html<&'static str> { Html(DASHBOARD_HTML) }

async fn status(State(state): State<DashboardState>) -> Json<DashboardStatus> {
    #[cfg(feature = "web-server")]
    let web_client_count = state.web_server.lock().await.as_ref().map(|w| w.client_count()).unwrap_or(0);
    #[cfg(not(feature = "web-server"))]
    let web_client_count = 0;

    let dsp = state.dsp_settings.read().ok()
        .and_then(|v| serde_json::to_value(&*v).ok())
        .unwrap_or_else(|| json!({}));

    Json(DashboardStatus {
        online: true,
        muted: state.network_stats.is_muted(),
        monitoring: state.is_monitoring.load(Ordering::Relaxed),
        web_client_count,
        queued_samples: state.audio_output.queued_samples(),
        sample_rate: state.network_stats.sample_rate.load(Ordering::Relaxed),
        bitrate: state.network_stats.bitrate.load(Ordering::Relaxed),
        network_latency_ms: state.network_stats.get_rtt(),
        jitter_ms: state.network_stats.get_jitter(),
        packet_loss_rate: state.network_stats.get_loss_rate(),
        dsp,
    })
}

#[derive(Deserialize)]
struct BoolControl { enabled: bool }

async fn set_mute(State(state): State<DashboardState>, Json(body): Json<BoolControl>) -> Json<Value> {
    state.network_stats.set_muted(body.enabled);
    state.audio_output.set_muted(body.enabled);
    state.plugins.broadcast_event(&micyou_plugin::PluginEvent::MuteChanged { muted: body.enabled });
    Json(json!({"ok": true, "muted": body.enabled}))
}

async fn set_monitoring(State(state): State<DashboardState>, Json(body): Json<BoolControl>) -> Json<Value> {
    state.is_monitoring.store(body.enabled, Ordering::Relaxed);
    state.audio_output.set_monitoring(body.enabled);
    state.plugins.broadcast_event(&micyou_plugin::PluginEvent::MonitoringChanged { enabled: body.enabled });
    Json(json!({"ok": true, "monitoring": body.enabled}))
}

async fn patch_dsp(State(state): State<DashboardState>, Json(patch): Json<Value>) -> impl IntoResponse {
    let patch = match patch.as_object() {
        Some(v) => v,
        None => return (StatusCode::BAD_REQUEST, Json(json!({"error":"DSP patch must be a JSON object"}))).into_response(),
    };

    let current = match state.dsp_settings.read() {
        Ok(v) => v.clone(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"DSP settings lock failed"}))).into_response(),
    };
    let mut value = match serde_json::to_value(current) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":e.to_string()}))).into_response(),
    };
    if let Some(obj) = value.as_object_mut() {
        for (k, v) in patch { obj.insert(k.clone(), v.clone()); }
    }
    let mut updated: micyou_audio::dsp::AudioDspSettings = match serde_json::from_value(value) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error":e.to_string()}))).into_response(),
    };
    updated.normalize();
    if updated.aec_enabled && !crate::commands::audio::aec_supported() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"AEC is not supported on macOS"}))).into_response();
    }
    if let Err(e) = crate::app_config::save_dsp_settings(&updated) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":e}))).into_response();
    }
    match state.dsp_settings.write() {
        Ok(mut target) => *target = updated.clone(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"DSP settings lock failed"}))).into_response(),
    }
    state.plugins.broadcast_event(&micyou_plugin::PluginEvent::DspSettingsChanged);
    (StatusCode::OK, Json(json!({"ok":true,"dsp":updated}))).into_response()
}

async fn get_devices() -> Json<Value> {
    Json(json!({"devices": crate::commands::audio::get_audio_devices(), "prefs": crate::app_config::load_server_prefs()}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutputDeviceControl { output_device: String }

async fn set_output_device(Json(body): Json<OutputDeviceControl>) -> impl IntoResponse {
    let mut prefs = crate::app_config::load_server_prefs();
    prefs.output_device = body.output_device;
    match crate::app_config::save_server_prefs(&prefs) {
        Ok(()) => (StatusCode::OK, Json(json!({"ok":true,"restartRequired":true}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":e}))).into_response(),
    }
}

pub async fn serve(state: DashboardState) -> Result<(), String> {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/control/mute", post(set_mute))
        .route("/api/control/monitoring", post(set_monitoring))
        .route("/api/control/dsp", post(patch_dsp))
        .route("/api/audio-devices", get(get_devices))
        .route("/api/control/output-device", post(set_output_device))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", DASHBOARD_PORT)).await
        .map_err(|e| format!("dashboard bind failed: {e}"))?;
    log::info!("Dashboard listening on http://127.0.0.1:{}", DASHBOARD_PORT);
    axum::serve(listener, app).await.map_err(|e| format!("dashboard server failed: {e}"))
}
