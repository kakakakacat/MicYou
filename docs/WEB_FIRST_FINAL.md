# MicYou Web-first architecture

This fork keeps MicYou's native Rust audio/virtual-device core and makes the browser the primary interaction surface.

## Final shape

```text
Phone browser / home-screen launch
        |
        | HTTPS + paired WSS audio
        v
0.0.0.0:8443
MicYou Web microphone server
        |
        | Float32 -> PCM16 -> existing stream pipeline
        v
Dedicated Web DSP path
(NS / AGC / EQ / VAD / gain / dereverb; AEC excluded)
        |
        v
Persistent native AudioOutput
        |
        v
Virtual microphone
(VB-CABLE / PipeWire / BlackHole)

PC browser
        |
        | localhost only
        v
127.0.0.1:19527
Dashboard + control plane
```

## Daily use

1. Start MicYou Core.
2. Open `http://127.0.0.1:19527/`, or use the tray item **Open Dashboard**.
3. Fresh installs default to Web mode. If Web mode is the persisted mode, the phone server starts automatically.
4. Scan the pairing QR with the phone.
5. On the first visit only, accept the local self-signed HTTPS certificate warning and allow microphone access.
6. Press the microphone button on the phone page.
7. Choose MicYou's configured virtual microphone in the target app.

## Pairing and LAN security

The phone page is no longer an unauthenticated LAN endpoint.

- Each Web server instance generates a cryptographically random 24-byte pairing token.
- The Dashboard displays a QR containing `https://<LAN-IP>:<port>/?token=<random-token>`.
- A successful paired visit receives a `Secure`, `HttpOnly`, `SameSite=Strict` cookie.
- `/ws` rejects clients without that pairing cookie.
- A new server instance gets a new token, invalidating the old pairing cookie.
- The PC control plane binds only to `127.0.0.1:19527` and also validates the HTTP Host header to reduce DNS-rebinding exposure.

## Dashboard features

The local Dashboard provides:

- Web server start / stop
- pairing QR and pairing URL
- connected phone client count
- browser audio level
- browser input bitrate
- browser packet-arrival jitter
- sample rate
- output queue duration
- mute state
- monitoring state
- Web DSP activity
- final-output mute / unmute
- local monitoring
- noise suppression toggle
- automatic gain toggle
- immediate output-device switching with rollback on failure

The Dashboard API includes:

```text
GET  /api/status
GET  /api/pairing
GET  /api/audio-devices
POST /api/control/server
POST /api/control/mute
POST /api/control/monitoring
POST /api/control/dsp
POST /api/control/output-device
```

These endpoints are available only from the localhost listener.

## DSP behavior

Upstream MicYou intentionally skips its normal server-side DSP branch when `mode == "web"`. This fork therefore uses a dedicated DSP processor immediately before the persistent native output device while Web mode is active.

That lets Web mode reuse MicYou's existing processing settings without double-processing Android/Wi-Fi/USB streams.

Supported in the Web DSP path:

- Noise suppression (RNNoise / PureVox / Speexdsp according to existing settings)
- Dereverberation
- Equalizer
- Gain/amplification
- AGC
- VAD

### AEC limitation

AEC is deliberately unavailable in Web transport in this implementation. MicYou's AEC requires the native far-end speaker loopback reference owned by the original server DSP path. Enabling AEC from the Web Dashboard is rejected instead of pretending it is active.

Native non-Web modes keep their existing AEC behavior.

## Mute behavior

Mute is enforced at the final native audio-output layer. This makes mute transport-independent and fixes the previous Web-mode gap where changing only the logical mute state could still leave audio reaching the virtual microphone.

The legacy Android `MuteMessage` is still sent in native phone modes so Android UI/state remains synchronized.

## Output device behavior

The Dashboard can switch the native output device live. If the requested device cannot be opened, MicYou attempts to restore the previous device and reports the failure without persisting the broken selection.

## Startup behavior

For a fresh installation, `ServerPrefs.mode` defaults to `web`.

Existing users are not migrated forcibly: an existing `server.json` keeps its saved connection mode.

When the saved mode is Web, the native Core starts the Web microphone server automatically after startup. The localhost Dashboard starts with the Core regardless of the selected transport mode.

## Tray behavior

The tray now exposes **Open Dashboard** and double-clicking the tray icon also opens:

```text
http://127.0.0.1:19527/
```

The old Tauri window remains available during the transition, so existing functionality is not removed prematurely.

## Browser/PWA assets

The phone server exposes a Web App Manifest and a service worker asset. The service worker deliberately avoids caching navigation, pairing pages, API calls, or WebSocket traffic; only static assets are eligible for caching so authentication state is never captured in an offline cache.

The core workflow does not depend on installation: opening the paired HTTPS page in the browser is sufficient. Browsers that offer **Add to Home Screen** can still be used for an app-like launch experience.

The self-signed HTTPS certificate remains the main browser-onboarding compromise for a purely local solution. It is required because microphone capture is a secure-context browser API when accessed from another device on the LAN.

## Validation

A focused workflow is included at:

```text
.github/workflows/web-first-ci.yml
```

It performs:

- frontend typecheck/build
- `cargo fmt --check`
- `cargo check -p micyou-app --all-targets`
- Web server unit tests

The fork's GitHub Actions are active; this workflow runs on the Web-first branch and on pull requests targeting `master`.

## Files introduced or materially changed

```text
tauri-app/src-tauri/src/dashboard_server.rs
tauri-app/src-tauri/src/web_server.rs
tauri-app/src-tauri/src/audio_output.rs
tauri-app/src-tauri/src/app_config.rs
tauri-app/src-tauri/src/tray.rs
tauri-app/src-tauri/src/lib.rs
tauri-app/src-tauri/src/commands/audio.rs
tauri-app/src-tauri/resources/dashboard_local.html
tauri-app/src-tauri/resources/manifest.webmanifest
tauri-app/src-tauri/resources/service-worker.js
.github/workflows/web-first-ci.yml
```

## Migration policy

The Android client, existing Tauri UI, CLI and TUI remain in the repository. Once the browser workflow has passed platform builds, the desktop window can be made optional and the installed product can be presented primarily as **MicYou Core + Tray + Web Dashboard**.
