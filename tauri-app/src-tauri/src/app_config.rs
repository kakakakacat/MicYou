/*
 * MicYou — Turns your Android device into a high-quality PC microphone.
 * Copyright (C) 2026 LanRhyme <https://github.com/LanRhyme/MicYou>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version, with the MicYou Plugin Exception.
 */

use micyou_audio::dsp::AudioDspSettings;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("micyou");
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(xdg).join("micyou");
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("micyou")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn ui_prefs_path() -> PathBuf {
    config_dir().join("ui.json")
}

pub fn theme_path() -> PathBuf {
    config_dir().join("theme.json")
}

pub fn server_prefs_path() -> PathBuf {
    config_dir().join("server.json")
}

pub fn load_dsp_settings() -> AudioDspSettings {
    fs::read_to_string(settings_path())
        .ok()
        .and_then(|text| serde_json::from_str::<AudioDspSettings>(&text).ok())
        .map(|mut settings| {
            settings.normalize();
            settings
        })
        .unwrap_or_default()
}

pub fn save_dsp_settings(settings: &AudioDspSettings) -> Result<(), String> {
    let dir = config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create config dir failed: {e}"))?;
    let mut normalized = settings.clone();
    normalized.normalize();
    let json = serde_json::to_string_pretty(&normalized)
        .map_err(|e| format!("serialize settings failed: {e}"))?;
    fs::write(settings_path(), json).map_err(|e| format!("write settings.json failed: {e}"))
}

pub fn settings_json() -> serde_json::Value {
    fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::to_value(AudioDspSettings::default()).unwrap_or_default())
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct UiPrefs {
    pub language: String,
    pub theme_color: String,
}

pub fn load_ui_prefs() -> UiPrefs {
    fs::read_to_string(ui_prefs_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_ui_prefs(prefs: &UiPrefs) -> Result<(), String> {
    let dir = config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create config dir failed: {e}"))?;
    let json = serde_json::to_string_pretty(prefs)
        .map_err(|e| format!("serialize ui prefs failed: {e}"))?;
    fs::write(ui_prefs_path(), json).map_err(|e| format!("write ui.json failed: {e}"))
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ThemeColors {
    pub primary: String,
    pub secondary: String,
    pub tertiary: String,
    pub surface: String,
    pub surface_variant: String,
    pub on_surface: String,
    pub error: String,
}

pub fn load_theme_colors() -> ThemeColors {
    fs::read_to_string(theme_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_theme_colors(colors: &ThemeColors) -> Result<(), String> {
    let dir = config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create config dir failed: {e}"))?;
    let json =
        serde_json::to_string_pretty(colors).map_err(|e| format!("serialize theme failed: {e}"))?;
    fs::write(theme_path(), json).map_err(|e| format!("write theme.json failed: {e}"))
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerPrefs {
    pub port: u16,
    pub web_port: u16,
    pub mode: String,
    pub bind_address: String,
    pub auto_bind: bool,
    pub output_device: String,
}

impl Default for ServerPrefs {
    fn default() -> Self {
        Self {
            port: 8554,
            web_port: 8443,
            // Web-first is the default for fresh installs. Existing server.json
            // files keep their chosen Wi-Fi/USB mode unchanged.
            mode: "web".to_string(),
            bind_address: "0.0.0.0".to_string(),
            auto_bind: true,
            output_device: String::new(),
        }
    }
}

pub fn load_server_prefs() -> ServerPrefs {
    fs::read_to_string(server_prefs_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_server_prefs(prefs: &ServerPrefs) -> Result<(), String> {
    let dir = config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create config dir failed: {e}"))?;
    let json = serde_json::to_string_pretty(prefs)
        .map_err(|e| format!("serialize server prefs failed: {e}"))?;
    fs::write(server_prefs_path(), json).map_err(|e| format!("write server.json failed: {e}"))
}
