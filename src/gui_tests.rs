//! Actual GTK controls and production CLI/owner against controlled external tools.
//! cargo build && cargo test gui_smoke -- --ignored --test-threads=1
//! Requires a graphical session and /dev/video0 (a sandbox placeholder suffices).
use adw::prelude::*;
use libadwaita as adw;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn pump(seconds: f64) {
    let until = Instant::now() + Duration::from_secs_f64(seconds);
    let context = gtk4::glib::MainContext::default();
    while Instant::now() < until {
        while context.pending() {
            context.iteration(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(8);
    while !predicate() {
        assert!(Instant::now() < until, "GUI lifecycle transition timed out");
        pump(0.05);
    }
}
fn widgets(root: &gtk4::Widget) -> Vec<gtk4::Widget> {
    let mut result = vec![root.clone()];
    let mut child = root.first_child();
    while let Some(widget) = child {
        result.extend(widgets(&widget));
        child = widget.next_sibling();
    }
    result
}
fn button(window: &gtk4::Window, label: &str) -> gtk4::Button {
    widgets(window.upcast_ref())
        .into_iter()
        .find_map(|w| {
            w.downcast::<gtk4::Button>()
                .ok()
                .filter(|b| b.label().as_deref() == Some(label))
        })
        .unwrap_or_else(|| panic!("Missing button: {label}"))
}
fn script(root: &Path, name: &str, body: &str) {
    let path = root.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn status() -> crate::session::Snapshot {
    crate::session::request(crate::session::Request::Status).unwrap()
}
struct Owner(Child);
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires graphical session, /dev/video0, and cargo build"]
fn gui_smoke_shared_session_and_battery() {
    assert!(
        Path::new("/dev/video0").exists(),
        "Use a sandbox video placeholder or real v4l2loopback"
    );
    let root = std::env::temp_dir().join(format!("avyx-gui-test-{}", std::process::id()));
    fs::create_dir_all(root.join("config/avyx-cam")).unwrap();
    fs::create_dir_all(root.join("runtime")).unwrap();
    fs::set_permissions(root.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.join("config/avyx-cam/config.json"), r#"{
        "auto_reconnect": false, "auto_rotate": false, "preview": false,
        "history": [{"serial":"192.0.2.1:5555", "model":"Test phone", "address":"192.0.2.1:5555", "is_wifi":true, "last_seen":"test"}]
    }"#).unwrap();
    script(
        &root,
        "adb",
        r#"
printf '%s\n' "$*" >> "$AVYX_GUI_FIXTURE/adb.log"
case "$*" in
  'devices -l')
    printf 'List of devices attached\n'
    if [ -f "$AVYX_GUI_FIXTURE/connected" ]; then
      printf '192.0.2.1:5555 device model:Test_phone\n'
    elif [ -f "$AVYX_GUI_FIXTURE/offline" ]; then
      printf '192.0.2.1:5555 offline model:Test_phone\n'
    fi ;;
  *'shell dumpsys battery')
    sleep 2
    printf 'level: 83\nscale: 100\nstatus: 2\ntemperature: 327\n' ;;
  connect*) printf 'failed to connect\n'; exit 1 ;;
esac
"#,
    );
    script(
        &root,
        "scrcpy",
        r#"
case "$*" in
  *--list-camera-sizes*)
    if [ -f "$AVYX_GUI_FIXTURE/slow-probe" ]; then
      touch "$AVYX_GUI_FIXTURE/probing"; exec sleep 30
    fi ;;
  *) echo $$ >> "$AVYX_GUI_FIXTURE/streams"; exec sleep 60 ;;
esac
"#,
    );
    script(
        &root,
        "pactl",
        "printf '1\tPhoneMic\tfixture\n2\tPhoneMic_sink\tfixture\n'",
    );
    script(&root, "v4l2-ctl", "printf 'Width/Height : 1280/720\n'");
    for name in ["pgrep", "pkill", "fuser"] {
        script(&root, name, "exit 1");
    }
    std::env::set_var(
        "PATH",
        format!("{}:{}", root.display(), std::env::var("PATH").unwrap()),
    );
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_RUNTIME_DIR", root.join("runtime"));
    std::env::set_var("AVYX_GUI_FIXTURE", &root);
    // Unit-test binaries do not dispatch main's --session-owner, so launch the
    // production binary first and have GUI and CLI attach to its private socket.
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("avyx-cam");
    let _owner = Owner(
        Command::new(&binary)
            .arg("--session-owner")
            .spawn()
            .unwrap(),
    );
    wait_for(|| root.join("runtime/avyx-cam/owner.sock").exists());
    let app = adw::Application::builder()
        .application_id("com.local.avyxcam.tests")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gtk4::gio::Cancellable>).unwrap();
    crate::ui::build_ui(&app);
    let window = app.active_window().unwrap();
    let all = widgets(window.upcast_ref());
    let battery = all
        .iter()
        .find_map(|w| {
            w.clone()
                .downcast::<adw::ActionRow>()
                .ok()
                .filter(|r| r.title() == "Phone Battery")
        })
        .unwrap();
    let reconnect = all
        .iter()
        .find_map(|w| {
            w.clone()
                .downcast::<adw::SwitchRow>()
                .ok()
                .filter(|r| r.title().contains("Auto-reconnect"))
        })
        .unwrap();
    let start = button(&window, "Start Camera");
    pump(1.0);
    assert!(battery.subtitle().unwrap().contains("Connect a phone"));
    reconnect.set_active(true);
    pump(0.6);
    assert!(
        !fs::read_to_string(root.join("adb.log"))
            .unwrap()
            .contains("connect "),
        "Idle must not create streaming intent"
    );

    // Slow battery work leaves GTK responsive and cannot survive deselection.
    fs::write(root.join("connected"), "").unwrap();
    wait_for(|| {
        fs::read_to_string(root.join("adb.log"))
            .unwrap()
            .contains("shell dumpsys battery")
    });
    fs::remove_file(root.join("connected")).unwrap();
    pump(3.0);
    assert!(battery.subtitle().unwrap().contains("Connect a phone"));
    fs::write(root.join("connected"), "").unwrap();
    let ticks = std::rc::Rc::new(std::cell::Cell::new(0));
    let counter = ticks.clone();
    let timer = gtk4::glib::timeout_add_local(Duration::from_millis(20), move || {
        counter.set(counter.get() + 1);
        gtk4::glib::ControlFlow::Continue
    });
    wait_for(|| battery.subtitle().unwrap().contains("83%"));
    assert!(battery
        .subtitle()
        .unwrap()
        .contains("32.7 °C battery temperature"));
    assert!(ticks.get() >= 20, "Battery work blocked GTK");

    // Stop during slow preparation must stay usable, with no late stream spawn.
    fs::write(root.join("slow-probe"), "").unwrap();
    start.emit_clicked();
    wait_for(|| root.join("probing").exists());
    assert_eq!(start.label().as_deref(), Some("Stop Camera"));
    let before = Instant::now();
    start.emit_clicked();
    wait_for(|| status().phase == "idle");
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(!root.join("streams").exists());
    fs::remove_file(root.join("slow-probe")).unwrap();
    wait_for(|| start.label().as_deref() == Some("Start Camera"));
    start.emit_clicked();
    wait_for(|| status().phase == "streaming");
    let pid = status().pid.unwrap();
    reconnect.set_active(false);
    pump(0.6);
    assert_eq!(
        status().pid,
        Some(pid),
        "Disabling reconnect stopped healthy stream"
    );
    reconnect.set_active(true);
    pump(0.6);
    timer.remove();

    if let Ok(path) = std::env::var("AVYX_GUI_SCREENSHOT") {
        let paintable = gtk4::WidgetPaintable::new(Some(&window));
        pump(0.2);
        let snapshot = gtk4::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(window.width()),
            f64::from(window.height()),
        );
        window
            .renderer()
            .unwrap()
            .render_texture(snapshot.to_node().unwrap(), None)
            .save_to_png(path)
            .unwrap();
    }
    // Closing and reopening attaches to the same supervised child.
    window.close();
    pump(0.2);
    assert_eq!(status().pid, Some(pid));
    crate::ui::build_ui(&app);
    let reopened = app.active_window().unwrap();
    wait_for(|| {
        widgets(reopened.upcast_ref()).iter().any(|w| {
            w.clone()
                .downcast::<gtk4::Button>()
                .ok()
                .is_some_and(|b| b.label().as_deref() == Some("Stop Camera"))
        })
    });
    assert_eq!(status().pid, Some(pid));

    // Simulate scrcpy losing its link; CLI Stop must clear the GUI retry intent.
    fs::remove_file(root.join("connected")).unwrap();
    fs::write(root.join("offline"), "").unwrap();
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    wait_for(|| status().phase == "waiting");
    let stopped = Command::new(&binary).arg("stop").output().unwrap();
    assert!(stopped.status.success());
    wait_for(|| {
        widgets(reopened.upcast_ref()).iter().any(|w| {
            w.clone()
                .downcast::<gtk4::Button>()
                .ok()
                .is_some_and(|b| b.label().as_deref() == Some("Start Camera"))
        })
    });
    let starts = fs::read_to_string(root.join("streams")).unwrap();
    fs::write(root.join("connected"), "").unwrap();
    pump(3.0);
    assert!(!status().desired);
    assert_eq!(status().phase, "idle");
    assert_eq!(fs::read_to_string(root.join("streams")).unwrap(), starts);
    // Keyboard/accessibility value changes have no pointer-release event.
    let zoom = widgets(reopened.upcast_ref())
        .into_iter()
        .find_map(|w| w.downcast::<gtk4::Scale>().ok())
        .unwrap();
    zoom.set_value(2.0);
    wait_for(|| status().cfg.zoom == 2.0);
    let saved: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("config/avyx-cam/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["zoom"], 2.0);
    reopened.close();
    pump(0.2);
    drop(_owner);
    crate::preview::tests::exercise_clickable_preview(&app);
    fs::remove_dir_all(root).unwrap();
}
