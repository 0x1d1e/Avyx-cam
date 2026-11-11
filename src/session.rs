//! Per-user session ownership. Transport accepts intent; one worker owns all stream work.
use crate::{
    config::{load_config, save_config, AppConfig},
    devices::{self, AdbDevice, CameraInfo},
    process,
    stream::{self, StreamHandle},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
pub enum Request {
    Status,
    Start {
        serial: Option<String>,
        cfg: AppConfig,
    },
    Configure {
        cfg: AppConfig,
    },
    Stop,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub phase: String,
    pub desired: bool,
    pub user_stopped: bool,
    pub serial: Option<String>,
    pub pid: Option<u32>,
    pub error: Option<String>,
    pub devices: Vec<AdbDevice>,
    pub cameras: Vec<CameraInfo>,
    pub cfg: AppConfig,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            desired: false,
            user_stopped: false,
            serial: None,
            pid: None,
            error: None,
            devices: vec![],
            cameras: vec![],
            cfg: AppConfig::default(),
        }
    }
}
const LIMIT: usize = 262_144;

pub fn runtime_dir() -> Result<PathBuf, String> {
    let uid = unsafe { libc::geteuid() };
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let dir = if let Some(base) = base {
        let meta = fs::symlink_metadata(&base).map_err(|e| format!("Runtime directory: {e}"))?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err("XDG_RUNTIME_DIR must be private and owned by this user".into());
        }
        base.join("avyx-cam")
    } else {
        std::env::temp_dir().join(format!("avyx-cam-{uid}"))
    };
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(format!("Create runtime directory: {e}")),
    }
    let meta = fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err("Session directory must be private and owned by this user".into());
    }
    Ok(dir)
}

fn exchange(mut socket: UnixStream, request: &Request) -> Result<Snapshot, String> {
    socket
        .set_read_timeout(Some(Duration::from_secs(6)))
        .map_err(|e| e.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|e| e.to_string())?;
    let data = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    if data.len() > LIMIT {
        return Err("Session request too large".into());
    }
    socket.write_all(&data).map_err(|e| e.to_string())?;
    socket
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    socket
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|e| format!("Session reply: {e}"))?;
    if data.len() > LIMIT {
        return Err("Session reply too large".into());
    }
    serde_json::from_slice::<Result<Snapshot, String>>(&data)
        .map_err(|e| format!("Session protocol: {e}"))?
}

