use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct BatteryStatus {
    pub percent: Option<u8>,
    pub state: &'static str,
    pub temperature_c: Option<f64>,
}

impl BatteryStatus {
    pub fn subtitle(&self) -> String {
        let level = self
            .percent
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "Level unavailable".into());
        let temperature = self
            .temperature_c
            .map(|t| format!("{t:.1} °C battery temperature"))
            .unwrap_or_else(|| "Battery temperature unavailable".into());
        format!("{level} · {} · {temperature}", self.state)
    }
}

pub fn parse_battery(output: &str) -> Result<BatteryStatus, String> {
    let mut level = None;
    let mut scale = None;
    let mut status = None;
    let mut temperature = None;
    for line in output.lines() {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = value.trim().parse::<i32>().ok();
        match key {
            "level" if level.is_none() => level = value,
            "scale" if scale.is_none() => scale = value,
            "status" if status.is_none() => status = value,
            "temperature" if temperature.is_none() => temperature = value,
            _ => {}
        }
    }
    let percent = match (level, scale) {
        (Some(level), Some(scale)) if scale > 0 && (0..=scale).contains(&level) => {
            Some((i64::from(level) * 100 / i64::from(scale)) as u8)
        }
        _ => None,
    };
    let state = match status {
        Some(2) => "Charging",
        Some(3) => "Discharging",
        Some(4) => "Not charging",
        Some(5) => "Full",
        _ => "Charge state unavailable",
    };
    let temperature_c = temperature
        .filter(|t| (-500..=1500).contains(t))
        .map(|t| f64::from(t) / 10.0);
    if percent.is_none() && status.is_none() && temperature_c.is_none() {
        return Err("Phone did not report battery data".into());
    }
    Ok(BatteryStatus {
        percent,
        state,
        temperature_c,
    })
}

pub fn query_battery(serial: &str) -> Result<BatteryStatus, String> {
    query_with_program("adb", serial, QUERY_TIMEOUT)
}

fn query_with_program(
    program: &str,
    serial: &str,
    timeout: Duration,
) -> Result<BatteryStatus, String> {
    let (mut reader, writer) = UnixStream::pair().map_err(|e| e.to_string())?;
    reader.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut child = Command::new(program)
        .args(["-s", serial, "shell", "dumpsys", "battery"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Battery query: {e}"))?;
    let started = Instant::now();
    let mut output = Vec::new();
    let result = loop {
        if started.elapsed() >= timeout {
            break Err("Battery query timed out".into());
        }
        let mut buffer = [0; 4096];
        let mut eof = false;
        match reader.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(n) => {
                if output.len() + n > MAX_OUTPUT {
                    break Err("Battery query output exceeded limit".into());
                }
                output.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => break Err(format!("Battery query: {e}")),
        }
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => break Err("Battery query failed".into()),
            Ok(Some(_)) if eof => break parse_battery(&String::from_utf8_lossy(&output)),
            Err(e) => break Err(format!("Battery query: {e}")),
            _ => {}
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Always reap the child, including timeout and output-limit failures.
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    // Serialize script creation/spawn: concurrent forks can inherit another test's
    // writable script descriptor briefly, causing Linux ETXTBSY on exec.
    static PROCESS_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn query_script(script: &str, timeout: Duration) -> Result<BatteryStatus, String> {
        let _guard = PROCESS_TEST.lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "avyx-battery-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = query_with_program(path.to_str().unwrap(), "test-phone", timeout);
        std::fs::remove_file(path).unwrap();
        result
    }

    #[test]
    fn parses_android_units_and_scaled_percentage() {
        let status = parse_battery("Current Battery Service state:\n  level: 150\n  scale: 200\n  status: 2\n  temperature: 327\n").unwrap();
        assert_eq!(status.percent, Some(75));
        assert_eq!(status.state, "Charging");
        assert_eq!(status.temperature_c, Some(32.7));
        assert!(status.subtitle().contains("battery temperature"));
    }

    #[test]
    fn handles_missing_invalid_and_full_readings() {
        let status =
            parse_battery("level: 101\nscale: 100\nstatus: 5\ntemperature: 99999").unwrap();
        assert_eq!(status.percent, None);
        assert_eq!(status.temperature_c, None);
        assert_eq!(status.state, "Full");
        assert!(parse_battery("permission denied").is_err());
        assert!(parse_battery("level: invalid\nscale: 0").is_err());
        assert_eq!(parse_battery("status: 3").unwrap().state, "Discharging");
    }

    #[test]
    fn runs_real_query_with_expected_arguments() {
        let script = "[ \"$*\" = '-s test-phone shell dumpsys battery' ] || exit 1\nprintf 'level: 83\\nscale: 100\\nstatus: 3\\ntemperature: 290\\n'";
        assert_eq!(
            query_script(script, Duration::from_secs(2))
                .unwrap()
                .percent,
            Some(83)
        );
    }

    #[test]
    fn reports_process_failure() {
        assert_eq!(
            query_script("exit 1", Duration::from_secs(2)).unwrap_err(),
            "Battery query failed"
        );
    }

    #[test]
    fn terminates_and_reaps_timed_out_child() {
        let started = Instant::now();
        let pid_path =
            std::env::temp_dir().join(format!("avyx-battery-child-{}", std::process::id()));
        let script = format!("echo $$ > '{}'\nexec sleep 10", pid_path.display());
        let result = query_script(&script, Duration::from_millis(200));
        assert_eq!(result.unwrap_err(), "Battery query timed out");
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = std::fs::read_to_string(&pid_path).unwrap();
        assert!(
            !std::path::Path::new(&format!("/proc/{}", pid.trim())).exists(),
            "timed-out child was not reaped"
        );
        std::fs::remove_file(pid_path).unwrap();
    }

    #[test]
    fn bounds_subprocess_output() {
        assert_eq!(
            query_script("exec yes battery", Duration::from_secs(3)).unwrap_err(),
            "Battery query output exceeded limit"
        );
    }
}
