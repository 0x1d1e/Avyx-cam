use crate::config::AppConfig;
use crate::devices::{
    ensure_virtual_mic, get_device_orientation, optimal_camera_rotation, video_exists, MIC_SINK,
    VIDEO_DEV,
};
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const PID_FILE: &str = "/tmp/avyx-cam.pid";
pub const LOG_FILE: &str = "/tmp/avyx-cam.log";
pub const LEGACY_PID_FILE: &str = "/tmp/phone-cam.pid";
pub const LEGACY_LOG_FILE: &str = "/tmp/phone-cam.log";

pub struct StreamHandle {
    pub child: Child,
    #[allow(dead_code)]
    pub serial: String,
    pub started_at: Instant,
    pub resolution: String,
    pub quick_retried: bool,
}

pub fn build_orientation(rotation: &str, flip: bool) -> String {
    if flip {
        format!("flip{rotation}")
    } else {
        rotation.to_string()
    }
}

pub fn next_fallback_resolution(res: &str) -> Option<&'static str> {
    match res {
        "2560x1440" => Some("1920x1080"),
        "1920x1080" => Some("1280x720"),
        "1280x720" => Some("640x480"),
        _ => None,
    }
}

pub fn read_external_pid() -> Option<u32> {
    for pid_f in [PID_FILE, LEGACY_PID_FILE] {
        if let Ok(txt) = fs::read_to_string(pid_f) {
            if let Ok(pid) = txt.trim().parse::<u32>() {
                if PathBuf::from(format!("/proc/{pid}")).exists() {
                    return Some(pid);
                }
            }
            let _ = fs::remove_file(pid_f);
        }
    }
    let out = Command::new("pgrep")
        .args(["-f", &format!("scrcpy.*--v4l2-sink={VIDEO_DEV}")])
        .output()
        .ok()?;
    if out.status.success() {
        let first = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()?
            .trim()
            .parse::<u32>()
            .ok()?;
        return Some(first);
    }
    None
}

pub fn stop_all_streams() {
    if let Some(pid) = read_external_pid() {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    let _ = Command::new("pkill")
        .args(["-f", &format!("scrcpy.*--v4l2-sink={VIDEO_DEV}")])
        .status();
    let _ = fs::remove_file(PID_FILE);
    let _ = fs::remove_file(LEGACY_PID_FILE);
}

pub fn start_scrcpy(
    serial: &str,
    cfg: &AppConfig,
    override_res: Option<&str>,
    quick_retried: bool,
) -> Result<StreamHandle, String> {
    stop_all_streams();
    ensure_virtual_mic()?;
    if !video_exists() {
        return Err(format!("{VIDEO_DEV} is missing. Enable /dev/video0 first."));
    }

    let res = override_res.unwrap_or(&cfg.resolution).to_string();
    let mut fps = cfg.fps.clone();
    if cfg.camera_id != "0" && fps.parse::<u32>().unwrap_or(30) >= 120 {
        fps = "30".to_string();
    }
    let mut rot = cfg.rotation.clone();
    if cfg.auto_rotate {
        if let Some(deg) = get_device_orientation(serial) {
            rot = optimal_camera_rotation(&cfg.camera_id, deg).to_string();
        }
    }
    let orient = build_orientation(&rot, cfg.flip);

    let mut args = vec![
        "-s".to_string(),
        serial.to_string(),
        "--video-source=camera".to_string(),
        format!("--camera-id={}", cfg.camera_id),
        format!("--camera-size={res}"),
        format!("--camera-fps={fps}"),
        format!("--capture-orientation={orient}"),
        format!("--v4l2-sink={VIDEO_DEV}"),
    ];

    if fps.parse::<u32>().unwrap_or(30) >= 120 {
        args.push("--camera-high-speed".to_string());
    }
    if cfg.zoom > 1.05 {
        args.push(format!("--camera-zoom={:.1}", cfg.zoom));
    }
    if cfg.torch && cfg.camera_id == "0" {
        args.push("--camera-torch".to_string());
    }
    if cfg.bitrate != "auto" {
        args.push(format!("--video-bit-rate={}", cfg.bitrate));
    }
    if cfg.v4l2_buffer != "0" {
        args.push(format!("--v4l2-buffer={}", cfg.v4l2_buffer));
    }

    match cfg.mic_mode.as_str() {
        "off" => args.push("--no-audio".to_string()),
        "raw" => args.push("--audio-source=mic".to_string()),
        "camcorder" => args.push("--audio-source=mic-camcorder".to_string()),
        _ => args.push("--audio-source=mic-voice-recognition".to_string()),
    }

    if !cfg.preview {
        args.push("--no-window".to_string());
    }

    let log_out = File::create(LOG_FILE).map_err(|e| format!("Log file error: {e}"))?;
    let log_err = log_out
        .try_clone()
        .map_err(|e| format!("Log clone error: {e}"))?;

    let child = Command::new("scrcpy")
        .args(&args)
        .env("PIPEWIRE_NODE", MIC_SINK)
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| format!("Failed to start scrcpy: {e}"))?;

    let _ = fs::write(PID_FILE, child.id().to_string());

    Ok(StreamHandle {
        child,
        serial: serial.to_string(),
        started_at: Instant::now(),
        resolution: res,
        quick_retried,
    })
}

pub fn read_log() -> String {
    fs::read_to_string(LOG_FILE)
        .or_else(|_| fs::read_to_string(LEGACY_LOG_FILE))
        .unwrap_or_default()
}

pub fn classify_crash(log: &str) -> Option<&'static str> {
    let lower = log.to_lowercase();
    if lower.contains("camera") && lower.contains("in use") || lower.contains("camera_in_use") {
        Some("Phone camera is in use by another app on the phone.")
    } else if lower.contains("camera_disabled") || lower.contains("disabled by policy") {
        Some("Phone camera access is blocked by privacy toggle.")
    } else if lower.contains("unauthorized") {
        Some("Tap 'Allow USB debugging' on your phone.")
    } else if lower.contains("device") && lower.contains("not found") {
        Some("Phone disconnected.")
    } else {
        None
    }
}

pub fn ran_long_enough(started_at: Instant) -> bool {
    started_at.elapsed() >= Duration::from_secs(10)
}