pub fn request(request: Request) -> Result<Snapshot, String> {
    let dir = runtime_dir()?;
    let path = dir.join("owner.sock");
    if let Ok(socket) = UnixStream::connect(&path) {
        return exchange(socket, &request);
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("owner.log"))
        .map_err(|e| e.to_string())?;
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg("--session-owner")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Start session owner: {e}"))?;
    // Reap a racing loser or the eventual owner without tying its lifetime to this client.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(socket) = UnixStream::connect(&path) {
            return exchange(socket, &request);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Session owner did not publish its socket; inspect {}",
                dir.join("owner.log").display()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stream_settings(cfg: &AppConfig) -> serde_json::Value {
    let mut value = serde_json::to_value(cfg).unwrap();
    for field in [
        "auto_reconnect",
        "auto_start_usb",
        "wireless_ip",
        "history",
        "preview",
    ] {
        value.as_object_mut().unwrap().remove(field);
    }
    value
}

pub fn run_owner() -> Result<(), String> {
    let dir = runtime_dir()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("owner.lock"))
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    let path = dir.join("owner.sock");
    if path.exists() {
        fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let state = Arc::new(Mutex::new(Snapshot {
        cfg: load_config(),
        ..Snapshot::default()
    }));
    let generation = Arc::new(AtomicU64::new(1));
    let worker_state = state.clone();
    let worker_generation = generation.clone();
    std::thread::spawn(move || worker(worker_state, worker_generation));
    for connection in listener.incoming() {
        let mut socket = connection.map_err(|e| e.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| e.to_string())?;
        socket
            .set_write_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| e.to_string())?;
        let mut data = Vec::new();
        let result = (|| -> Result<Snapshot, String> {
            // Bound total request time, including clients that keep trickling bytes.
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("Session request timed out".into());
                }
                socket
                    .set_read_timeout(Some(remaining))
                    .map_err(|e| e.to_string())?;
                let mut chunk = [0; 8192];
                let n = socket.read(&mut chunk).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&chunk[..n]);
                if data.len() > LIMIT {
                    return Err("Session request too large".into());
                }
            }
            let request: Request = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
            let stopping = matches!(request, Request::Stop);
            {
                let mut s = state.lock().unwrap();
                match request {
                    Request::Status => (),
                    Request::Stop => {
                        s.desired = false;
                        s.user_stopped = true;
                        s.phase = "stopping".into();
                        s.error = None;
                        generation.fetch_add(1, Ordering::SeqCst);
                    }
                    Request::Start { serial, cfg } => {
                        save_config(&cfg);
                        s.serial = serial;
                        s.cfg = cfg;
                        s.desired = true;
                        s.user_stopped = false;
                        s.error = None;
                        s.phase = "starting".into();
                        generation.fetch_add(1, Ordering::SeqCst);
                    }
                    Request::Configure { cfg } => {
                        let restart = stream_settings(&cfg) != stream_settings(&s.cfg);
                        let cancel_retry =
                            s.cfg.auto_reconnect && !cfg.auto_reconnect && s.pid.is_none();
                        save_config(&cfg);
                        s.cfg = cfg;
                        if restart && s.desired {
                            s.phase = "starting".into();
                        } else if cancel_retry {
                            s.phase = "failed".into();
                        }
                        if (restart && s.desired) || cancel_retry {
                            generation.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            }
            if stopping {
                let deadline = Instant::now() + Duration::from_secs(4);
                while state.lock().unwrap().phase == "stopping" && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if state.lock().unwrap().phase == "stopping" {
                    return Err("Stream cleanup has not completed".into());
                }
            }
            Ok(state.lock().unwrap().clone())
        })();
        if let Ok(data) = serde_json::to_vec(&result) {
            if data.len() <= LIMIT {
                let _ = socket.write_all(&data);
            }
        }
    }
    drop(lock);
    Ok(())
}

fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(5)).min(30))
}
fn fallback_after_crash(
    resolution: &str,
    quick_retried: bool,
    early: bool,
) -> Option<(String, bool)> {
    if !early {
        return Some((resolution.into(), false));
    }
    if !quick_retried {
        return Some((resolution.into(), true));
    }
    stream::next_fallback_resolution(resolution).map(|res| (res.into(), false))
}

fn terminal(error: &str) -> bool {
    let lower = error.to_lowercase();
    [
        "unauthorized",
        "allow usb debugging",
        "privacy",
        "in use",
        "camera_in_use",
        "camera_disabled",
        "disabled by policy",
        "permission denied",
        "missing. enable",
    ]
    .iter()
    .any(|term| lower.contains(term))
}

