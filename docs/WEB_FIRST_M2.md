# MicYou Web-first M2

M2 introduces a dedicated local control plane for the PC dashboard while keeping the phone microphone transport isolated on the LAN-facing HTTPS server.

## Runtime layout

- Phone microphone: `https://<pc-lan-ip>:8443/`
- Audio WebSocket: `wss://<pc-lan-ip>:8443/ws`
- PC dashboard: `http://127.0.0.1:19527/`
- Dashboard control APIs: `http://127.0.0.1:19527/api/*`

The dashboard server binds **only to 127.0.0.1**. Control endpoints are therefore not exposed to other devices on the LAN.

## M2 APIs

### Status

`GET /api/status`

Returns:

- mute state
- monitoring state
- current web client count
- queued audio samples
- sample rate / bitrate
- network RTT
- jitter
- packet loss
- current DSP settings

### Mute

`POST /api/control/mute`

```json
{ "enabled": true }
```

### Monitoring

`POST /api/control/monitoring`

```json
{ "enabled": true }
```

### DSP patch

`POST /api/control/dsp`

The payload is merged into the current `AudioDspSettings`, normalized, persisted to `settings.json`, and applied to the shared in-memory state.

Example:

```json
{ "aecEnabled": true }
```

The exact serialized field names come from `AudioDspSettings` and are discovered by the dashboard before enabling controls.

### Audio devices

`GET /api/audio-devices`

Returns available output devices and current shared server preferences.

### Output device

`POST /api/control/output-device`

```json
{ "outputDevice": "Device Name" }
```

The value is persisted to `server.json`. M2 marks this as requiring a device/server reopen because the persistent output stream is deliberately not torn down during normal server restarts.

## Dashboard UI

The localhost dashboard now provides live values for:

- browser client count
- RTT
- jitter
- packet loss
- sample rate
- bitrate
- queued samples
- mute state

And controls for:

- mute
- monitoring
- noise suppression when present in the current DSP schema
- AEC when present/supported
- AGC when present in the current DSP schema
- output device preference

## Security model

The main design change in M2 is the split control plane:

```text
Phone / LAN
    |
    | HTTPS + WSS audio only
    v
0.0.0.0:8443

PC browser only
    |
    | HTTP control API
    v
127.0.0.1:19527
```

This is intentionally preferred over exposing control routes on port 8443 and attempting to secure them only with browser Origin/Host checks.

## Next: M3

- QR pairing token/session identity
- Web audio telemetry that is meaningful for browser PCM transport
- output-level meter exposed directly to the dashboard
- tray action: Open Dashboard
- automatically start web audio mode based on saved preferences
- browser certificate onboarding improvements
- optional daemon-only desktop mode after dashboard feature parity
