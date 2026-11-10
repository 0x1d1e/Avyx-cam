use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

pub const VIDEO_DEV: &str = "/dev/video0";
pub const MIC_SINK: &str = "PhoneMic_sink";
pub const MIC_SOURCE: &str = "PhoneMic";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String,
    pub model: String,
    pub is_wifi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    pub id: String,
    pub facing: String,
    pub sizes: Vec<String>,
    pub has_high_speed: bool,
}

pub fn run_out(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        })
    }
}

pub fn video_exists() -> bool {
    Path::new(VIDEO_DEV).exists()
}

pub fn boot_persistence_installed() -> bool {
    (Path::new("/etc/modules-load.d/avyx-cam.conf").exists()
        && Path::new("/etc/modprobe.d/avyx-cam.conf").exists())
        || (Path::new("/etc/modules-load.d/phone-cam.conf").exists()
            && Path::new("/etc/modprobe.d/phone-cam.conf").exists())
}

pub fn install_boot_persistence() -> Result<(), String> {
    let script = concat!(
        "echo 'v4l2loopback' > /etc/modules-load.d/avyx-cam.conf && ",
        "echo 'options v4l2loopback devices=1 video_nr=0 card_label=\"AvyxCam\" exclusive_caps=1' > /etc/modprobe.d/avyx-cam.conf && ",
        "rm -f /etc/modules-load.d/phone-cam.conf /etc/modprobe.d/phone-cam.conf 2>/dev/null || true; ",
        "modprobe -r v4l2loopback 2>/dev/null || true; ",
        "modprobe v4l2loopback devices=1 video_nr=0 card_label=AvyxCam exclusive_caps=1"
    );
    run_out("pkexec", &["sh", "-c", script]).map(|_| ())
}

pub fn load_or_reload_v4l2(reload: bool) -> Result<(), String> {
    let cmd = if reload {
        "modprobe -r v4l2loopback 2>/dev/null || true; modprobe v4l2loopback devices=1 video_nr=0 card_label=AvyxCam exclusive_caps=1"
    } else {
        "modprobe v4l2loopback devices=1 video_nr=0 card_label=AvyxCam exclusive_caps=1"
    };
    run_out("pkexec", &["sh", "-c", cmd]).map(|_| ())
}

pub fn video_busy_apps() -> Vec<String> {
    if !video_exists() {
        return Vec::new();
    }
    let Ok(out) = Command::new("fuser").arg(VIDEO_DEV).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut apps = Vec::new();
    for pid in text.split_whitespace() {
        let comm_path = format!("/proc/{pid}/comm");
        if let Ok(comm) = fs::read_to_string(comm_path) {
            let name = comm.trim().to_string();
            if !name.is_empty() && name != "scrcpy" && !apps.contains(&name) {
                apps.push(name);
            }
        }
    }
    apps.sort();
    apps
}

pub fn v4l2_current_size() -> Option<String> {
    let out = run_out("v4l2-ctl", &["-d", VIDEO_DEV, "--get-fmt-video"]).ok()?;
    for line in out.lines() {
        if line.contains("Width/Height") {
            let rhs = line.split(':').nth(1)?.trim();
            let parts: Vec<&str> = rhs.split('/').collect();
            if parts.len() == 2 {
                return Some(format!("{}x{}", parts[0].trim(), parts[1].trim()));
            }
        }
    }
    None
}

pub fn mic_ready() -> bool {
    let Ok(sources) = run_out("pactl", &["list", "sources", "short"]) else {
        return false;
    };
    let Ok(sinks) = run_out("pactl", &["list", "sinks", "short"]) else {
        return false;
    };
    sources.contains(&format!("\t{MIC_SOURCE}\t")) && sinks.contains(&format!("\t{MIC_SINK}\t"))
}

pub fn ensure_virtual_mic() -> Result<(), String> {
    let sources = run_out("pactl", &["list", "sources", "short"])?;
    let sinks = run_out("pactl", &["list", "sinks", "short"])?;

    if !sinks.contains(&format!("\t{MIC_SINK}\t")) {
        run_out(
            "pactl",
            &[
                "load-module",
                "module-null-sink",
                &format!("sink_name={MIC_SINK}"),
                &format!("sink_properties=device.description={MIC_SINK}"),
            ],
        )?;
    }

    if !sources.contains(&format!("\t{MIC_SOURCE}\t")) {
        run_out(
            "pactl",
            &[
                "load-module",
                "module-remap-source",
                &format!("master={MIC_SINK}.monitor"),
                &format!("source_name={MIC_SOURCE}"),
                &format!("source_properties=device.description={MIC_SOURCE}"),
            ],
        )?;
    }

    Ok(())
}

pub fn list_adb_devices() -> Vec<AdbDevice> {
    let Ok(out) = run_out("adb", &["devices", "-l"]) else {
        return Vec::new();
    };
    let mut list = Vec::new();
    for line in out.lines().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(serial) = parts.next() else { continue };
        let Some(state) = parts.next() else { continue };
        let is_wifi = serial.contains(':');
        let mut model = serial.to_string();
        for tok in parts {
            if let Some(m) = tok.strip_prefix("model:") {
                model = m.replace('_', " ");
                break;
            }
        }
        list.push(AdbDevice {
            serial: serial.to_string(),
            state: state.to_string(),
            model,
            is_wifi,
        });
    }
    list
}

pub fn get_phone_wifi_ip(serial: &str) -> Option<String> {
    let out = run_out("adb", &["-s", serial, "shell", "ip", "route"]).ok()?;
    for line in out.lines() {
        if line.contains("wlan") {
            let toks: Vec<&str> = line.split_whitespace().collect();
            for win in toks.windows(2) {
                if win[0] == "src" {
                    return Some(win[1].to_string());
                }
            }
        }
    }
    None
}

