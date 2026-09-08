/*
 * MicYou — Web-first localhost dashboard/control plane.
 * This listener is deliberately bound to 127.0.0.1 only.
 */

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use qrcode::types::Color;
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tauri::Manager;
use tokio::sync::Mutex;

use crate::{audio_output::AudioOutputHandle, plugins::PluginHost, stats::NetworkStats};

pub const DASHBOARD_PORT: u16 = 19527;
const DASHBOARD_HTML: &str = include_str!("../resources/dashboard_local.html");

#[derive(Clone)]
pub struct DashboardState {
    pub app: tauri::AppHandle,
    pub dsp_settings: Arc<RwLock<micyou_audio::dsp::AudioDspSettings>>,
    pub is_monitoring: Arc<AtomicBool>,
    pub network_stats: Arc<NetworkStats>,
    pub audio_output: Arc<AudioOutputHandle>,
    pub plugins: Arc<PluginHost>,
    pub web_server: Arc<Mutex<Option<crate::web_server::WebServer>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardStatus {
    online: bool,
    server_running: bool,
    muted: bool,
    monitoring: bool,
    web_client_count: usize,
    queued_samples: usize,
    output_buffer_ms: f64,
    sample_rate: u32,
    bitrate: u32,
    network_latency_ms: i64,
    jitter_ms: f64,
    packet_loss_rate: f64,
    audio_level: u32,
    web_dsp_active: bool,
    web_aec_supported: bool,
    dsp: Value,
    phone_urls: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingInfo {
    ready: bool,
    primary_url: String,
    urls: Vec<String>,
    qr_size: usize,
    qr_bits: Vec<bool>,
    self_signed_certificate: bool,
}

async fn local_host_only(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let allowed = host.starts_with("127.0.0.1:")
        || host == "127.0.0.1"
        || host.starts_with("localhost:")
        || host == "localhost"
        || host.starts_with("[::1]:")
        || host == "[::1]";
    if !allowed {
        return (StatusCode::FORBIDDEN, "Local dashboard only").into_response();
    }
    next.run(req).await
}

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn status(State(state): State<DashboardState>) -> Json<DashboardStatus> {
    let (server_running, web_client_count, web_telemetry, phone_urls) = {
        let web = state.web_server.lock().await;
        if let Some(web) = web.as_ref() {
            (
                web.is_running(),
                web.client_count(),
                Some(web.telemetry()),
                web.pairing_urls(),
            )
        } else {
            (false, 0, None, Vec::new())
        }
    };

    crate::audio_output::set_web_dsp_active(server_running);

    let dsp = state
        .dsp_settings
        .read()
        .ok()
        .and_then(|v| serde_json::to_value(&*v).ok())
        .unwrap_or_else(|| json!({}));

    let queued_samples = state.audio_output.queued_samples();
    let output_buffer_ms = queued_samples as f64 / 48.0;
    let (sample_rate, bitrate, jitter_ms, audio_level) = if let Some(web) = web_telemetry {
        (
            if web.sample_rate == 0 && server_running { 48_000 } else { web.sample_rate },
            web.bitrate,
            web.jitter_ms,
            web.audio_level,
        )
    } else {
        (
            state.network_stats.sample_rate.load(Ordering::Relaxed),
            state.network_stats.bitrate.load(Ordering::Relaxed),
            state.network_stats.get_jitter(),
            0,
        )
    };

    Json(DashboardStatus {
        online: true,
        server_running,
        muted: state.network_stats.is_muted(),
        monitoring: state.is_monitoring.load(Ordering::Relaxed),
        web_client_count,
        queued_samples,
        output_buffer_ms,
        sample_rate,
        bitrate,
        network_latency_ms: state.network_stats.get_rtt(),
        jitter_ms,
        packet_loss_rate: if server_running {
            0.0
        } else {
            state.network_stats.get_loss_rate()
        },
        audio_level,
        web_dsp_active: crate::audio_output::web_dsp_active(),
        web_aec_supported: false,
        dsp,
        phone_urls,
    })
}

async fn pairing(State(state): State<DashboardState>) -> impl IntoResponse {
    let urls = state
        .web_server
        .lock()
        .await
        .as_ref()
        .filter(|server| server.is_running())
        .map(|server| server.pairing_urls())
        .unwrap_or_default();
    let primary_url = urls.first().cloned().unwrap_or_default();
    if primary_url.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Web microphone server is not running"})),
        )
            .into_response();
    }

