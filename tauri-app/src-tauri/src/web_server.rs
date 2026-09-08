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

use rcgen::{CertificateParams, KeyPair, SanType};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_WEB_PORT: u16 = 8443;

pub struct WebServer {
    cancel_token: std::sync::Mutex<CancellationToken>,
    client_count: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

pub struct GeneratedCert {
    pub cert_pem: String,
    pub key_pem: String,
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

pub fn generate_self_signed_cert_pem() -> Result<GeneratedCert, String> {
    let lan_ips = get_lan_ips();

    let mut params = CertificateParams::new(vec!["localhost".to_string()])
        .map_err(|e| format!("Failed to create cert params: {}", e))?;

    params.subject_alt_names.push(SanType::IpAddress(IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    )));
    for ip_str in &lan_ips {
        if let Ok(ip) = ip_str.parse::<IpAddr>() {
            params.subject_alt_names.push(SanType::IpAddress(ip));
        }
    }

    let key_pair =
        KeyPair::generate().map_err(|e| format!("Failed to generate key pair: {}", e))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("Failed to sign certificate: {}", e))?;

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

use crate::events::SharedEvents;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::serve::Listener;
use axum::{Json, Router};
use rustls::pki_types::CertificateDer;
use rustls::ServerConfig;
use serde::Serialize;
use std::io::BufReader;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::{server::TlsStream, TlsAcceptor};

const MAX_TLS_HANDSHAKES: usize = 32;
const MAX_WEBSOCKET_CONNECTIONS: usize = 8;
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const WEB_CLIENT_HTML: &str = include_str!("../resources/web_client.html");
const WEB_DASHBOARD_HTML: &str = include_str!("../resources/web_dashboard.html");
const WEB_MANIFEST: &str = include_str!("../resources/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("../resources/service-worker.js");
const ALPINE_JS: &str = include_str!("../resources/alpine.min.js");

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

async fn handle_websocket(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<WebServerState>,
) -> impl IntoResponse {
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    if !is_valid_origin(origin) {
        return (StatusCode::FORBIDDEN, "Invalid origin").into_response();
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
    log::info!("Web client connected (total: {})", count);

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
                    log::warn!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    }

    if state.active_sender.deactivate(generation) {
        let remaining = decrement_client_count(&state.client_count);
        state.events.web_client_count(remaining as u32);
        log::info!("Web client disconnected (remaining: {})", remaining);
        if remaining == 0 {
            state.events.device_disconnected();
        }
    } else {
        log::info!("Replaced Web client closed");
    }
}

fn decrement_client_count(client_count: &AtomicUsize) -> usize {
    client_count
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| count.checked_sub(1))
        .map(|previous| previous - 1)
        .unwrap_or(0)
}

async fn serve_mobile() -> impl IntoResponse {
    let with_manifest = WEB_CLIENT_HTML.replace(
        "</head>",
        "<link rel=\"manifest\" href=\"/manifest.webmanifest\"><meta name=\"theme-color\" content=\"#111418\"></head>",
    );
    let pwa_ready = with_manifest.replace(
        "</body>",
        "<script>if('serviceWorker' in navigator){navigator.serviceWorker.register('/service-worker.js').catch(()=>{});}</script></body>",
    );
    Html(pwa_ready)
}

async fn serve_dashboard() -> impl IntoResponse {
    Html(WEB_DASHBOARD_HTML)
}

async fn serve_alpine_js() -> impl IntoResponse {
    ([("Content-Type", "application/javascript")], ALPINE_JS)
}

async fn serve_manifest() -> impl IntoResponse {
    (
        [("Content-Type", "application/manifest+json")],
        WEB_MANIFEST,
    )
}

async fn serve_service_worker() -> impl IntoResponse {
    (
        [
            ("Content-Type", "application/javascript"),
            ("Cache-Control", "no-cache"),
            ("Service-Worker-Allowed", "/"),
        ],
        SERVICE_WORKER,
    )
}

#[derive(Serialize)]
struct WebStatus {
    running: bool,
    client_count: usize,
    port: u16,
    lan_ips: Vec<String>,
    phone_urls: Vec<String>,
}

async fn serve_status(State(state): State<WebServerState>) -> Json<WebStatus> {
    let lan_ips = get_lan_ips();
    let phone_urls = lan_ips
        .iter()
        .map(|ip| format!("https://{}:{}/", ip, state.port))
        .collect();
    Json(WebStatus {
        running: true,
        client_count: state.client_count.load(Ordering::SeqCst),
        port: state.port,
        lan_ips,
        phone_urls,
    })
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

#[derive(Clone)]
pub struct WebServerState {
    pub events: SharedEvents,
    pub audio_tx: tokio::sync::mpsc::Sender<(u64, micyou_protocol::micyou::AudioPacketMessage)>,
    pub client_count: Arc<AtomicUsize>,
    active_sender: Arc<ActiveWebSender>,
    pub websocket_slots: Arc<Semaphore>,
    pub port: u16,
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
                                    Ok(Err(e)) => log::debug!("TLS handshake failed: {}", e),
                                    Err(_) => log::debug!("TLS handshake timed out for {}", addr),
                                }
                            });
                        }
                        Err(e) => {
                            log::warn!("TCP accept error: {}", e);
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
            task: std::sync::Mutex::new(None),
        }
    }

    pub fn client_count(&self) -> usize {
        self.client_count.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
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

        let state = WebServerState {
            events,
            audio_tx,
            client_count: self.client_count.clone(),
            active_sender: Arc::new(ActiveWebSender::default()),
            websocket_slots: Arc::new(Semaphore::new(MAX_WEBSOCKET_CONNECTIONS)),
            port,
        };

        let app = Router::new()
            .route("/", get(serve_mobile))
            .route("/dashboard", get(serve_dashboard))
            .route("/api/status", get(serve_status))
            .route("/manifest.webmanifest", get(serve_manifest))
            .route("/service-worker.js", get(serve_service_worker))
            .route("/alpine.min.js", get(serve_alpine_js))
            .route("/ws", get(handle_websocket))
            .with_state(state);

        let cert = load_or_generate_cert_pem()?;
        let cert_chain: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(cert.cert_pem.as_bytes()))
                .filter_map(|r| r.ok())
                .collect();

        let private_key = rustls_pemfile::private_key(&mut BufReader::new(cert.key_pem.as_bytes()))
            .map_err(|e| format!("Failed to read private key: {}", e))?
            .ok_or("No private key found in PEM")?;

        let mut tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)
            .map_err(|e| format!("TLS config error: {}", e))?;
        tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let acceptor = TlsAcceptor::from(Arc::new(tls_config));
        let addr: SocketAddr = format!("0.0.0.0:{}", port)
            .parse()
            .map_err(|e| format!("Invalid address: {}", e))?;
        let tcp = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("Web server bind error: {}", e))?;

        let (completed, completed_rx) = tokio::sync::mpsc::channel(MAX_TLS_HANDSHAKES);
        let tls_listener = TlsListener {
            tcp,
            acceptor,
            handshake_slots: Arc::new(Semaphore::new(MAX_TLS_HANDSHAKES)),
            completed,
            completed_rx,
        };

        log::info!("Web microphone: https://0.0.0.0:{}", port);
        log::info!("Web dashboard: https://127.0.0.1:{}/dashboard", port);

        let new_token = CancellationToken::new();
        {
            let mut token_guard = self.cancel_token.lock().unwrap();
            *token_guard = new_token.clone();
        }
        let cancel = new_token;
        let running = self.running.clone();
        let client_count = self.client_count.clone();

        running.store(true, Ordering::SeqCst);
        let task = tokio::spawn(async move {
            axum::serve(tls_listener, app)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
                .ok();
            running.store(false, Ordering::SeqCst);
            client_count.store(0, Ordering::SeqCst);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_self_signed_cert_pem() {
        let cert = generate_self_signed_cert_pem().expect("certificate should be generated");
        assert!(cert.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(cert.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn test_get_lan_ips() {
        for ip in get_lan_ips() {
            assert!(ip.parse::<IpAddr>().is_ok(), "Invalid IP: {}", ip);
        }
    }

    #[test]
    fn test_float32_to_pcm16() {
        let input = [1.0f32, 0.0f32, -1.0f32]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let pcm = float32_to_pcm16(&input);
        assert_eq!(pcm.len(), 6);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), 32767);
        assert_eq!(i16::from_le_bytes([pcm[2], pcm[3]]), 0);
        assert_eq!(i16::from_le_bytes([pcm[4], pcm[5]]), -32767);
    }

    #[test]
    fn decrement_client_count_does_not_underflow_after_stop_reset() {
        let count = AtomicUsize::new(0);
        assert_eq!(decrement_client_count(&count), 0);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn new_web_sender_closes_replaced_sender() {
        let senders = ActiveWebSender::default();
        let (first_generation, first_cancel, first_replaced) = senders.activate();
        let (second_generation, second_cancel, second_replaced) = senders.activate();
        assert!(!first_replaced);
        assert!(second_replaced);
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert!(!senders.is_current(first_generation));
        assert!(senders.is_current(second_generation));
    }
}
