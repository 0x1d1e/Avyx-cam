//! Runs the actual GTK window against controlled external tools.
//! Run alone in a graphical session: cargo test gui_smoke -- --ignored --test-threads=1
use adw::prelude::*;
use libadwaita as adw;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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

fn widgets(root: &gtk4::Widget) -> Vec<gtk4::Widget> {
    let mut result = vec![root.clone()];
    let mut child = root.first_child();
    while let Some(widget) = child {
        result.extend(widgets(&widget));
        child = widget.next_sibling();
    }
    result
}

fn script(root: &Path, name: &str, body: &str) {
    let path = root.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
#[ignore = "requires a graphical session, /dev/video0, and no active webcam stream"]
fn gui_smoke_battery_and_reconnect_intent() {
    assert!(
        Path::new("/dev/video0").exists(),
        "GUI streaming scenario requires /dev/video0"
    );
    assert!(
        crate::stream::read_external_pid().is_none(),
        "Stop active webcam streams before this test"
    );
    let root = std::env::temp_dir().join(format!("avyx-gui-test-{}", std::process::id()));
    fs::create_dir_all(root.join("config/avyx-cam")).unwrap();
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
    if [ -f "$AVYX_GUI_FIXTURE/connected" ]; then printf '192.0.2.1:5555 device model:Test_phone\n'; fi ;;
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
        "case \"$*\" in *--list-camera-sizes*) exit 0;; *) exec sleep 60;; esac",
    );
    script(
        &root,
        "pactl",
        "printf '1\\tPhoneMic\\tfixture\\n2\\tPhoneMic_sink\\tfixture\\n'",
    );
    script(&root, "v4l2-ctl", "printf 'Width/Height : 1280/720\\n'");
    for name in ["pgrep", "pkill", "fuser"] {
        script(&root, name, "exit 1");
    }
    std::env::set_var(
        "PATH",
        format!("{}:{}", root.display(), std::env::var("PATH").unwrap()),
    );
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("AVYX_GUI_FIXTURE", &root);

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
                .filter(|row| row.title() == "Phone Battery")
        })
        .unwrap();
    let reconnect = all
        .iter()
        .find_map(|w| {
            w.clone()
                .downcast::<adw::SwitchRow>()
                .ok()
                .filter(|row| row.title().contains("Auto-reconnect"))
        })
        .unwrap();
    let start = all
        .iter()
        .find_map(|w| {
            w.clone()
                .downcast::<gtk4::Button>()
                .ok()
                .filter(|button| button.label().as_deref() == Some("Start Camera"))
        })
        .unwrap();
    pump(2.0);
    assert!(!fs::read_to_string(root.join("adb.log"))
        .unwrap()
        .contains("connect "));
    assert!(battery.subtitle().unwrap().contains("Connect a phone"));
    reconnect.set_active(true);
    pump(1.3);
    assert!(fs::read_to_string(root.join("adb.log"))
        .unwrap()
        .contains("connect 192.0.2.1:5555"));
    reconnect.set_active(false);
    fs::write(root.join("adb.log"), "").unwrap();
    pump(8.3);
    assert!(!fs::read_to_string(root.join("adb.log"))
        .unwrap()
        .contains("connect "));

    // A slow battery command must not block GTK timers or show stale data.
    fs::write(root.join("connected"), "").unwrap();
    let query_deadline = Instant::now() + Duration::from_secs(3);
    while !fs::read_to_string(root.join("adb.log"))
        .unwrap()
        .contains("shell dumpsys battery")
    {
        assert!(
            Instant::now() < query_deadline,
            "battery query did not start"
        );
        pump(0.05);
    }
    fs::remove_file(root.join("connected")).unwrap();
    pump(3.0);
    assert!(
        battery.subtitle().unwrap().contains("Connect a phone"),
        "stale result survived disconnect"
    );
    fs::write(root.join("connected"), "").unwrap();
    let ticks = std::rc::Rc::new(std::cell::Cell::new(0));
    let counter = ticks.clone();
    let timer = gtk4::glib::timeout_add_local(Duration::from_millis(20), move || {
        counter.set(counter.get() + 1);
        gtk4::glib::ControlFlow::Continue
    });
    let reading_deadline = Instant::now() + Duration::from_secs(6);
    while !battery.subtitle().unwrap().contains("83%") {
        assert!(
            Instant::now() < reading_deadline,
            "battery reading unavailable: {:?}",
            battery.subtitle()
        );
        pump(0.05);
    }
    timer.remove();
    assert!(ticks.get() >= 20, "battery query blocked GTK");
    assert!(battery.subtitle().unwrap().contains("83%"));
    assert!(battery
        .subtitle()
        .unwrap()
        .contains("32.7 °C battery temperature"));

    if let Ok(path) = std::env::var("AVYX_GUI_SCREENSHOT") {
        let paintable = gtk4::WidgetPaintable::new(Some(&window));
        let snapshot = gtk4::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(window.width()),
            f64::from(window.height()),
        );
        let node = snapshot.to_node().unwrap();
        window
            .renderer()
            .unwrap()
            .render_texture(&node, None)
            .save_to_png(path)
            .unwrap();
    }

    start.emit_clicked();
    pump(0.2);
    assert_eq!(start.label().as_deref(), Some("Stop Camera"));
    start.emit_clicked();
    reconnect.set_active(true);
    fs::remove_file(root.join("connected")).unwrap();
    fs::write(root.join("adb.log"), "").unwrap();
    pump(9.0);
    assert!(
        !fs::read_to_string(root.join("adb.log"))
            .unwrap()
            .contains("connect "),
        "Stop was lost after Wi-Fi disconnected"
    );
    assert!(battery.subtitle().unwrap().contains("Connect a phone"));
    window.close();
    pump(0.1);
    fs::remove_dir_all(root).unwrap();
}
