//! Exercise the production owner and CLI without GTK or phone hardware.
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    fallback: bool,
}
impl Fixture {
    fn new(fallback: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "avyx-owner-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        let adb = root.join("bin/adb");
        fs::write(&adb, "#!/bin/sh\nif [ -n \"$AVYX_DELAY\" ]; then touch \"$AVYX_DELAY\"; sleep 30; fi\nprintf 'List of devices attached\\n'\n").unwrap();
        fs::set_permissions(adb, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root, fallback }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_avyx-cam"));
        cmd.env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env("AVYX_TEST_ROOT", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            );
        if self.fallback {
            cmd.env_remove("XDG_RUNTIME_DIR").env("TMPDIR", &self.root);
        } else {
            cmd.env("XDG_RUNTIME_DIR", &self.root);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd
    }
    fn socket_path(&self) -> PathBuf {
        if self.fallback {
            self.root
                .join(format!("avyx-cam-{}", unsafe { libc::geteuid() }))
                .join("owner.sock")
        } else {
            self.root.join("avyx-cam/owner.sock")
        }
    }
    fn cli(&self, arg: &str) -> Output {
        self.command().arg(arg).output().unwrap()
    }
    fn try_request(&self, request: Value) -> Value {
        let mut socket = UnixStream::connect(self.socket_path()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(6)))
            .unwrap();
        socket
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        socket.shutdown(std::net::Shutdown::Write).unwrap();
        let mut data = Vec::new();
        socket.read_to_end(&mut data).unwrap();
        serde_json::from_slice(&data).unwrap()
    }
    fn request(&self, request: Value) -> Value {
        let mut socket = UnixStream::connect(self.socket_path()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(6)))
            .unwrap();
        socket
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        socket.shutdown(std::net::Shutdown::Write).unwrap();
        let mut data = Vec::new();
        socket.read_to_end(&mut data).unwrap();
        let value: Value = serde_json::from_slice(&data).unwrap();
        value
            .get("Ok")
            .unwrap_or_else(|| panic!("Owner rejected request: {value}"))
            .clone()
    }
    fn tool(&self, name: &str, script: &str) {
        let path = self.root.join("bin").join(name);
        let next = path.with_extension("next");
        fs::write(&next, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&next, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(next, path).unwrap();
    }
    fn wait_for(&self, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "Timed out waiting for lifecycle transition"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn owner_pid(&self) -> i32 {
        let socket = UnixStream::connect(self.socket_path()).unwrap();
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&cred) as libc::socklen_t;
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    &mut cred as *mut _ as *mut _,
                    &mut size,
                )
            },
            0
        );
        cred.pid
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if UnixStream::connect(self.socket_path()).is_ok() {
            let pid = self.owner_pid();
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn headless_races_stop_and_crash_recovery() {
    let f = Fixture::new(false);
    assert!(f.cli("--help").status.success());
    let mut clients = (0..8)
        .map(|_| f.command().arg("status").spawn().unwrap())
        .collect::<Vec<_>>();
    for client in &mut clients {
        assert!(client.wait().unwrap().success());
    }
    let pid = f.owner_pid();
    assert_eq!(f.request(json!("Status"))["phase"], "idle");
    assert!(f.cli("stop").status.success());
    let snapshot = f.request(json!("Status"));
    assert_eq!(snapshot["desired"], false);
    assert_eq!(snapshot["user_stopped"], true);
    let snapshot = f.request(json!({"Configure":{"changes":{"auto_start_usb":true}}}));
    assert_eq!(snapshot["user_stopped"], true);
    assert_eq!(f.owner_pid(), pid);
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let output = f.cli("status");
        if output.status.success() {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    let snapshot = f.request(json!("Status"));
    assert_eq!(snapshot["phase"], "idle");
    assert_eq!(snapshot["desired"], false);
    // Stop survives an owner crash until an explicit Start.
    assert_eq!(snapshot["user_stopped"], true);
}

#[test]
fn stop_survives_owner_restart_and_blocks_usb_autostart_until_explicit_start() {
    let f = Fixture::new(false);
    f.tool("adb", "printf 'List of devices attached\\n'");
    f.tool("scrcpy", "exit 1");
    assert!(f.cli("status").status.success());
    f.request(json!({"Configure":{"changes":{"auto_start_usb":true}}}));
    assert!(f.cli("stop").status.success());
    unsafe {
        libc::kill(f.owner_pid(), libc::SIGKILL);
    }
    f.wait_for(|| UnixStream::connect(f.socket_path()).is_err());
    assert!(f.cli("status").status.success());
    assert_eq!(f.request(json!("Status"))["user_stopped"], true);
    f.wait_for(|| f.request(json!("Status"))["devices"].as_array().is_some());
    f.tool(
        "adb",
        "printf 'List of devices attached\\nusb1 device model:Simulated\\n'",
    );
    std::thread::sleep(Duration::from_millis(2600));
    let status = f.request(json!("Status"));
    assert_eq!(status["phase"], "idle");
    assert_eq!(status["desired"], false);
    // Explicit Start clears the latch durably.
    f.request(json!({"Start":{"serial":null,"changes":{"auto_reconnect":false}}}));
    assert_eq!(f.request(json!("Status"))["user_stopped"], false);
    unsafe {
        libc::kill(f.owner_pid(), libc::SIGKILL);
    }
    f.wait_for(|| UnixStream::connect(f.socket_path()).is_err());
    assert!(f.cli("status").status.success());
    assert_eq!(f.request(json!("Status"))["user_stopped"], false);
}

#[test]
fn stop_and_start_report_latch_io_failures() {
    let f = Fixture::new(false);
    f.tool("adb", "printf 'List of devices attached\\n'");
    assert!(f.cli("status").status.success());
    // A directory in place of the marker makes both create and remove fail.
    let latch = f.root.join("avyx-cam/user-stopped");
    fs::create_dir(&latch).unwrap();
    let reply = f.try_request(json!("Stop"));
    assert!(
        reply["Err"].as_str().is_some_and(|e| e.contains("durably")),
        "{reply}"
    );
    // The in-memory Stop still applies.
    assert_eq!(f.request(json!("Status"))["user_stopped"], true);
    let reply = f.try_request(json!({"Start":{"serial":null,"changes":{}}}));
    assert!(
        reply["Err"]
            .as_str()
            .is_some_and(|e| e.contains("Cannot persist")),
        "{reply}"
    );
    let status = f.request(json!("Status"));
    assert_eq!(status["desired"], false);
    assert_eq!(status["user_stopped"], true);
    fs::remove_dir(&latch).unwrap();
    f.request(json!({"Start":{"serial":null,"changes":{"auto_reconnect":false}}}));
    assert_eq!(f.request(json!("Status"))["user_stopped"], false);
}

#[test]
#[ignore = "requires /dev/video0; use a sandbox placeholder"]
fn wifi_handoff_during_inflight_usb_start_survives_and_recovers_after_unplug() {
    let f = streaming_fixture();
    f.tool(
        "adb",
        r#"
case "$1" in
 connect) echo "connected to $2" ;;
 devices)
  printf 'List of devices attached\n'
  if [ -f "$AVYX_TEST_ROOT/unplugged" ]; then echo '192.0.2.5:5555 device model:Simulated'
  else echo 'usb1 device model:Simulated'; echo '192.0.2.5:5555 device model:Simulated'; fi ;;
esac
"#,
    );
    f.tool(
        "scrcpy",
        r#"
case "$*" in
 *--list-camera-sizes*)
  touch "$AVYX_TEST_ROOT/probing"
  while [ ! -f "$AVYX_TEST_ROOT/release" ]; do sleep 0.05; done
  exit 0 ;;
esac
case "$*" in
 *usb1*) while [ ! -f "$AVYX_TEST_ROOT/unplugged" ]; do sleep 0.05; done; exit 1 ;;
