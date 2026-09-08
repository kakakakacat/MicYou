# MicYou Web-first MVP

This branch moves MicYou toward a web-first product without replacing the proven native audio core.

## Product shape

- **Phone:** mobile browser/PWA captures microphone audio.
- **PC control surface:** browser dashboard served by the native MicYou process.
- **Native core:** existing Rust transport, buffering, DSP, audio output, virtual-device integration, plugins, and platform-specific routing remain authoritative.
- **Desktop shell:** Tauri remains in place during the migration. A later milestone can reduce it to tray/daemon responsibilities once the browser dashboard reaches feature parity.

## Current endpoints

When Web mode is running on the default port (`8443`):

- `https://<pc-ip>:8443/` — phone microphone client.
- `https://127.0.0.1:8443/dashboard` — PC web dashboard.
- `https://127.0.0.1:8443/api/status` — lightweight live status JSON.
- `/manifest.webmanifest` and `/service-worker.js` — initial PWA plumbing.
- `/ws` — existing low-latency browser audio WebSocket.

## What this MVP keeps unchanged

The browser still sends 48 kHz mono Float32 frames over the existing WebSocket path. The Rust server converts them to PCM16 and injects them into the existing MicYou audio packet pipeline. This deliberately avoids touching the jitter buffer, DSP chain, platform virtual-audio routing, or plugin system in the first migration step.

## Status API

`GET /api/status` currently returns:

```json
{
  "running": true,
  "client_count": 1,
  "port": 8443,
  "lan_ips": ["192.168.1.10"],
  "phone_urls": ["https://192.168.1.10:8443/"]
}
```

The dashboard polls this endpoint once per second to show connection state and the LAN URL to open on a phone.

## Migration milestones

### M1 — Web-first MVP (this branch)

- Keep existing Rust audio core.
- Keep the existing phone Web client.
- Add browser dashboard.
- Add live server/client status API.
- Add manifest/service-worker plumbing.
- Do not remove Android or Tauri UI yet.

### M2 — Dashboard control parity

Expose safe local APIs for:

- mute/unmute;
- monitoring;
- noise suppression;
- AEC;
- AGC;
- VAD;
- gain/EQ;
- output-device selection;
- server start/stop and connection settings;
- realtime latency, packet-loss, jitter, bitrate, and level/spectrum telemetry.

The API should reuse the same Rust state and command implementations used by Tauri rather than duplicating DSP logic in JavaScript.

### M3 — Better phone onboarding

- Generate a QR code locally in the dashboard.
- Add pairing/session tokens rather than relying only on LAN-origin checks.
- Improve certificate onboarding for iOS/Android browsers.
- Add a proper installable PWA icon set and offline shell.
- Move capture to `AudioWorklet` if the current browser path is not already doing so consistently.

### M4 — Desktop daemon/tray mode

Once the dashboard has feature parity:

- launch the Rust core at login;
- keep a minimal tray/menu-bar icon;
- `Open Dashboard` opens the local browser UI;
- keep virtual-audio device setup in native code;
- make the full Tauri window optional or remove it after a deprecation period.

### M5 — Optional internet mode

Keep WebSocket/PCM (or Opus) for LAN simplicity. Add WebRTC only for remote/P2P use cases where NAT traversal and TURN are required.

## Security notes

The current Web mode uses a self-signed TLS certificate and accepts localhost/LAN origins. Before exposing control APIs, add an explicit pairing token/session and CSRF-resistant mutation model. Control endpoints should not be remotely writable merely because the requester is on the same LAN.

## Compatibility strategy

Do not delete the Android client during the early migration. Keep Android as a fallback until the browser capture path is verified across Chrome/Android and Safari/iOS, especially backgrounding, screen lock, Bluetooth input selection, echo cancellation behavior, and long-session stability.