    let code = match QrCode::new(primary_url.as_bytes()) {
        Ok(code) => code,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":e.to_string()})),
            )
                .into_response()
        }
    };
    let qr_size = code.width();
    let qr_bits = code
        .to_colors()
        .into_iter()
        .map(|color| color == Color::Dark)
        .collect();
    (
        StatusCode::OK,
        Json(json!(PairingInfo {
            ready: true,
            primary_url,
            urls,
            qr_size,
            qr_bits,
            self_signed_certificate: true,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct BoolControl {
    enabled: bool,
}

async fn set_mute(
    State(state): State<DashboardState>,
    Json(body): Json<BoolControl>,
) -> Json<Value> {
    state.network_stats.set_muted(body.enabled);
    state.audio_output.set_muted(body.enabled);
    state
        .plugins
        .broadcast_event(&micyou_plugin::PluginEvent::MuteChanged {
            muted: body.enabled,
        });
    Json(json!({"ok": true, "muted": body.enabled}))
}

async fn set_monitoring(
    State(state): State<DashboardState>,
    Json(body): Json<BoolControl>,
) -> Json<Value> {
    state.is_monitoring.store(body.enabled, Ordering::Relaxed);
    state.audio_output.set_monitoring(body.enabled);
    state
        .plugins
        .broadcast_event(&micyou_plugin::PluginEvent::MonitoringChanged {
            enabled: body.enabled,
        });
    Json(json!({"ok": true, "monitoring": body.enabled}))
}

async fn patch_dsp(
    State(state): State<DashboardState>,
    Json(patch): Json<Value>,
) -> impl IntoResponse {
    let patch = match patch.as_object() {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"DSP patch must be a JSON object"})),
            )
                .into_response()
        }
    };

    let web_running = state
        .web_server
        .lock()
        .await
        .as_ref()
        .is_some_and(|web| web.is_running());
    let asks_for_web_aec = patch
        .get("aec_enabled")
        .or_else(|| patch.get("aecEnabled"))
        .and_then(Value::as_bool)
        == Some(true);
    if web_running && asks_for_web_aec {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"AEC is unavailable in Web transport because it requires the native far-end loopback reference"})),
        )
            .into_response();
    }

    let current = match state.dsp_settings.read() {
        Ok(v) => v.clone(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"DSP settings lock failed"})),
            )
                .into_response()
        }
    };
    let mut value = match serde_json::to_value(current) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":e.to_string()})),
            )
                .into_response()
        }
    };
    if let Some(obj) = value.as_object_mut() {
        for (k, v) in patch {
            obj.insert(k.clone(), v.clone());
        }
    }
    let mut updated: micyou_audio::dsp::AudioDspSettings = match serde_json::from_value(value) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":e.to_string()})),
            )
                .into_response()
        }
    };
    updated.normalize();
    if updated.aec_enabled && !crate::commands::audio::aec_supported() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"AEC is not supported on macOS"})),
        )
            .into_response();
    }
    if let Err(e) = crate::app_config::save_dsp_settings(&updated) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":e})),
        )
            .into_response();
    }
    match state.dsp_settings.write() {
        Ok(mut target) => *target = updated.clone(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"DSP settings lock failed"})),
            )
                .into_response()
        }
    }
    state.audio_output.update_web_dsp_settings(updated.clone());
    state
        .plugins
        .broadcast_event(&micyou_plugin::PluginEvent::DspSettingsChanged);
    (StatusCode::OK, Json(json!({"ok":true,"dsp":updated}))).into_response()
}

