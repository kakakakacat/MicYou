/*
 * MicYou — Turns your Android device into a high-quality PC microphone.
 * Copyright (C) 2026 LanRhyme <https://github.com/LanRhyme/MicYou>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version, with the MicYou Plugin Exception.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU General Public License for more details.
 */

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::serve::Listener;
use axum::Router;
use rand::RngCore;
use rcgen::{CertificateParams, KeyPair, SanType};
use rustls::pki_types::CertificateDer;
use rustls::ServerConfig;
use serde::Serialize;
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{
    AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_util::sync::CancellationToken;

use crate::events::SharedEvents;

pub const DEFAULT_WEB_PORT: u16 = 8443;
const MAX_TLS_HANDSHAKES: usize = 32;
const MAX_WEBSOCKET_CONNECTIONS: usize = 8;
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const COOKIE_NAME: &str = "micyou_pair";

const WEB_CLIENT_HTML: &str = include_str!("../resources/web_client.html");
const ALPINE_JS: &str = include_str!("../resources/alpine.min.js");
const MANIFEST_JSON: &str = include_str!("../resources/manifest.webmanifest");
const SERVICE_WORKER_JS: &str = include_str!("../resources/service-worker.js");

const UNPAIRED_HTML: &str = r#"<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="theme-color" content="#111418"><title>MicYou Pairing</title><style>body{font-family:-apple-system,BlinkMacSystemFont,Segoe UI,sans-serif;background:#111418;color:#e2e2e6;display:grid;place-items:center;min-height:100vh;margin:0;padding:24px}.c{max-width:430px;background:#1e2024;border:1px solid #42474f;border-radius:24px;padding:28px;text-align:center}.i{font-size:42px}.m{color:#c2c6cf;line-height:1.6}</style></head><body><div class="c"><div class="i">🎙️</div><h2>MicYou pairing required</h2><p class="m">Open MicYou Dashboard on your computer and scan its pairing QR code. This prevents other devices on the same network from taking over your microphone stream.</p></div></body></html>"#;

pub struct GeneratedCert {
    pub cert_pem: String,
    pub key_pem: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebTelemetrySnapshot {
    pub audio_level: u32,
    pub bitrate: u32,
    pub sample_rate: u32,
    pub jitter_ms: f64,
    pub packet_count: u64,
    pub bytes_received: u64,
}

#[derive(Default)]
struct WebTelemetry {
    audio_level: AtomicU32,
    bitrate: AtomicU32,
    sample_rate: AtomicU32,
    jitter_bits: AtomicU64,
    packet_count: AtomicU64,
    bytes_received: AtomicU64,
    last_arrival_ms: AtomicU64,
    rate_window_start_ms: AtomicU64,
    rate_window_bytes: AtomicU64,
}

impl WebTelemetry {
    fn record_packet(&self, data: &[u8]) {
        let now = now_ms();
        self.sample_rate.store(48_000, Ordering::Relaxed);
        self.packet_count.fetch_add(1, Ordering::Relaxed);
        self.bytes_received
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        let samples = data.len() / 4;
        if samples > 0 {
            let mut sum = 0.0f64;
            for chunk in data.chunks_exact(4) {
                let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64;
                sum += sample * sample;
            }
            let rms = (sum / samples as f64).sqrt();
            self.audio_level
                .store((rms * 500.0).min(100.0) as u32, Ordering::Relaxed);

            let previous = self.last_arrival_ms.swap(now, Ordering::Relaxed);
            if previous > 0 {
                let observed = now.saturating_sub(previous) as f64;
                let expected = samples as f64 / 48_000.0 * 1000.0;
                let deviation = (observed - expected).abs();
                let old = f64::from_bits(self.jitter_bits.load(Ordering::Relaxed));
                let smoothed = if old > 0.0 {
                    old * 0.8 + deviation * 0.2
                } else {
                    deviation
                };
                self.jitter_bits
                    .store(smoothed.to_bits(), Ordering::Relaxed);
            }
        }

        let start = self.rate_window_start_ms.load(Ordering::Relaxed);
        if start == 0 {
            self.rate_window_start_ms.store(now, Ordering::Relaxed);
        }
        self.rate_window_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        let start = self.rate_window_start_ms.load(Ordering::Relaxed);
        let elapsed = now.saturating_sub(start);
        if elapsed >= 1000 {
            let bytes = self.rate_window_bytes.swap(0, Ordering::Relaxed);
            self.rate_window_start_ms.store(now, Ordering::Relaxed);
            let bps = bytes
                .saturating_mul(8)
                .saturating_mul(1000)
                .checked_div(elapsed.max(1))
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32;
            self.bitrate.store(bps, Ordering::Relaxed);
        }
    }

    fn reset_live(&self) {
        self.audio_level.store(0, Ordering::Relaxed);
        self.bitrate.store(0, Ordering::Relaxed);
        self.last_arrival_ms.store(0, Ordering::Relaxed);
        self.rate_window_bytes.store(0, Ordering::Relaxed);
        self.rate_window_start_ms.store(0, Ordering::Relaxed);
    }

    fn snapshot(&self) -> WebTelemetrySnapshot {
        WebTelemetrySnapshot {
            audio_level: self.audio_level.load(Ordering::Relaxed),
            bitrate: self.bitrate.load(Ordering::Relaxed),
            sample_rate: self.sample_rate.load(Ordering::Relaxed),
            jitter_ms: f64::from_bits(self.jitter_bits.load(Ordering::Relaxed)),
            packet_count: self.packet_count.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
        }
    }
}

pub struct WebServer {
    cancel_token: std::sync::Mutex<CancellationToken>,
    client_count: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
    port: Arc<AtomicU16>,
    pairing_token: Arc<String>,
    telemetry: Arc<WebTelemetry>,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

pub fn cert_cache_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("micyou_web_cert");
    std::fs::create_dir_all(&dir).ok();
    dir
}

pub fn get_lan_ips() -> Vec<String> {
    let mut ips = Vec::new();
    if let Ok(interfaces) = local_ip_address::list_afinet_netifas() {
        for (_, ip) in interfaces {
            if ip.is_loopback() || !ip.is_ipv4() {
                continue;
            }
            let ip_str = ip.to_string();
            if ip_str.starts_with("198.18.") || ip_str.starts_with("169.254.") {
                continue;
            }
            ips.push(ip_str);
        }
    }
    ips.sort();
    ips.dedup();
    ips
}

fn generate_pairing_token() -> String {
    let mut random = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut random);
    random.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn generate_self_signed_cert_pem() -> Result<GeneratedCert, String> {
    let lan_ips = get_lan_ips();
    let mut params = CertificateParams::new(vec!["localhost".to_string()])
        .map_err(|e| format!("Failed to create cert params: {e}"))?;
    params.subject_alt_names.push(SanType::IpAddress(IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    )));
    for ip_str in &lan_ips {
        if let Ok(ip) = ip_str.parse::<IpAddr>() {
            params.subject_alt_names.push(SanType::IpAddress(ip));
        }
    }
    let key_pair = KeyPair::generate().map_err(|e| format!("Failed to generate key pair: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("Failed to sign certificate: {e}"))?;
    Ok(GeneratedCert {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}

pub fn load_or_generate_cert_pem() -> Result<GeneratedCert, String> {
    let cache_dir = cert_cache_dir();
    let cert_path = cache_dir.join("cert.pem");
    let key_path = cache_dir.join("key.pem");
    if cert_path.exists() && key_path.exists() {
        if let (Ok(cert_pem), Ok(key_pem)) = (
            std::fs::read_to_string(&cert_path),
            std::fs::read_to_string(&key_path),
        ) {
            if !cert_pem.is_empty() && !key_pem.is_empty() {
                return Ok(GeneratedCert { cert_pem, key_pem });
            }
        }
    }
    let cert = generate_self_signed_cert_pem()?;
    std::fs::write(&cert_path, &cert.cert_pem).ok();
    std::fs::write(&key_path, &cert.key_pem).ok();
    Ok(cert)
}

pub fn float32_to_pcm16(float32_bytes: &[u8]) -> Vec<u8> {
    let num_floats = float32_bytes.len() / 4;
    let mut pcm = Vec::with_capacity(num_floats * 2);
    for i in 0..num_floats {
        let offset = i * 4;
        let sample = f32::from_le_bytes([
            float32_bytes[offset],
            float32_bytes[offset + 1],
            float32_bytes[offset + 2],
            float32_bytes[offset + 3],
        ]);
        let clamped = sample.clamp(-1.0, 1.0);
        let pcm_sample = (clamped * 32767.0) as i16;
        pcm.extend_from_slice(&pcm_sample.to_le_bytes());
    }
    pcm
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in value.split(';') {
        let mut pair = part.trim().splitn(2, '=');
        if pair.next()? == COOKIE_NAME {
            return pair.next();
        }
    }
    None
}

fn is_paired(headers: &HeaderMap, token: &str) -> bool {
    cookie_token(headers).is_some_and(|value| value == token)
}

fn is_valid_origin(origin: Option<&str>) -> bool {
    match origin {
        None => true,
        Some(o) => {
            let o = o.to_lowercase();
            o.contains("localhost")
                || o.contains("127.0.0.1")
                || get_lan_ips().iter().any(|ip| o.contains(ip))
        }
    }
}

#[derive(Clone)]
pub struct WebServerState {
    pub events: SharedEvents,
    pub audio_tx: tokio::sync::mpsc::Sender<(u64, micyou_protocol::micyou::AudioPacketMessage)>,
    pub client_count: Arc<AtomicUsize>,
    active_sender: Arc<ActiveWebSender>,
    pub websocket_slots: Arc<Semaphore>,
    pairing_token: Arc<String>,
    telemetry: Arc<WebTelemetry>,
    port: u16,
}

async fn serve_html(
    State(state): State<WebServerState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let query_ok = query
        .get("token")
        .is_some_and(|token| token == state.pairing_token.as_str());
    if !query_ok && !is_paired(&headers, &state.pairing_token) {
        return (StatusCode::UNAUTHORIZED, Html(UNPAIRED_HTML)).into_response();
    }

    if query_ok {
        let cookie = format!(
            "{COOKIE_NAME}={}; Max-Age=2592000; Path=/; HttpOnly; Secure; SameSite=Strict",
            state.pairing_token
        );
        return ([(header::SET_COOKIE, cookie)], Html(WEB_CLIENT_HTML)).into_response();
    }
    Html(WEB_CLIENT_HTML).into_response()
}

async fn serve_alpine_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], ALPINE_JS)
}

async fn serve_manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        MANIFEST_JSON,
    )
}