esac
exec sleep 60
"#,
    );
    f.request(json!({"Start":{"serial":"usb1","changes":{"auto_rotate":false,"mic_mode":"off"}}}));
    // The worker is now stuck probing on the USB serial.
    f.wait_for(|| f.root.join("probing").exists());
    f.request(json!({"WifiEndpoint":{"endpoint":"192.0.2.5:5555","model":"Simulated","switch_from":"usb1"}}));
    fs::write(f.root.join("release"), "").unwrap();
    f.wait_for(|| f.request(json!("Status"))["phase"] == "streaming");
    assert_eq!(
        f.request(json!("Status"))["serial"],
        "192.0.2.5:5555",
        "In-flight USB start must not overwrite the Wi-Fi handoff"
    );
    let usb_pid = f.request(json!("Status"))["pid"].clone();
    fs::write(f.root.join("unplugged"), "").unwrap();
    f.wait_for(|| {
        let s = f.request(json!("Status"));
        s["phase"] == "streaming" && s["pid"] != usb_pid && s["serial"] == "192.0.2.5:5555"
    });
    assert!(f.cli("stop").status.success());
}

#[test]
fn stale_clients_cannot_overwrite_each_others_settings() {
    let f = Fixture::new(false);
    assert!(f.cli("status").status.success());
    // Client A and B both started from the same snapshot and change different fields.
    f.request(json!({"Configure":{"changes":{"zoom":2.0}}}));
    f.request(json!({"Configure":{"changes":{"fps":"15"}}}));
    let cfg = &f.request(json!("Status"))["cfg"];
    assert_eq!(cfg["zoom"], 2.0);
    assert_eq!(cfg["fps"], "15");
    // Unknown and owner-managed fields are rejected, leaving state untouched.
    let mut socket = UnixStream::connect(f.socket_path()).unwrap();
    socket
        .write_all(br#"{"Configure":{"changes":{"history":[],"bogus":1}}}"#)
        .unwrap();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    assert!(reply.contains("Err"), "{reply}");
    assert_eq!(f.request(json!("Status"))["cfg"]["zoom"], 2.0);
}

#[test]
#[ignore = "requires /dev/video0; use a sandbox placeholder"]
fn usb_to_wifi_switch_rebinds_owner_and_recovers_after_unplug() {
    let f = streaming_fixture();
    f.tool(
        "adb",
        r#"
case "$1" in
 connect) echo "connected to $2" ;;
 devices)
  printf 'List of devices attached\n'
  if [ -f "$AVYX_TEST_ROOT/unplugged" ]; then echo '192.0.2.5:5555 device model:Simulated'
  else echo 'usb1 device model:Simulated'; echo '192.0.2.5:5555 device model:Simulated'; fi ;;
esac
"#,
    );
    f.tool(
        "scrcpy",
        r#"
case "$*" in
 *--list-camera-sizes*) exit 0 ;;