async fn get_devices() -> Json<Value> {
    Json(json!({
        "devices": crate::commands::audio::get_audio_devices(),
        "prefs": crate::app_config::load_server_prefs()
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutputDeviceControl {
    output_device: String,
}

async fn set_output_device(
    State(state): State<DashboardState>,
    Json(body): Json<OutputDeviceControl>,
) -> impl IntoResponse {
    let mut prefs = crate::app_config::load_server_prefs();
    let previous = prefs.output_device.clone();
    let buffer_ms = state
        .dsp_settings
        .read()
        .map(|settings| (settings.output_buffer_ms as usize).clamp(100, 1200))
        .unwrap_or(800);
    let requested = crate::commands::system::normalize_output_device(&body.output_device);
    if !state.audio_output.reopen(requested, buffer_ms) {
        let fallback = crate::commands::system::normalize_output_device(&previous);
        let _ = state.audio_output.reopen(fallback, buffer_ms);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Failed to open selected audio device; restored previous device"})),
        )
            .into_response();
    }
    prefs.output_device = body.output_device;
    match crate::app_config::save_server_prefs(&prefs) {
        Ok(()) => (StatusCode::OK, Json(json!({"ok":true,"restartRequired":false}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":e}))).into_response(),
    }
}

async fn set_web_server(
    State(state): State<DashboardState>,
    Json(body): Json<BoolControl>,
) -> impl IntoResponse {
    let core = state.app.state::<crate::server::ServerState>();
    let events: crate::events::SharedEvents =
        Arc::new(crate::events::TauriEventSink(state.app.clone()));

    if body.enabled {
        if state
            .web_server
            .lock()
            .await
            .as_ref()
            .is_some_and(|server| server.is_running())
        {
            return (StatusCode::OK, Json(json!({"ok":true,"running":true}))).into_response();
        }
        let prefs = crate::app_config::load_server_prefs();
        let output = crate::commands::system::normalize_output_device(&prefs.output_device);
        let resource_dir = state.app.path().resource_dir().ok();
        match crate::commands::system::start_server_inner(
            core.inner(),
            prefs.web_port,
            "web".to_string(),
            Some("0.0.0.0".to_string()),
            output,
            resource_dir,
            events,
        )
        .await
        {
            Ok(message) => {
                crate::audio_output::set_web_dsp_active(true);
                (StatusCode::OK, Json(json!({"ok":true,"running":true,"message":message}))).into_response()
            }
            Err(e) => (StatusCode::CONFLICT, Json(json!({"error":e}))).into_response(),
        }
    } else {
        match crate::commands::system::stop_server_inner(core.inner(), events).await {
            Ok(message) => {
                crate::audio_output::set_web_dsp_active(false);
                (StatusCode::OK, Json(json!({"ok":true,"running":false,"message":message}))).into_response()
            }
            Err(e) if e == "Server is not running" => {
                crate::audio_output::set_web_dsp_active(false);
                (StatusCode::OK, Json(json!({"ok":true,"running":false}))).into_response()
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":e}))).into_response(),
        }
    }
}

pub async fn serve(state: DashboardState) -> Result<(), String> {
    let watcher = state.clone();
    tokio::spawn(async move {
        loop {
            let running = watcher
                .web_server
                .lock()
                .await
                .as_ref()
                .is_some_and(|server| server.is_running());
            crate::audio_output::set_web_dsp_active(running);
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/pairing", get(pairing))
        .route("/api/control/server", post(set_web_server))
        .route("/api/control/mute", post(set_mute))
        .route("/api/control/monitoring", post(set_monitoring))
        .route("/api/control/dsp", post(patch_dsp))
        .route("/api/audio-devices", get(get_devices))
        .route("/api/control/output-device", post(set_output_device))
        .layer(middleware::from_fn(local_host_only))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", DASHBOARD_PORT))
        .await
        .map_err(|e| format!("dashboard bind failed: {e}"))?;
    log::info!("Dashboard listening on http://127.0.0.1:{DASHBOARD_PORT}");
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("dashboard server failed: {e}"))
}