async fn serve_service_worker() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        SERVICE_WORKER_JS,
    )
}

async fn redirect_dashboard() -> Redirect {
    Redirect::temporary("http://127.0.0.1:19527/")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicStatus {
    running: bool,
    client_count: usize,
    telemetry: WebTelemetrySnapshot,
}

async fn serve_status(State(state): State<WebServerState>) -> axum::Json<PublicStatus> {
    axum::Json(PublicStatus {
        running: true,
        client_count: state.client_count.load(Ordering::SeqCst),
        telemetry: state.telemetry.snapshot(),
    })
}

async fn handle_websocket(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<WebServerState>,
) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !is_valid_origin(origin) {
        return (StatusCode::FORBIDDEN, "Invalid origin").into_response();
    }
    if !is_paired(&headers, &state.pairing_token) {
        return (StatusCode::UNAUTHORIZED, "Pairing required").into_response();
    }
    let permit = match state.websocket_slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many WebSocket connections",
            )
                .into_response()
        }
    };
    ws.on_upgrade(move |socket| handle_ws_socket(socket, state, permit))
}

async fn handle_ws_socket(
    mut socket: WebSocket,
    state: WebServerState,
    _permit: OwnedSemaphorePermit,
) {
    let (generation, cancel, replaced) = state.active_sender.activate();
    let count = if replaced {
        state.client_count.load(Ordering::SeqCst)
    } else {
        state.client_count.fetch_add(1, Ordering::SeqCst) + 1
    };
    state.events.web_client_count(count as u32);
    log::info!("Web client connected (total: {count})");

    if count == 1 && !replaced {
        state
            .events
            .device_connected(crate::tcp_server::DeviceInfo {
                name: "Web Browser".to_string(),
                ip: "browser".to_string(),
                latency: 0,
            });
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(data))) => {
                    if !state.active_sender.is_current(generation) {
                        break;
                    }
                    if data.len() > 64 * 1024 {
                        log::warn!("Web audio packet too large ({} bytes), dropping", data.len());
                        continue;
                    }
                    if data.len() % 4 != 0 {
                        log::warn!("Web audio packet not aligned to 4 bytes, dropping");
                        continue;
                    }

                    state.telemetry.record_packet(&data);
                    state.events.audio_level(state.telemetry.snapshot().audio_level);
                    let pcm = float32_to_pcm16(&data);
                    let packet = micyou_protocol::micyou::AudioPacketMessage {
                        buffer: pcm,
                        sample_rate: 48000,
                        channel_count: 1,
                        audio_format: 2,
                        codec: micyou_protocol::CODEC_PCM,
                    };
                    match state.audio_tx.try_send((generation, packet)) {
                        Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(e)) => {
                    log::warn!("WebSocket error: {e}");
                    break;
                }
                _ => {}
            }
        }
    }

    if state.active_sender.deactivate(generation) {
        let remaining = decrement_client_count(&state.client_count);
        state.events.web_client_count(remaining as u32);
        log::info!("Web client disconnected (remaining: {remaining})");
        if remaining == 0 {
            state.telemetry.reset_live();
            state.events.audio_level(0);
            state.events.device_disconnected();
        }
    } else {
        log::info!("Replaced Web client closed");
    }
}

