use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceHistory {
    pub serial: String,
    pub model: String,
    pub address: String,
    pub is_wifi: bool,
    pub last_seen: String,
    #[serde(default = "default_camera_id")]
    pub camera_id: String,
    #[serde(default = "default_resolution")]
    pub resolution: String,
    #[serde(default = "default_fps")]
    pub fps: String,
}

fn default_camera_id() -> String {
    "0".to_string()
}
fn default_resolution() -> String {
    "1280x720".to_string()
}
fn default_fps() -> String {
    "30".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub camera_id: String,
    pub resolution: String,
    pub fps: String,
    pub rotation: String,
    pub auto_rotate: bool,
    pub flip: bool,
    pub zoom: f64,
    pub torch: bool,
    pub mic_mode: String, // "off", "voice", "raw", "camcorder"
    pub bitrate: String,  // "auto", "8M", "16M", "24M"
    pub v4l2_buffer: String, // "0", "30", "50"
    pub preview: bool,
    pub auto_reconnect: bool,
    pub auto_start_usb: bool,
    pub wireless_ip: String,
    pub history: Vec<DeviceHistory>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            camera_id: "0".to_string(),
            resolution: "1280x720".to_string(),
            fps: "30".to_string(),
            rotation: "0".to_string(),
            auto_rotate: true,
            flip: false,
            zoom: 1.0,
            torch: false,
            mic_mode: "voice".to_string(),
            bitrate: "auto".to_string(),
            v4l2_buffer: "0".to_string(),
            preview: true,
            auto_reconnect: true,
            auto_start_usb: false,
            wireless_ip: String::new(),
            history: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn record_device(
        &mut self,
        serial: &str,
        model: &str,
        is_wifi: bool,
        address: Option<&str>,
    ) {
        let now = Command::new("date")
            .args(["+%Y-%m-%d %H:%M"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "Recently".to_string());

        let addr = address.unwrap_or(if is_wifi { serial } else { "" }).to_string();

        if let Some(existing) = self
            .history
            .iter_mut()
            .find(|d| d.serial == serial || (!addr.is_empty() && d.address == addr))
        {
            if !model.is_empty() && model != serial {
                existing.model = model.to_string();
            }
            if !addr.is_empty() {
                existing.address = addr;
            }
            existing.is_wifi = is_wifi;
            existing.last_seen = now;
            existing.camera_id = self.camera_id.clone();
            existing.resolution = self.resolution.clone();
            existing.fps = self.fps.clone();
        } else {
            self.history.insert(
                0,
                DeviceHistory {
                    serial: serial.to_string(),
                    model: if model.is_empty() { serial.to_string() } else { model.to_string() },
                    address: addr,
                    is_wifi,
                    last_seen: now,
                    camera_id: self.camera_id.clone(),
                    resolution: self.resolution.clone(),
                    fps: self.fps.clone(),
                },
            );
            if self.history.len() > 10 {
                self.history.truncate(10);
            }
        }
    }

    pub fn forget_device(&mut self, serial: &str) {
        self.history.retain(|d| d.serial != serial && d.address != serial);
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("avyx-cam").join("config.json")
}

pub fn legacy_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("phone-cam").join("config.json")
}

pub fn load_config() -> AppConfig {
    let path = config_path();
    if let Ok(s) = fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str(&s) {
            return cfg;
        }
    }
    // Fallback/migrate from legacy phone-cam config if present
    let legacy = legacy_config_path();
    if let Ok(s) = fs::read_to_string(&legacy) {
        if let Ok(cfg) = serde_json::from_str::<AppConfig>(&s) {
            save_config(&cfg);
            return cfg;
        }
    }
    AppConfig::default()
}

pub fn save_config(cfg: &AppConfig) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(path, json);
    }
}
