use crate::config::AppConfig;
use crate::devices::{
    ensure_virtual_mic, get_device_orientation, optimal_camera_rotation, video_exists, MIC_SINK,
    VIDEO_DEV,
};
use std::fs::File;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct StreamHandle {
    pub child: Child,
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

impl Drop for StreamHandle {
    fn drop(&mut self) {
        crate::process::terminate(&mut self.child);
    }
}

pub fn start_scrcpy(
    serial: &str,
    cfg: &AppConfig,
    override_res: Option<&str>,
    quick_retried: bool,
) -> Result<StreamHandle, String> {
    if crate::process::cancelled() {
        return Err("Operation cancelled".into());
    }
    if cfg.mic_mode != "off" {
        ensure_virtual_mic()?;
    }
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

    let log_out = File::create(crate::session::runtime_dir()?.join("stream.log"))
        .map_err(|e| format!("Log file error: {e}"))?;
    let log_err = log_out
        .try_clone()
        .map_err(|e| format!("Log clone error: {e}"))?;

    if crate::process::cancelled() {
        return Err("Operation cancelled".into());
    }
    let mut command = Command::new("scrcpy");
    command
        .args(&args)
        .env("PIPEWIRE_NODE", MIC_SINK)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err));
    crate::process::prepare(&mut command);
    let child = command
        .spawn()
        .map_err(|e| format!("Failed to start scrcpy: {e}"))?;

    Ok(StreamHandle {
        child,
        serial: serial.to_string(),
        started_at: Instant::now(),
        resolution: res,
        quick_retried,
    })
}

pub fn read_log() -> String {
    use std::io::Read;
    crate::session::runtime_dir()
        .ok()
        .and_then(|dir| File::open(dir.join("stream.log")).ok())
        .map(|file| {
            let mut text = String::new();
            let _ = file.take(262_144).read_to_string(&mut text);
            text
        })
        .unwrap_or_default()
}

pub fn classify_crash(log: &str) -> Option<&'static str> {
    let lower = log.to_lowercase();
    if lower.contains("camera") && lower.contains("in use") || lower.contains("camera_in_use") {
        Some("Phone camera is in use by another app on the phone.")
    } else if lower.contains("camera_disabled") || lower.contains("disabled by policy") {
        Some("Phone camera access is blocked by privacy toggle.")
    } else if lower.contains("permission denied") {
        Some("Phone camera permission denied. Check camera permissions on the phone.")
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
