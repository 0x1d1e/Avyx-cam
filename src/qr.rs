use mdns_sd::{ServiceDaemon, ServiceEvent};
use qrcode::{Color, QrCode};
use std::fs::File;
use std::io::Read;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ADB_PAIRING_SERVICE: &str = "_adb-tls-pairing._tcp.local.";
const ADB_CONNECT_SERVICE: &str = "_adb-tls-connect._tcp.local.";

pub fn random_token(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = vec![0u8; len];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(13);
        }
    }
    bytes
        .iter()
        .map(|b| CHARSET[*b as usize % CHARSET.len()] as char)
        .collect()
}

pub fn adb_qr_payload(name: &str, password: &str) -> String {
    format!("WIFI:T:ADB;S:{};P:{};;", name, password)
}

pub fn draw_qr_code(cr: &gtk4::cairo::Context, width: i32, height: i32, code: Option<&QrCode>) {
    cr.set_source_rgb(1.0, 1.0, 1.0);
    let _ = cr.paint();
    let Some(code) = code else { return };

    const QUIET_ZONE: usize = 3;
    let modules = code.width();
    let total = (modules + 2 * QUIET_ZONE) as f64;
    let scale = (f64::from(width.min(height)) / total).floor().max(1.0);
    let offset_x = ((f64::from(width) - scale * total) / 2.0).floor();
    let offset_y = ((f64::from(height) - scale * total) / 2.0).floor();

    cr.set_source_rgb(0.0, 0.0, 0.0);
    for (i, color) in code.to_colors().iter().enumerate() {
        if *color == Color::Dark {
            let x = (i % modules + QUIET_ZONE) as f64;
            let y = (i / modules + QUIET_ZONE) as f64;
            cr.rectangle(offset_x + x * scale, offset_y + y * scale, scale, scale);
        }
    }
    let _ = cr.fill();
}

fn poll_adb_mdns(service_keyword: &str) -> Vec<(String, String)> {
    let Ok(out) = Command::new("adb").args(["mdns", "services"]).output() else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let name = parts[0];
            let typ = parts[1];
            let addr = parts[2];
            if typ.to_lowercase().contains(service_keyword) {
                results.push((name.to_string(), addr.to_string()));
            }
        }
    }
    results
}

pub fn pair_and_connect<F>(
    service_name: String,
    password: String,
    cancel: Arc<AtomicBool>,
    mut on_status: F,
) -> Result<String, String>
where
    F: FnMut(&str),
{
    let mdns = ServiceDaemon::new().ok();
    let pair_rx = mdns
        .as_ref()
        .and_then(|d| d.browse(ADB_PAIRING_SERVICE).ok());
    let conn_rx = mdns
        .as_ref()
        .and_then(|d| d.browse(ADB_CONNECT_SERVICE).ok());

    on_status("Waiting for phone to scan QR code...");

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut paired_ip: Option<String> = None;

    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            if let Some(d) = mdns {
                let _ = d.shutdown();
            }
            return Err("Cancelled".to_string());
        }

        // 1. Check adb built-in mdns discovery (fastest and most reliable)
        for (name, addr) in poll_adb_mdns("pairing") {
            if name.contains(&service_name) {
                on_status(&format!("Pairing with {addr}..."));
                let out = Command::new("adb")
                    .args(["pair", &addr, &password])
                    .output()
                    .map_err(|e| format!("adb pair failed: {e}"))?;
                let txt = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                if out.status.success() && txt.to_lowercase().contains("successfully paired") {
                    let ip = addr.split(':').next().unwrap_or("").to_string();
                    paired_ip = Some(ip);
                    break;
                } else {
                    if let Some(d) = mdns {
                        let _ = d.shutdown();
                    }
                    return Err(format!("Pairing failed: {}", txt.trim()));
                }
            }
        }
        if paired_ip.is_some() {
            break;
        }

        // 2. Check mdns-sd crate channel fallback
        if let Some(rx) = &pair_rx {
            if let Ok(ServiceEvent::ServiceResolved(info)) = rx.recv_timeout(Duration::from_millis(250)) {
                let fullname = info.get_fullname();
                if fullname.contains(&service_name) {
                    if let Some(ip) = info.get_addresses_v4().into_iter().next().map(|a| a.to_string()) {
                        let port = info.get_port();
                        let addr = format!("{ip}:{port}");
                        on_status(&format!("Pairing with {addr}..."));

                        let out = Command::new("adb")
                            .args(["pair", &addr, &password])
                            .output()
                            .map_err(|e| format!("adb pair failed: {e}"))?;
                        let txt = format!(
                            "{}{}",
                            String::from_utf8_lossy(&out.stdout),
                            String::from_utf8_lossy(&out.stderr)
                        );
                        if out.status.success() && txt.to_lowercase().contains("successfully paired") {
                            paired_ip = Some(ip);
                            break;
                        }
                        if let Some(d) = mdns {
                            let _ = d.shutdown();
                        }
                        return Err(format!("Pairing failed: {}", txt.trim()));
                    }
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    let Some(target_ip) = paired_ip else {
        if let Some(d) = mdns {
            let _ = d.shutdown();
        }
        return Err("Timed out waiting for QR scan (check Wi-Fi / UDP 5353 firewall).".to_string());
    };

    on_status("Paired! Finding wireless connect port...");

    let conn_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < conn_deadline {
        if cancel.load(Ordering::Relaxed) {
            if let Some(d) = mdns {
                let _ = d.shutdown();
            }
            return Err("Cancelled".to_string());
        }

        // 1. Check adb mdns connect services
        for (_name, addr) in poll_adb_mdns("connect") {
            let ip = addr.split(':').next().unwrap_or("");
            if ip == target_ip || target_ip.is_empty() {
                on_status(&format!("Connecting to {addr}..."));
                let out = Command::new("adb")
                    .args(["connect", &addr])
                    .output()
                    .map_err(|e| format!("adb connect failed: {e}"))?;
                let txt = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                if let Some(d) = mdns {
                    let _ = d.shutdown();
                }
                if txt.to_lowercase().contains("connected") {
                    return Ok(addr);
                }
                return Err(format!("Connect failed: {}", txt.trim()));
            }
        }

        // 2. Check mdns-sd crate
        if let Some(rx) = &conn_rx {
            if let Ok(ServiceEvent::ServiceResolved(info)) = rx.recv_timeout(Duration::from_millis(250)) {
                let matches_ip = info
                    .get_addresses_v4()
                    .into_iter()
                    .any(|a| a.to_string() == target_ip);
                if matches_ip {
                    let addr = format!("{target_ip}:{}", info.get_port());
                    on_status(&format!("Connecting to {addr}..."));
                    let out = Command::new("adb")
                        .args(["connect", &addr])
                        .output()
                        .map_err(|e| format!("adb connect failed: {e}"))?;
                    let txt = format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    if let Some(d) = mdns {
                        let _ = d.shutdown();
                    }
                    if txt.to_lowercase().contains("connected") {
                        return Ok(addr);
                    }
                    return Err(format!("Connect failed: {}", txt.trim()));
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    // Fallback: try connecting on default port 5555
    let fallback_addr = format!("{target_ip}:5555");
    let out = Command::new("adb").args(["connect", &fallback_addr]).output();
    if let Ok(o) = out {
        let txt = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        if txt.to_lowercase().contains("connected") {
            if let Some(d) = mdns {
                let _ = d.shutdown();
            }
            return Ok(fallback_addr);
        }
    }

    if let Some(d) = mdns {
        let _ = d.shutdown();
    }
    Err("Paired, but connect port was not announced. Enter IP:port manually.".to_string())
}