fn decrement_client_count(client_count: &AtomicUsize) -> usize {
    client_count
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
            count.checked_sub(1)
        })
        .map(|previous| previous - 1)
        .unwrap_or(0)
}

#[derive(Default)]
struct ActiveWebSender {
    generation: AtomicU64,
    cancel: std::sync::Mutex<Option<(u64, CancellationToken)>>,
}

impl ActiveWebSender {
    fn activate(&self) -> (u64, CancellationToken, bool) {
        let cancel = CancellationToken::new();
        let mut active = self.cancel.lock().unwrap();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let replaced = if let Some((_, previous)) = active.replace((generation, cancel.clone())) {
            previous.cancel();
            true
        } else {
            false
        };
        (generation, cancel, replaced)
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    fn deactivate(&self, generation: u64) -> bool {
        self.cancel
            .lock()
            .map(|mut active| {
                if active
                    .as_ref()
                    .map(|(active_generation, _)| *active_generation)
                    == Some(generation)
                {
                    active.take();
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false)
    }
}

struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    handshake_slots: Arc<Semaphore>,
    completed: tokio::sync::mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
    completed_rx: tokio::sync::mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                Some(accepted) = self.completed_rx.recv() => return accepted,
                accept_result = self.tcp.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            let permit = match self.handshake_slots.clone().try_acquire_owned() {
                                Ok(permit) => permit,
                                Err(_) => continue,
                            };
                            let acceptor = self.acceptor.clone();
                            let completed = self.completed.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                                    Ok(Ok(tls)) => { let _ = completed.send((tls, addr)).await; }
                                    Ok(Err(e)) => log::debug!("TLS handshake failed: {e}"),
                                    Err(_) => log::debug!("TLS handshake timed out for {addr}"),
                                }
                            });
                        }
                        Err(e) => {
                            log::warn!("TCP accept error: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

impl Default for WebServer {
    fn default() -> Self {
        Self::new()
    }
}

impl WebServer {
    pub fn new() -> Self {
        Self {
            cancel_token: std::sync::Mutex::new(CancellationToken::new()),
            client_count: Arc::new(AtomicUsize::new(0)),
            running: Arc::new(AtomicBool::new(false)),
            port: Arc::new(AtomicU16::new(DEFAULT_WEB_PORT)),
            pairing_token: Arc::new(generate_pairing_token()),
            telemetry: Arc::new(WebTelemetry::default()),
            task: std::sync::Mutex::new(None),
        }
    }

    pub fn client_count(&self) -> usize {
        self.client_count.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn telemetry(&self) -> WebTelemetrySnapshot {
        self.telemetry.snapshot()
    }

    pub fn pairing_token(&self) -> &str {
        self.pairing_token.as_str()
    }

    pub fn pairing_urls(&self) -> Vec<String> {
        let port = self.port.load(Ordering::Relaxed);
        get_lan_ips()
            .into_iter()
            .map(|ip| format!("https://{ip}:{port}/?token={}", self.pairing_token))
            .collect()
    }

    pub async fn start(
        &self,
        port: u16,
        events: SharedEvents,
        audio_tx: tokio::sync::mpsc::Sender<(u64, micyou_protocol::micyou::AudioPacketMessage)>,
    ) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Web server is already running".to_string());
        }
        self.port.store(port, Ordering::Relaxed);
        let state = WebServerState {
            events,
            audio_tx,
            client_count: self.client_count.clone(),
            active_sender: Arc::new(ActiveWebSender::default()),
            websocket_slots: Arc::new(Semaphore::new(MAX_WEBSOCKET_CONNECTIONS)),
            pairing_token: self.pairing_token.clone(),
            telemetry: self.telemetry.clone(),
            port,
        };

        let app = Router::new()
            .route("/", get(serve_html))
            .route("/alpine.min.js", get(serve_alpine_js))
            .route("/manifest.webmanifest", get(serve_manifest))
            .route("/service-worker.js", get(serve_service_worker))
            .route("/dashboard", get(redirect_dashboard))
            .route("/api/status", get(serve_status))
            .route("/ws", get(handle_websocket))
            .with_state(state);

        let cert = load_or_generate_cert_pem()?;
        let cert_chain: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(cert.cert_pem.as_bytes()))
                .filter_map(|r| r.ok())
                .collect();
        let private_key = rustls_pemfile::private_key(&mut BufReader::new(cert.key_pem.as_bytes()))
            .map_err(|e| format!("Failed to read private key: {e}"))?
            .ok_or("No private key found in PEM")?;
        let mut tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)
            .map_err(|e| format!("TLS config error: {e}"))?;
        tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(tls_config));

        let addr: SocketAddr = format!("0.0.0.0:{port}")
            .parse()
            .map_err(|e| format!("Invalid address: {e}"))?;
        let tcp = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("Web server bind error: {e}"))?;
        let (completed, completed_rx) = tokio::sync::mpsc::channel(MAX_TLS_HANDSHAKES);
        let tls_listener = TlsListener {
            tcp,
            acceptor,
            handshake_slots: Arc::new(Semaphore::new(MAX_TLS_HANDSHAKES)),
            completed,
            completed_rx,
        };

        log::info!("Web server listening on https://0.0.0.0:{port}");
        let new_token = CancellationToken::new();
        {
            let mut token_guard = self.cancel_token.lock().unwrap();
            *token_guard = new_token.clone();
        }
        let cancel = new_token;
        let running = self.running.clone();
        let client_count = self.client_count.clone();
        let telemetry = self.telemetry.clone();
        running.store(true, Ordering::SeqCst);

        let task = tokio::spawn(async move {
            axum::serve(tls_listener, app)
                .with_graceful_shutdown(async move {
                    cancel.cancelled().await;
                })
                .await
                .ok();
            running.store(false, Ordering::SeqCst);
            client_count.store(0, Ordering::SeqCst);
            telemetry.reset_live();
        });
        *self.task.lock().unwrap() = Some(task);
        Ok(())
    }

    pub async fn stop(&self) {
        if let Ok(token_guard) = self.cancel_token.lock() {
            token_guard.cancel();
        }
        let task = self.task.lock().ok().and_then(|mut task| task.take());
        if let Some(mut task) = task {
            if tokio::time::timeout(std::time::Duration::from_secs(3), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        self.running.store(false, Ordering::SeqCst);
        self.client_count.store(0, Ordering::SeqCst);
        self.telemetry.reset_live();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_self_signed_cert_pem() {
        let cert = generate_self_signed_cert_pem().unwrap();
        assert!(cert.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(cert.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn test_get_lan_ips_are_valid() {
        for ip in get_lan_ips() {
            assert!(ip.parse::<IpAddr>().is_ok());
        }
    }

    #[test]
    fn test_float32_to_pcm16() {
        let input = [1.0f32, 0.0f32, -1.0f32]
            .into_iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>();
        let pcm = float32_to_pcm16(&input);
        assert_eq!(pcm.len(), 6);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), 32767);
        assert_eq!(i16::from_le_bytes([pcm[2], pcm[3]]), 0);
        assert_eq!(i16::from_le_bytes([pcm[4], pcm[5]]), -32767);
    }

    #[test]
    fn pairing_token_is_random_and_nontrivial() {
        let a = generate_pairing_token();
        let b = generate_pairing_token();
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
    }

    #[test]
    fn decrement_does_not_underflow() {
        let count = AtomicUsize::new(0);
        assert_eq!(decrement_client_count(&count), 0);
    }
}