esac
printf '%s\n' "$*" >> "$AVYX_TEST_ROOT/launches"
case "$*" in
 *usb1*) while [ ! -f "$AVYX_TEST_ROOT/unplugged" ]; do sleep 0.05; done; exit 1 ;;
esac
exec sleep 60
"#,
    );
    f.request(json!({"Start":{"serial":"usb1","changes":{"auto_rotate":false,"mic_mode":"off"}}}));
    f.wait_for(|| f.request(json!("Status"))["phase"] == "streaming");
    let usb_pid = f.request(json!("Status"))["pid"].clone();
    f.request(json!({"WifiEndpoint":{"endpoint":"192.0.2.5:5555","model":"Simulated","switch_from":"usb1"}}));
    let status = f.request(json!("Status"));
    assert_eq!(status["serial"], "192.0.2.5:5555");
    assert_eq!(
        status["pid"], usb_pid,
        "Handoff must not interrupt the healthy stream"
    );
    fs::write(f.root.join("unplugged"), "").unwrap();
    f.wait_for(|| {
        let s = f.request(json!("Status"));
        s["phase"] == "streaming" && s["pid"] != usb_pid && s["serial"] == "192.0.2.5:5555"
    });
    assert!(f.cli("stop").status.success());
}

#[test]
fn stop_cancels_probe_and_never_touches_unrelated_process() {
    let f = Fixture::new(false);
    f.tool(
        "adb",
        "printf 'List of devices attached\\nphone device model:Simulated\\n'",
    );
    f.tool(
        "scrcpy",
        "echo $$ > \"$AVYX_TEST_ROOT/probe.pid\"; exec sleep 30",
    );
    let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
    assert!(f.cli("status").status.success());
    f.request(json!({"Start":{"serial":null,"changes":{"auto_rotate":false}}}));
    f.wait_for(|| f.root.join("probe.pid").exists());
    let child_pid: i32 = fs::read_to_string(f.root.join("probe.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let before = Instant::now();
    let stopped = f.cli("stop");
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(
        !PathBuf::from(format!("/proc/{child_pid}")).exists(),
        "Probe must be reaped before Stop acknowledges"
    );
    assert!(unrelated.try_wait().unwrap().is_none());
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    // Device remains present, but neither USB auto-start nor retries can undo Stop.
    f.request(json!({"Configure":{"changes":{"auto_reconnect":true,"auto_start_usb":true}}}));
    std::thread::sleep(Duration::from_millis(2200));
    let status = f.request(json!("Status"));
    assert_eq!(status["phase"], "idle");
    assert_eq!(status["desired"], false);
}

#[test]
fn disabling_reconnect_cancels_attempt_and_stop_invalidates_waiting() {
    let f = Fixture::new(false);
    f.tool("adb", "if [ \"$1\" = connect ]; then echo $$ > \"$AVYX_TEST_ROOT/connect.pid\"; exec sleep 30; fi\nprintf 'List of devices attached\\n'");
    assert!(f.cli("status").status.success());
    f.request(json!({"Start":{"serial":null,"changes":{"wireless_ip":"192.0.2.1:5555"}}}));
    f.wait_for(|| f.root.join("connect.pid").exists());
    let pid: i32 = fs::read_to_string(f.root.join("connect.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let before = Instant::now();
    f.request(
        json!({"Configure":{"changes":{"auto_reconnect":false,"wireless_ip":"192.0.2.1:5555"}}}),
    );
    f.wait_for(|| !PathBuf::from(format!("/proc/{pid}")).exists());
    assert!(before.elapsed() < Duration::from_secs(1));
    assert_eq!(f.request(json!("Status"))["phase"], "failed");
    f.tool("adb", "printf 'List of devices attached\\n'");
    f.request(json!({"Start":{"serial":null,"changes":{"auto_reconnect":true}}}));
    f.wait_for(|| f.request(json!("Status"))["phase"] == "waiting");
    assert!(f.cli("stop").status.success());
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(f.request(json!("Status"))["phase"], "idle");
}

#[test]
fn unauthorized_is_actionable_and_cli_start_fails() {
    let f = Fixture::new(false);
    f.tool(
        "adb",
        "printf 'List of devices attached\\nphone unauthorized model:Simulated\\n'",
    );
    let output = f.cli("back");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Allow USB debugging"));
    let status = f.request(json!("Status"));
    assert_eq!(status["phase"], "failed");
    assert_eq!(status["pid"], Value::Null);
}

#[test]
fn owner_death_kills_inflight_command_and_restarts_idle() {
    let f = Fixture::new(false);
    f.tool(
        "adb",
        "echo $$ > \"$AVYX_TEST_ROOT/discovery.pid\"; exec sleep 30",
    );
    assert!(f.cli("status").status.success());
    f.wait_for(|| f.root.join("discovery.pid").exists());
    let pid: i32 = fs::read_to_string(f.root.join("discovery.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let owner = f.owner_pid();
    unsafe {
        libc::kill(owner, libc::SIGKILL);
    }
    // A zombie may remain until the host's init reaps it; it must no longer execute.
    f.wait_for(|| {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| s.contains(") Z "))
            .unwrap_or(true)
    });
    f.tool("adb", "printf 'List of devices attached\\n'");
    f.wait_for(|| f.cli("status").status.success());
    assert_ne!(f.owner_pid(), owner);
    assert_eq!(f.request(json!("Status"))["desired"], false);
}

#[test]
fn slow_protocol_client_cannot_block_stop() {
    let f = Fixture::new(false);
    assert!(f.cli("status").status.success());
    let mut slow = UnixStream::connect(f.socket_path()).unwrap();
    slow.write_all(b"{").unwrap();
    let before = Instant::now();
    assert!(f.cli("stop").status.success());
    assert!(before.elapsed() < Duration::from_secs(1));
}

#[test]
fn fallback_is_private_and_stop_cancels_hung_discovery() {
    let f = Fixture::new(true);
    let marker = f.root.join("discovery-started");
    let output = f
        .command()
        .env("AVYX_DELAY", &marker)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::metadata(f.socket_path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    f.request(json!({"Start":{"serial":null,"changes":{}}}));
    let before = Instant::now();
    assert!(f.cli("stop").status.success());
    assert!(before.elapsed() < Duration::from_secs(1));
    let snapshot = f.request(json!("Status"));
    assert_eq!(snapshot["phase"], "idle");
    assert_eq!(snapshot["desired"], false);
    assert_eq!(snapshot["pid"], Value::Null);
}

fn streaming_fixture() -> Fixture {
    assert!(
        std::path::Path::new("/dev/video0").exists(),
        "Run in a sandbox with a video placeholder"
    );
    let f = Fixture::new(false);
    f.tool(
        "adb",
        "printf 'List of devices attached\\nphone device model:Simulated\\n'",
    );
    f.tool(
        "scrcpy",
        r#"
case "$*" in
  *--list-camera-sizes*) exit 0 ;;
esac
printf '%s\n' "$$" >> "$AVYX_TEST_ROOT/streams"
if [ -f "$AVYX_TEST_ROOT/ignore-term" ]; then trap '' TERM; fi
exec sleep 60
"#,
    );
    assert!(f.cli("status").status.success());
    f
}

#[test]
#[ignore = "requires /dev/video0; use a sandbox placeholder"]
fn streaming_replacements_are_serial_and_forced_stop_is_bounded() {
    let f = streaming_fixture();
    fs::write(f.root.join("ignore-term"), "").unwrap();
    let start = json!({"Start":{"serial":null,"changes":{"auto_rotate":false,"mic_mode":"off"}}});
    f.request(json!({"Configure":{"changes":{"auto_rotate":false,"mic_mode":"off"}}}));
    let headless = f.cli("back");
    assert!(
        headless.status.success(),
        "{}",
        String::from_utf8_lossy(&headless.stderr)
    );
    assert!(String::from_utf8_lossy(&headless.stdout).contains("Frame delivery is not verified"));
    f.wait_for(|| f.request(json!("Status"))["phase"] == "streaming");
    let first = f.request(json!("Status"))["pid"].as_u64().unwrap();
    let mut requests = vec![];
    for _ in 0..8 {
        let path = f.socket_path();
        let value = start.clone();
        requests.push(std::thread::spawn(move || {
            let mut socket = UnixStream::connect(path).unwrap();
            socket
                .write_all(&serde_json::to_vec(&value).unwrap())
                .unwrap();
            socket.shutdown(std::net::Shutdown::Write).unwrap();
            let mut reply = String::new();
            socket.read_to_string(&mut reply).unwrap();
            assert!(reply.contains("Ok"));
        }));
    }
    for request in requests {
        request.join().unwrap();
    }
    f.wait_for(|| f.request(json!("Status"))["phase"] == "streaming");
    assert!(
        !PathBuf::from(format!("/proc/{first}")).exists(),
        "Old stream was not reaped"
    );
    let snapshot = f.request(json!("Status"));
    let active = snapshot["pid"].as_u64().unwrap();
    let pids = fs::read_to_string(f.root.join("streams")).unwrap();
    assert_eq!(
        pids.lines()
            .filter(|pid| PathBuf::from(format!("/proc/{pid}")).exists())
            .count(),
        1
    );
    f.request(
        json!({"Configure":{"changes":{"auto_rotate":false,"mic_mode":"off","auto_reconnect":false}}}),
    );
    assert_eq!(f.request(json!("Status"))["pid"], active);
    let before = Instant::now();
    assert!(f.cli("stop").status.success());
    assert!(before.elapsed() < Duration::from_secs(5));
    assert!(!PathBuf::from(format!("/proc/{active}")).exists());
    // Rapid intent replacement must finish in the last explicit Start.
    f.request(start.clone());
    f.request(json!("Stop"));
    f.request(start);
    f.wait_for(|| f.request(json!("Status"))["phase"] == "streaming");
    assert!(f.cli("stop").status.success());
}

#[test]
#[ignore = "requires /dev/video0; use a sandbox placeholder"]
fn active_owner_death_kills_stream_and_restart_stays_idle_with_usb_autostart() {
    let f = streaming_fixture();
    f.request(json!({"Start":{"serial":null,"changes":{"auto_rotate":false,"mic_mode":"off","auto_start_usb":true}}}));
    f.wait_for(|| f.request(json!("Status"))["phase"] == "streaming");
    let pid = f.request(json!("Status"))["pid"].as_u64().unwrap();
    unsafe {
        libc::kill(f.owner_pid(), libc::SIGKILL);
    }
    f.wait_for(|| {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| s.contains(") Z "))
            .unwrap_or(true)
    });
    f.wait_for(|| f.cli("status").status.success());
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(f.request(json!("Status"))["phase"], "idle");
    assert_eq!(f.request(json!("Status"))["desired"], false);
}

#[test]
fn selected_wifi_endpoint_takes_priority_over_other_saved_devices() {
    let f = Fixture::new(false);
    f.tool("adb", r#"
case "$1" in
 connect) printf '%s\n' "$2" >> "$AVYX_TEST_ROOT/endpoints"; touch "$AVYX_TEST_ROOT/selected"; echo connected ;;
 devices)
  printf 'List of devices attached\na:5555 device model:Other\n'
  if [ -f "$AVYX_TEST_ROOT/selected" ]; then echo 'z:5555 device model:Selected'; fi ;;
esac
"#);
    assert!(f.cli("status").status.success());
    f.request(json!({"WifiEndpoint":{"endpoint":"z:5555","model":"Selected","switch_from":null}}));
    f.request(json!({"Configure":{"changes":{"auto_rotate":false,"mic_mode":"off","wireless_ip":"a:5555"}}}));
    f.request(json!({"Start":{"serial":"z:5555","changes":{}}}));
    f.wait_for(|| fs::read_to_string(f.root.join("endpoints")).is_ok_and(|s| !s.is_empty()));
    assert_eq!(
        fs::read_to_string(f.root.join("endpoints")).unwrap(),
        "z:5555\n"
    );
    assert!(f.cli("stop").status.success());
}

#[test]
#[ignore = "requires /dev/video0; use a sandbox placeholder"]
fn early_crash_retries_once_then_publishes_effective_resolution() {
    let f = streaming_fixture();
    f.tool(
        "scrcpy",
        r#"
case "$*" in
 *--list-camera-sizes*) exit 0 ;;
esac
printf '%s\n' "$*" >> "$AVYX_TEST_ROOT/resolutions"
case "$*" in
 *--camera-size=1280x720*) exit 1 ;;
esac
exec sleep 60
"#,
    );
    f.request(json!({"Start":{"serial":null,"changes":{"auto_rotate":false,"mic_mode":"off"}}}));
    f.wait_for(|| {
        let s = f.request(json!("Status"));
        s["phase"] == "streaming" && s["cfg"]["resolution"] == "640x480"
    });
    let commands = fs::read_to_string(f.root.join("resolutions")).unwrap();
    assert_eq!(
        commands
            .lines()
            .filter(|line| line.contains("--camera-size=1280x720"))
            .count(),
        2
    );
    assert_eq!(
        commands
            .lines()
            .filter(|line| line.contains("--camera-size=640x480"))
            .count(),
        1
    );
    assert!(f.cli("stop").status.success());
}