pub fn switch_usb_to_wifi(usb_serial: &str, port: u16) -> Result<String, String> {
    let ip = get_phone_wifi_ip(usb_serial)
        .ok_or_else(|| "Phone Wi-Fi IP not found. Connect phone to the same Wi-Fi.".to_string())?;
    let port_str = port.to_string();
    run_out("adb", &["-s", usb_serial, "tcpip", &port_str])?;
    thread::sleep(Duration::from_millis(1200));
    let endpoint = format!("{ip}:{port}");
    let out = run_out("adb", &["connect", &endpoint])?;
    if out.to_lowercase().contains("connected") {
        Ok(endpoint)
    } else {
        Err(out.trim().to_string())
    }
}

pub fn connect_wifi(endpoint: &str) -> Result<String, String> {
    let ep = if endpoint.contains(':') {
        endpoint.trim().to_string()
    } else {
        format!("{}:5555", endpoint.trim())
    };
    let out = run_out("adb", &["connect", &ep])?;
    if out.to_lowercase().contains("connected") {
        Ok(ep)
    } else {
        Err(out.trim().to_string())
    }
}

pub fn disconnect_wifi(endpoint: &str) -> Result<(), String> {
    run_out("adb", &["disconnect", endpoint]).map(|_| ())
}

pub fn probe_cameras(serial: &str) -> Vec<CameraInfo> {
    let Ok(out) = Command::new("scrcpy")
        .args(["-s", serial, "--list-camera-sizes"])
        .output()
    else {
        return Vec::new();
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut cams: Vec<CameraInfo> = Vec::new();
    let mut in_high_speed = false;

    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("--camera-id=") {
            in_high_speed = false;
            let id = rest.split_whitespace().next().unwrap_or("0").to_string();
            let facing = if rest.contains("front") {
                "Front".to_string()
            } else if rest.contains("back") {
                "Back".to_string()
            } else {
                format!("Camera {id}")
            };
            cams.push(CameraInfo {
                id,
                facing,
                sizes: Vec::new(),
                has_high_speed: false,
            });
        } else if line.starts_with("High speed capture") {
            in_high_speed = true;
            if let Some(last) = cams.last_mut() {
                last.has_high_speed = true;
            }
        } else if !in_high_speed && line.starts_with("- ") {
            if let Some(last) = cams.last_mut() {
                let sz = line.trim_start_matches("- ").trim().to_string();
                if sz.contains('x') && !last.sizes.contains(&sz) {
                    last.sizes.push(sz);
                }
            }
        }
    }
    cams
}

/// Query the device's physical/surface orientation in degrees: 0, 90, 180, 270.
/// 0: Portrait (normal upright)
/// 90: Landscape (turned counter-clockwise, top to left)
/// 180: Reverse portrait (upside down)
/// 270: Landscape (turned clockwise, top to right)
pub fn get_device_orientation(serial: &str) -> Option<u32> {
    // 1. Try dumpsys input SurfaceOrientation
    if let Ok(out) = run_out("adb", &["-s", serial, "shell", "dumpsys", "input"]) {
        for line in out.lines() {
            if line.contains("SurfaceOrientation") {
                if let Some(val) = line
                    .split(':')
                    .nth(1)
                    .and_then(|v| v.trim().parse::<u32>().ok())
                {
                    return match val {
                        1 => Some(90),
                        2 => Some(180),
                        3 => Some(270),
                        _ => Some(0),
                    };
                }
            }
        }
    }
    // 2. Try dumpsys display mCurrentOrientation
    if let Ok(out) = run_out("adb", &["-s", serial, "shell", "dumpsys", "display"]) {
        for line in out.lines() {
            if line.contains("mCurrentOrientation=") {
                if let Some(val) = line
                    .split("mCurrentOrientation=")
                    .nth(1)
                    .and_then(|v| v.chars().next()?.to_digit(10))
                {
                    return match val {
                        1 => Some(90),
                        2 => Some(180),
                        3 => Some(270),
                        _ => Some(0),
                    };
                }
            }
        }
    }
    // 3. Try settings get system user_rotation
    if let Ok(out) = run_out(
        "adb",
        &[
            "-s",
            serial,
            "shell",
            "settings",
            "get",
            "system",
            "user_rotation",
        ],
    ) {
        if let Ok(val) = out.trim().parse::<u32>() {
            return match val {
                1 => Some(90),
                2 => Some(180),
                3 => Some(270),
                _ => Some(0),
            };
        }
    }
    None
}

/// Computes the optimal scrcpy rotation ("0", "90", "180", "270")
/// given the phone's physical orientation and camera lens (0=Back, 1=Front).
pub fn optimal_camera_rotation(camera_id: &str, surface_deg: u32) -> &'static str {
    let is_front = camera_id == "1";
    if is_front {
        match surface_deg {
            0 => "270",
            90 => "0",
            180 => "90",
            270 => "180",
            _ => "0",
        }
    } else {
        match surface_deg {
            0 => "90",
            90 => "0",
            180 => "270",
            270 => "180",
            _ => "0",
        }
    }
}

pub fn try_auto_reconnect_history(history: &[crate::config::DeviceHistory]) -> Option<String> {
    // A discovered service is not sufficient evidence that this is a remembered phone.
    // Until stable device identity is available, only retry saved endpoints.
    for item in history {
        if item.is_wifi && !item.address.is_empty() {
            if let Ok(connected) = connect_wifi(&item.address) {
                return Some(connected);
            }
        }
    }
    None
}