fn worker(state: Arc<Mutex<Snapshot>>, generation: Arc<AtomicU64>) {
    let mut handle: Option<StreamHandle> = None;
    let mut seen = 0;
    let mut attempt = 0;
    let mut retry = Instant::now();
    let mut poll = Instant::now();
    let mut fallback: Option<String> = None;
    let mut quick_retry = false;
    let mut blocked = false;
    let mut orientation = None;
    let mut known_usb: Option<String> = None;
    let mut discovered = false;
    loop {
        let current = generation.load(Ordering::SeqCst);
        if current != seen {
            drop(handle.take());
            seen = current;
            attempt = 0;
            fallback = None;
            quick_retry = false;
            orientation = None;
            retry = Instant::now();
            poll = Instant::now();
            let mut s = state.lock().unwrap();
            s.pid = None;
            blocked = s.phase != "starting" && s.error.as_deref().is_some_and(terminal);
            if !s.desired {
                s.phase = "idle".into();
            }
        }
        let cfg = state.lock().unwrap().cfg.clone();
        if let Some(h) = handle.as_mut() {
            match process::exited(&h.child) {
                Ok(None) => {
                    if stream::ran_long_enough(h.started_at) {
                        attempt = 0;
                    }
                }
                _ => {
                    let log = stream::read_log();
                    let error = stream::classify_crash(&log)
                        .unwrap_or("scrcpy exited; inspect the session stream.log")
                        .to_string();
                    blocked = terminal(&error);
                    if !blocked {
                        match fallback_after_crash(
                            &h.resolution,
                            h.quick_retried,
                            h.started_at.elapsed() < Duration::from_secs(4),
                        ) {
                            Some((resolution, quick)) => {
                                fallback = Some(resolution);
                                quick_retry = quick;
                            }
                            None => blocked = true,
                        }
                    }
                    drop(handle.take());
                    let mut s = state.lock().unwrap();
                    if generation.load(Ordering::SeqCst) != seen {
                        continue;
                    }
                    s.pid = None;
                    s.error = Some(error);
                    s.phase = if cfg.auto_reconnect && !blocked {
                        "waiting"
                    } else {
                        "failed"
                    }
                    .into();
                    retry = Instant::now() + retry_delay(attempt);
                    attempt = attempt.saturating_add(1);
                }
            }
        }
        if Instant::now() >= poll {
            process::with_cancel(generation.clone(), seen, || {
                let devices = devices::list_adb_devices();
                if process::cancelled() {
                    return;
                }
                let mut s = state.lock().unwrap();
                if generation.load(Ordering::SeqCst) != seen {
                    return;
                }
                let usb = devices
                    .iter()
                    .find(|d| !d.is_wifi && d.state == "device")
                    .map(|d| d.serial.clone());
                if discovered
                    && usb != known_usb
                    && usb.is_some()
                    && !s.desired
                    && !s.user_stopped
                    && cfg.auto_start_usb
                {
                    s.serial = usb.clone();
                    s.desired = true;
                    s.phase = "starting".into();
                }
                known_usb = usb;
                discovered = true;
                if s.devices != devices {
                    for d in devices.iter().filter(|d| d.state == "device") {
                        s.cfg.record_device(
                            &d.serial,
                            &d.model,
                            d.is_wifi,
                            if d.is_wifi { Some(&d.serial) } else { None },
                        );
                    }
                    save_config(&s.cfg);
                }
                s.devices = devices;
            });
            if cfg.auto_rotate {
                if let Some(h) = handle.as_ref() {
                    let next = process::with_cancel(generation.clone(), seen, || {
                        devices::get_device_orientation(&h.serial)
                    });
                    if let Some(next) = next {
                        if orientation.is_some_and(|old| old != next) {
                            let mut s = state.lock().unwrap();
                            if generation.load(Ordering::SeqCst) == seen {
                                s.phase = "starting".into();
                                generation.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        orientation = Some(next);
                    }
                }
            }
            poll = Instant::now() + Duration::from_secs(2);
        }
        let snapshot = state.lock().unwrap().clone();
        let initial = snapshot.phase == "starting";
        if handle.is_none()
            && snapshot.desired
            && !blocked
            && (initial || cfg.auto_reconnect)
            && Instant::now() >= retry
        {
            let result = process::with_cancel(
                generation.clone(),
                seen,
                || -> Result<StreamHandle, String> {
                    // Discovery snapshots can predate a disconnect or revocation.
                    // Revalidate authorization immediately before every attempt.
                    let mut devices = devices::list_adb_devices();
                    let matches = |d: &&AdbDevice| {
                        d.state == "device"
                            && snapshot
                                .serial
                                .as_ref()
                                .is_none_or(|serial| *serial == d.serial)
                    };
                    if !devices.iter().any(|d| matches(&d)) && cfg.auto_reconnect {
                        // Remembered endpoints only. Authorization is checked after connection.
                        let mut endpoints = vec![];
                        if !cfg.wireless_ip.is_empty() {
                            endpoints.push(cfg.wireless_ip.clone());
                        }
                        endpoints.extend(
                            cfg.history
                                .iter()
                                .filter(|d| d.is_wifi && !d.address.is_empty())
                                .map(|d| d.address.clone()),
                        );
                        endpoints.sort();
                        endpoints.dedup();
                        endpoints
                            .sort_by_key(|endpoint| snapshot.serial.as_ref() != Some(endpoint));
                        for endpoint in endpoints.into_iter().take(10) {
                            if process::cancelled() {
                                return Err("Operation cancelled".into());
                            }
                            if devices::connect_wifi(&endpoint).is_ok() {
                                devices = devices::list_adb_devices();
                                if devices.iter().any(|d| matches(&d)) {
                                    break;
                                }
                            }
                        }
                        devices = devices::list_adb_devices();
                    }
                    let device = devices.iter().find(matches).ok_or_else(|| {
                        if devices.iter().any(|d| {
                            d.state == "unauthorized"
                                && snapshot
                                    .serial
                                    .as_ref()
                                    .is_none_or(|serial| *serial == d.serial)
                        }) {
                            "Phone unauthorized. Tap 'Allow USB debugging' on your phone."
                        } else {
                            "No phone found. Connect a phone with USB debugging enabled."
                        }
                    })?;
                    if process::cancelled() {
                        return Err("Operation cancelled".into());
                    }
                    {
                        let mut s = state.lock().unwrap();
                        if generation.load(Ordering::SeqCst) != seen {
                            return Err("Operation cancelled".into());
                        }
                        s.devices = devices.clone();
                        s.serial = Some(device.serial.clone());
                        s.cfg.record_device(
                            &device.serial,
                            &device.model,
                            device.is_wifi,
                            if device.is_wifi {
                                Some(&device.serial)
                            } else {
                                None
                            },
                        );
                        s.phase = "starting".into();
                    }
                    let cameras = devices::probe_cameras(&device.serial);
                    if process::cancelled() {
                        return Err("Operation cancelled".into());
                    }
                    {
                        let mut s = state.lock().unwrap();
                        if generation.load(Ordering::SeqCst) != seen {
                            return Err("Operation cancelled".into());
                        }
                        s.cameras = cameras;
                    }
                    stream::start_scrcpy(&device.serial, &cfg, fallback.as_deref(), quick_retry)
                },
            );
            let mut s = state.lock().unwrap();
            if generation.load(Ordering::SeqCst) != seen {
                drop(s);
                drop(result);
                continue;
            }
            match result {
                Ok(h) => {
                    s.cfg.resolution = h.resolution.clone();
                    s.pid = Some(h.child.id());
                    s.serial = Some(h.serial.clone());
                    s.phase = "streaming".into();
                    s.error = None;
                    handle = Some(h);
                }
                Err(error) => {
                    blocked = terminal(&error);
                    s.phase = if cfg.auto_reconnect && !blocked {
                        "waiting"
                    } else {
                        "failed"
                    }
                    .into();
                    s.error = Some(error);
                    retry = Instant::now() + retry_delay(attempt);
                    attempt = attempt.saturating_add(1);
                }
            }
            save_config(&s.cfg);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backoff_and_terminal_policy() {
        assert_eq!(
            (0..8).map(|n| retry_delay(n).as_secs()).collect::<Vec<_>>(),
            vec![1, 2, 4, 8, 16, 30, 30, 30]
        );
        assert!(terminal(
            "Phone camera access is blocked by privacy toggle."
        ));
        assert!(terminal("Tap 'Allow USB debugging' on your phone."));
        assert!(!terminal(
            "No phone found. Connect a phone with USB debugging enabled."
        ));
        assert!(!terminal("Phone disconnected."));
        for log in [
            "CAMERA_IN_USE",
            "CAMERA_DISABLED",
            "permission denied",
            "unauthorized",
        ] {
            assert!(terminal(stream::classify_crash(log).unwrap()));
        }
        // Exercise the healthy-run reset threshold without real-time sleeps.
        let now = Instant::now();
        assert!(!stream::ran_long_enough(now));
        assert!(stream::ran_long_enough(now - Duration::from_secs(10)));
        assert_eq!(
            fallback_after_crash("1280x720", false, true),
            Some(("1280x720".into(), true))
        );
        assert_eq!(
            fallback_after_crash("1280x720", true, true),
            Some(("640x480".into(), false))
        );
        assert_eq!(fallback_after_crash("640x480", true, true), None);
        assert_eq!(
            fallback_after_crash("640x480", true, false),
            Some(("640x480".into(), false))
        );
    }
}
