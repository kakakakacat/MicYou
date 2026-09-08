/*
 * MicYou — Turns your Android device into a high-quality PC microphone.
 * Copyright (C) 2026 LanRhyme <https://github.com/LanRhyme/MicYou>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version, with the MicYou Plugin Exception.
 */

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, RwLock};

/// The legacy server pipeline intentionally skips DSP for Web transport.
/// Keep that behavior isolated and apply Web DSP at the final output layer so
/// Wi-Fi/USB/Android paths are not double-processed.
static WEB_DSP_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set_web_dsp_active(active: bool) {
    WEB_DSP_ACTIVE.store(active, Ordering::Release);
}

pub fn web_dsp_active() -> bool {
    WEB_DSP_ACTIVE.load(Ordering::Acquire)
}

fn find_web_dsp_resources() -> Option<std::path::PathBuf> {
    const MARKERS: [&str; 2] = ["purevox6.onnx", "aec7_ep0185.onnx"];
    let mut candidates = Vec::new();
    if let Some(executable_dir) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
    {
        candidates.push(executable_dir.clone());
        candidates.push(executable_dir.join("resources"));
        if let Some(prefix) = executable_dir.parent() {
            candidates.push(prefix.join("lib").join("micyou").join("resources"));
        }
    }
    candidates.push(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"));
    candidates
        .into_iter()
        .find(|directory| MARKERS.iter().any(|model| directory.join(model).exists()))
}

enum AudioOutputCommand {
    Open(Option<String>, usize, Sender<bool>),
    Reopen(Option<String>, usize, Sender<bool>),
    Push(Vec<f32>, usize),
    PushSound(Vec<f32>, f32),
    SetMonitoring(bool),
    SetMuted(bool),
    UpdateWebDsp(micyou_audio::dsp::AudioDspSettings),
    Queued(Sender<usize>),
    Shutdown,
}

pub struct AudioOutputHandle {
    tx: Sender<AudioOutputCommand>,
}

impl Default for AudioOutputHandle {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel::<AudioOutputCommand>();
        let initial_settings = Arc::new(RwLock::new(web_safe_dsp_settings(
            crate::app_config::load_dsp_settings(),
        )));
        std::thread::spawn(move || {
            let mut manager = micyou_audio::AudioOutputManager::new();
            let mut muted = false;
            let web_settings = initial_settings;
            let mut web_dsp: Option<micyou_audio::dsp::DspProcessor> = None;

            loop {
                match rx.recv() {
                    Ok(AudioOutputCommand::Open(device, buffer_ms, reply)) => {
                        let ok = if manager.is_open() {
                            true
                        } else {
                            match manager.start(device, buffer_ms) {
                                Ok(()) => {
                                    log::info!("[Audio] Output device opened");
                                    true
                                }
                                Err(e) => {
                                    eprintln!("[Audio] Failed to open output device: {e}");
                                    false
                                }
                            }
                        };
                        let _ = reply.send(ok);
                    }
                    Ok(AudioOutputCommand::Reopen(device, buffer_ms, reply)) => {
                        manager.close();
                        let ok = match manager.start(device, buffer_ms) {
                            Ok(()) => {
                                log::info!("[Audio] Output device switched");
                                true
                            }
                            Err(e) => {
                                eprintln!("[Audio] Failed to switch output device: {e}");
                                false
                            }
                        };
                        let _ = reply.send(ok);
                    }
                    Ok(AudioOutputCommand::Push(mut data, channels)) => {
                        if web_dsp_active() && !data.is_empty() {
                            if web_dsp.is_none() {
                                web_dsp = Some(micyou_audio::dsp::DspProcessor::new(
                                    web_settings.clone(),
                                    find_web_dsp_resources(),
                                ));
                                log::info!("[Audio] Web DSP pipeline initialized");
                            }
                            if let Some(processor) = web_dsp.as_mut() {
                                let queued_ms = if channels > 0 {
                                    (manager.queued_samples() as f64 / channels as f64) / 48.0
                                } else {
                                    0.0
                                };
                                let _ = processor.process(&mut data, channels.max(1), queued_ms);
                            }
                        } else if web_dsp.is_some() {
                            web_dsp = None;
                        }

                        if muted {
                            data.fill(0.0);
                        }
                        manager.push_audio_data(&data, channels);
                    }
                    Ok(AudioOutputCommand::PushSound(mut samples, gain)) => {
                        if muted {
                            samples.fill(0.0);
                        }
                        manager.push_sound_effect(samples, gain);
                    }
                    Ok(AudioOutputCommand::SetMonitoring(enabled)) => {
                        manager.set_monitoring(enabled);
                    }
                    Ok(AudioOutputCommand::SetMuted(value)) => {
                        muted = value;
                    }
                    Ok(AudioOutputCommand::UpdateWebDsp(settings)) => {
                        if let Ok(mut current) = web_settings.write() {
                            *current = web_safe_dsp_settings(settings);
                        }
                    }
                    Ok(AudioOutputCommand::Queued(reply)) => {
                        let _ = reply.send(manager.queued_samples());
                    }
                    Ok(AudioOutputCommand::Shutdown) | Err(_) => {
                        manager.close();
                        break;
                    }
                }
            }
        });
        Self { tx }
    }
}

fn web_safe_dsp_settings(
    mut settings: micyou_audio::dsp::AudioDspSettings,
) -> micyou_audio::dsp::AudioDspSettings {
    // AEC requires the desktop far-end loopback reference which is owned by
    // the native server DSP path. The Web output-layer DSP intentionally keeps
    // AEC disabled while preserving NS/AGC/EQ/VAD/amplification stages.
    settings.aec_enabled = false;
    settings.normalize();
    settings
}

impl AudioOutputHandle {
    pub fn spawn() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn ensure_open(&self, device: Option<String>, buffer_ms: usize) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(AudioOutputCommand::Open(device, buffer_ms, reply_tx))
            .is_err()
        {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    pub fn reopen(&self, device: Option<String>, buffer_ms: usize) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(AudioOutputCommand::Reopen(device, buffer_ms, reply_tx))
            .is_err()
        {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    pub fn push(&self, data: Vec<f32>, channels: usize) {
        let _ = self.tx.send(AudioOutputCommand::Push(data, channels));
    }

    pub fn push_sound(&self, samples: Vec<f32>, gain: f32) {
        let _ = self.tx.send(AudioOutputCommand::PushSound(samples, gain));
    }

    pub fn set_monitoring(&self, enabled: bool) {
        let _ = self.tx.send(AudioOutputCommand::SetMonitoring(enabled));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.tx.send(AudioOutputCommand::SetMuted(muted));
    }

    pub fn update_web_dsp_settings(&self, settings: micyou_audio::dsp::AudioDspSettings) {
        let _ = self.tx.send(AudioOutputCommand::UpdateWebDsp(settings));
    }

    pub fn queued_samples(&self) -> usize {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(AudioOutputCommand::Queued(reply_tx)).is_err() {
            return 0;
        }
        reply_rx.recv().unwrap_or(0)
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(AudioOutputCommand::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_safe_settings_force_aec_off() {
        let mut settings = micyou_audio::dsp::AudioDspSettings::default();
        settings.aec_enabled = true;
        assert!(!web_safe_dsp_settings(settings).aec_enabled);
    }
}
