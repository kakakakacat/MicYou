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

use std::sync::mpsc::{self, Sender};
use std::sync::Arc;

/// Persistent audio output device thread.
///
/// `cpal::Stream` is deliberately `!Send + !Sync`, so the
/// `AudioOutputManager` can never live inside the shared `ServerState`.
/// Instead a dedicated thread owns it for the whole process lifetime and
/// receives commands over an mpsc channel. The device is opened at app
/// startup (GUI) or on the first server start (CLI/TUI) and only closed when
/// the process exits — server stop and phone connect/disconnect never tear it
/// down.
enum AudioOutputCommand {
    Open(Option<String>, usize, Sender<bool>),
    Push(Vec<f32>, usize),
    PushSound(Vec<f32>, f32),
    SetMonitoring(bool),
    SetMuted(bool),
    Queued(Sender<usize>),
    Shutdown,
}

pub struct AudioOutputHandle {
    tx: Sender<AudioOutputCommand>,
}

impl Default for AudioOutputHandle {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel::<AudioOutputCommand>();
        std::thread::spawn(move || {
            let mut manager = micyou_audio::AudioOutputManager::new();
            let mut muted = false;
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
                                    eprintln!("[Audio] Failed to open output device: {}", e);
                                    false
                                }
                            }
                        };
                        let _ = reply.send(ok);
                    }
                    Ok(AudioOutputCommand::Push(mut data, channels)) => {
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

impl AudioOutputHandle {
    /// Spawn the persistent device thread and return a shared handle.
    pub fn spawn() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Blocking open of the output device. Idempotent: returns immediately if
    /// the stream is already open.
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

    /// Push decoded PCM into the output ring buffer. The channel is unbounded,
    /// so this never blocks or drops audio while the device thread lives.
    pub fn push(&self, data: Vec<f32>, channels: usize) {
        let _ = self.tx.send(AudioOutputCommand::Push(data, channels));
    }

    /// Queue a plugin sound effect; mixed into the virtual mic output stream.
    pub fn push_sound(&self, samples: Vec<f32>, gain: f32) {
        let _ = self.tx.send(AudioOutputCommand::PushSound(samples, gain));
    }

    pub fn set_monitoring(&self, enabled: bool) {
        let _ = self.tx.send(AudioOutputCommand::SetMonitoring(enabled));
    }

    /// Mute is applied at the final virtual-microphone output layer so it works
    /// for Android, Web and future transports consistently.
    pub fn set_muted(&self, muted: bool) {
        let _ = self.tx.send(AudioOutputCommand::SetMuted(muted));
    }

    /// Samples currently queued in the output ring buffer.
    pub fn queued_samples(&self) -> usize {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(AudioOutputCommand::Queued(reply_tx)).is_err() {
            return 0;
        }
        reply_rx.recv().unwrap_or(0)
    }

    /// Close the output stream and stop the device thread. Only called when
    /// the process is exiting.
    pub fn shutdown(&self) {
        let _ = self.tx.send(AudioOutputCommand::Shutdown);
    }
}
