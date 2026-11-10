use crate::battery::{query_battery, BatteryStatus};
use crate::config::{load_config, save_config, AppConfig};
use crate::devices::{
    boot_persistence_installed, connect_wifi, disconnect_wifi, ensure_virtual_mic,
    get_device_orientation, install_boot_persistence, list_adb_devices, load_or_reload_v4l2,
    mic_ready, optimal_camera_rotation, probe_cameras, switch_usb_to_wifi,
    try_auto_reconnect_history, v4l2_current_size, video_busy_apps, video_exists, AdbDevice,
};
use crate::qr::{adb_qr_payload, draw_qr_code, pair_and_connect, random_token};
use crate::stream::{
    classify_crash, next_fallback_resolution, ran_long_enough, read_external_pid, read_log,
    start_scrcpy, stop_all_streams, StreamHandle,
};
use adw::prelude::*;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use qrcode::QrCode;
use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

struct AppState {
    cfg: AppConfig,
    devices: Vec<AdbDevice>,
    selected_serial: Option<String>,
    stream: Option<StreamHandle>,
    user_stopped: bool,
    auto_started_usb_serial: Option<String>,
    last_usb_seen: bool,
    probed_serial: Option<String>,
    back_has_high_speed: bool,
    reconnect_at: Option<Instant>,
    last_detected_orientation: Option<u32>,
    last_history_reconnect_attempt: Option<Instant>,
}

type PollCallback = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

const RESOLUTIONS: &[&str] = &["1280x720", "1920x1080", "2560x1440", "640x480"];
const ROTATIONS: &[(&str, &str)] = &[("0°", "0"), ("90°", "90"), ("180°", "180"), ("270°", "270")];
const MIC_MODES: &[(&str, &str)] = &[
    ("Voice (Recommended)", "voice"),
    ("Raw Microphone", "raw"),
    ("Camcorder", "camcorder"),
    ("Off (Camera only)", "off"),
];
const BITRATES: &[(&str, &str)] = &[
    ("Auto", "auto"),
    ("8 Mbps", "8M"),
    ("16 Mbps", "16M"),
    ("24 Mbps", "24M"),
];
const BUFFERS: &[(&str, &str)] = &[
    ("0 ms (Low latency)", "0"),
    ("30 ms", "30"),
    ("50 ms", "50"),
];

pub fn build_ui(app: &adw::Application) {
    let updating_ui = Rc::new(Cell::new(false));
    let state = Rc::new(RefCell::new(AppState {
        cfg: load_config(),
        devices: Vec::new(),
        selected_serial: None,
        stream: None,
        user_stopped: false,
        auto_started_usb_serial: None,
        last_usb_seen: false,
        probed_serial: None,
        back_has_high_speed: true,
        reconnect_at: None,
        last_detected_orientation: None,
        last_history_reconnect_attempt: None,
    }));

    let poll_tick_holder: PollCallback = Rc::new(RefCell::new(None));
    let history_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let reconnect_cancel = Arc::new(AtomicBool::new(false));
    let reconnect_in_flight = Arc::new(AtomicBool::new(false));

    let _ = ensure_virtual_mic();

    let toast_overlay = adw::ToastOverlay::new();
    let stack = adw::ViewStack::new();
    toast_overlay.set_child(Some(&stack));

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();

    let header = adw::HeaderBar::builder().title_widget(&switcher).build();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&toast_overlay));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Avyx-cam")
        .default_width(500)
        .default_height(700)
        .content(&toolbar_view)
        .build();

    // ---------------- TAB 1: CAMERA ----------------
    let cam_page = adw::PreferencesPage::new();
    let status_group = adw::PreferencesGroup::new();

    let status_row = adw::ActionRow::builder()
        .title("Checking device...")
        .subtitle("Plug in phone or switch to Wi-Fi")
        .build();
    let start_stop_btn = gtk4::Button::builder()
        .label("Start Camera")
        .valign(gtk4::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    status_row.add_suffix(&start_stop_btn);
    status_group.add(&status_row);

    let warning_row = adw::ActionRow::builder()
        .title("Device Warning")
        .visible(false)
        .build();
    warning_row.add_css_class("warning");
    let fix_v4l2_inline_btn = gtk4::Button::builder()
        .label("Enable /dev/video0")
        .valign(gtk4::Align::Center)
        .build();
    warning_row.add_suffix(&fix_v4l2_inline_btn);
    status_group.add(&warning_row);
    cam_page.add(&status_group);

    let battery_group = adw::PreferencesGroup::new();
    let battery_row = adw::ActionRow::builder()
        .title("Phone Battery")
        .subtitle("Connect a phone to view battery status")
        .subtitle_lines(2)
        .build();
    battery_group.add(&battery_row);
    cam_page.add(&battery_group);

    let video_group = adw::PreferencesGroup::builder().title("Video").build();

    // Segmented Back / Front lens row
    let lens_row = adw::ActionRow::builder().title("Camera Lens").build();
    let lens_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .valign(gtk4::Align::Center)
        .css_classes(["linked"])
        .build();
    let back_btn = gtk4::ToggleButton::with_label("Back");
    let front_btn = gtk4::ToggleButton::with_label("Front");
    front_btn.set_group(Some(&back_btn));
    if state.borrow().cfg.camera_id == "1" {
        front_btn.set_active(true);
    } else {
        back_btn.set_active(true);
    }
    lens_box.append(&back_btn);
    lens_box.append(&front_btn);
    lens_row.add_suffix(&lens_box);
    video_group.add(&lens_row);

    // Resolution ComboRow
    let res_model = gtk4::StringList::new(RESOLUTIONS);
    let res_row = adw::ComboRow::builder()
        .title("Resolution")
        .model(&res_model)
        .build();
    if let Some(pos) = RESOLUTIONS
        .iter()
        .position(|&r| r == state.borrow().cfg.resolution)
    {
        res_row.set_selected(pos as u32);
    }
    video_group.add(&res_row);

    // FPS ComboRow
    let fps_model = gtk4::StringList::new(&[]);
    let fps_row = adw::ComboRow::builder()
        .title("Frame Rate")
        .model(&fps_model)
        .build();
    video_group.add(&fps_row);

    // Auto-Rotate SwitchRow
    let auto_rotate_row = adw::SwitchRow::builder()
        .title("Auto-Rotate Camera")
        .subtitle("Automatically aligns upright when phone turns")
        .active(state.borrow().cfg.auto_rotate)
        .build();
    video_group.add(&auto_rotate_row);

    // Rotation ComboRow
    let rot_labels: Vec<&str> = ROTATIONS.iter().map(|(l, _)| *l).collect();
    let rot_model = gtk4::StringList::new(&rot_labels);
    let rot_row = adw::ComboRow::builder()
        .title("Rotation")
        .model(&rot_model)
        .build();
    if let Some(pos) = ROTATIONS
        .iter()
        .position(|(_, v)| *v == state.borrow().cfg.rotation)
    {
        rot_row.set_selected(pos as u32);
    }
    rot_row.set_sensitive(!state.borrow().cfg.auto_rotate);
    video_group.add(&rot_row);

    // Flip SwitchRow
    let flip_row = adw::SwitchRow::builder()
        .title("Flip Horizontally")
        .subtitle("Un-mirror front or selfie view")
        .active(state.borrow().cfg.flip)
        .build();
    video_group.add(&flip_row);

    // Zoom Scale Row
    let zoom_row = adw::ActionRow::builder()
        .title("Zoom")
        .subtitle(format!("{:.1}x", state.borrow().cfg.zoom))
        .build();
    let zoom_scale = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, 1.0, 8.0, 0.5);
    zoom_scale.set_width_request(170);
    zoom_scale.set_valign(gtk4::Align::Center);
    zoom_scale.set_value(state.borrow().cfg.zoom);
    zoom_row.add_suffix(&zoom_scale);
    video_group.add(&zoom_row);

    // Torch SwitchRow
    let torch_row = adw::SwitchRow::builder()
        .title("Flashlight (Torch)")
        .subtitle("Back camera LED")
        .active(state.borrow().cfg.torch)
        .build();
    video_group.add(&torch_row);

    cam_page.add(&video_group);

    // Audio & Advanced Stream Group
    let audio_group = adw::PreferencesGroup::builder()
        .title("Audio &amp; Stream")
        .build();

    let mic_labels: Vec<&str> = MIC_MODES.iter().map(|(l, _)| *l).collect();
    let mic_model = gtk4::StringList::new(&mic_labels);
    let mic_row = adw::ComboRow::builder()
        .title("Microphone (PhoneMic)")
        .model(&mic_model)
        .build();
    if let Some(pos) = MIC_MODES
        .iter()
        .position(|(_, v)| *v == state.borrow().cfg.mic_mode)
    {
        mic_row.set_selected(pos as u32);
    }
    let mixer_btn = gtk4::Button::builder()
        .label("Mixer")
        .valign(gtk4::Align::Center)
        .build();
    mic_row.add_suffix(&mixer_btn);
    audio_group.add(&mic_row);

    let preview_row = adw::SwitchRow::builder()
        .title("Show Live Preview Window")
        .subtitle("Turn off for background webcam mode")
        .active(state.borrow().cfg.preview)
        .build();
    audio_group.add(&preview_row);

    let br_labels: Vec<&str> = BITRATES.iter().map(|(l, _)| *l).collect();
    let br_model = gtk4::StringList::new(&br_labels);
    let bitrate_row = adw::ComboRow::builder()
        .title("Video Bitrate")
        .model(&br_model)
        .build();
    if let Some(pos) = BITRATES
        .iter()
        .position(|(_, v)| *v == state.borrow().cfg.bitrate)
    {
        bitrate_row.set_selected(pos as u32);
    }
    audio_group.add(&bitrate_row);

    let buf_labels: Vec<&str> = BUFFERS.iter().map(|(l, _)| *l).collect();
    let buf_model = gtk4::StringList::new(&buf_labels);
    let buffer_row = adw::ComboRow::builder()
        .title("V4L2 Buffer")
        .model(&buf_model)
        .build();
    if let Some(pos) = BUFFERS
        .iter()
        .position(|(_, v)| *v == state.borrow().cfg.v4l2_buffer)
    {
        buffer_row.set_selected(pos as u32);
    }
    audio_group.add(&buffer_row);
    cam_page.add(&audio_group);

    // ---------------- TAB 2: CONNECT ----------------
    let conn_page = adw::PreferencesPage::new();
    let dev_group = adw::PreferencesGroup::builder().title("Devices").build();

    let dev_model = gtk4::StringList::new(&[]);
    let dev_row = adw::ComboRow::builder()
        .title("Active Phone")
        .model(&dev_model)
        .build();
    let refresh_btn = gtk4::Button::builder()
        .icon_name("view-refresh-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Refresh devices")
        .build();
    dev_row.add_suffix(&refresh_btn);
    dev_group.add(&dev_row);

    let wifi_switch_row = adw::ActionRow::builder()
        .title("Wireless Mode")
        .subtitle("Switch a USB-connected phone to Wi-Fi")
        .build();
    let wifi_switch_btn = gtk4::Button::builder()
        .label("Switch to Wi-Fi")
        .valign(gtk4::Align::Center)
        .build();
    wifi_switch_row.add_suffix(&wifi_switch_btn);
    dev_group.add(&wifi_switch_row);
    conn_page.add(&dev_group);

    let history_group = adw::PreferencesGroup::builder()
        .title("Saved Devices")
        .description("Remembered phones for fast reconnect")
        .build();
    conn_page.add(&history_group);

    let wireless_group = adw::PreferencesGroup::builder()
        .title("Wireless Pairing (No USB Cable Needed)")
        .build();

    let qr_row = adw::ActionRow::builder()
        .title("Pair with QR Code")
        .subtitle("Android 11+: Developer options -> Wireless debugging -> Pair with QR")
        .build();
    let qr_btn = gtk4::Button::builder()
        .label("Show QR Code")
        .valign(gtk4::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    qr_row.add_suffix(&qr_btn);
    wireless_group.add(&qr_row);

    let ip_row = adw::EntryRow::builder()
        .title("Wi-Fi IP:Port (e.g. 192.168.10.44:5555)")
        .show_apply_button(true)
        .text(&state.borrow().cfg.wireless_ip)
        .build();
    wireless_group.add(&ip_row);
    conn_page.add(&wireless_group);

    let auto_group = adw::PreferencesGroup::builder()
        .title("Automation &amp; Resilience")
        .build();
    let auto_usb_row = adw::SwitchRow::builder()
        .title("Auto-start on USB Plug-in")
        .subtitle("Start camera automatically when phone connects via USB")
        .active(state.borrow().cfg.auto_start_usb)
        .build();
    let auto_reconn_row = adw::SwitchRow::builder()
        .title("Auto-reconnect &amp; Fallback")
        .subtitle("Restart on drops and step down resolution if a mode fails")
        .active(state.borrow().cfg.auto_reconnect)
        .build();
    auto_group.add(&auto_usb_row);
    auto_group.add(&auto_reconn_row);
    conn_page.add(&auto_group);

    // ---------------- TAB 3: SYSTEM & LOGS ----------------
    let sys_page = adw::PreferencesPage::new();
    let sys_group = adw::PreferencesGroup::builder()
        .title("Virtual Devices")
        .build();

    let v4l2_sys_row = adw::ActionRow::builder()
        .title("Virtual Camera (/dev/video0)")
        .subtitle("AvyxCam")
        .build();
    let reload_v4l2_btn = gtk4::Button::builder()
        .label("Load / Reload")
        .valign(gtk4::Align::Center)
        .build();
    v4l2_sys_row.add_suffix(&reload_v4l2_btn);
    sys_group.add(&v4l2_sys_row);

    let persist_row = adw::ActionRow::builder()
        .title("Load AvyxCam on Boot")
        .subtitle("Avoids password prompt after reboot")
        .build();
    let persist_btn = gtk4::Button::builder()
        .label("Install")
        .valign(gtk4::Align::Center)
        .build();
    persist_row.add_suffix(&persist_btn);
    sys_group.add(&persist_row);

    let mic_sys_row = adw::ActionRow::builder()
        .title("Virtual Microphone (PhoneMic)")
        .subtitle("PipeWire source")
        .build();
    let fix_mic_btn = gtk4::Button::builder()
        .label("Ensure Ready")
        .valign(gtk4::Align::Center)
        .build();
    mic_sys_row.add_suffix(&fix_mic_btn);
    sys_group.add(&mic_sys_row);
    sys_page.add(&sys_group);

    let log_group = adw::PreferencesGroup::builder()
        .title("scrcpy Logs")
        .build();
    let log_view = gtk4::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk4::WrapMode::WordChar)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    let log_scroll = gtk4::ScrolledWindow::builder()
        .min_content_height(220)
        .child(&log_view)
        .css_classes(["card"])
        .build();
    log_group.add(&log_scroll);
    sys_page.add(&log_group);

    stack.add_titled_with_icon(&cam_page, Some("camera"), "Camera", "camera-web-symbolic");
    stack.add_titled_with_icon(
        &conn_page,
        Some("connect"),
        "Connect",
        "network-wireless-symbolic",
    );
    stack.add_titled_with_icon(
        &sys_page,
        Some("system"),
        "System",
        "emblem-system-symbolic",
    );

    // Populate FPS options based on lens
    let refresh_fps_model = {
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let fps_model = fps_model.clone();
        let fps_row = fps_row.clone();
        move || {
            updating_ui.set(true);
            while fps_model.n_items() > 0 {
                fps_model.remove(0);
            }
            let is_back = state.borrow().cfg.camera_id == "0";
            let mut opts: Vec<(&str, &str)> = vec![("30 fps", "30")];
            if is_back && state.borrow().back_has_high_speed {
                opts.push(("120 fps (High-speed)", "120"));
            }
            opts.push(("24 fps", "24"));
            opts.push(("15 fps", "15"));
            for (lbl, _) in &opts {
                fps_model.append(lbl);
            }
            if !is_back && state.borrow().cfg.fps == "120" {
                state.borrow_mut().cfg.fps = "30".to_string();
            }
            let cur_fps = state.borrow().cfg.fps.clone();
            let idx = opts.iter().position(|(_, v)| *v == cur_fps).unwrap_or(0);
            fps_row.set_selected(idx as u32);
            updating_ui.set(false);
        }
    };
    refresh_fps_model();

    let show_toast = {
        let toast_overlay = toast_overlay.clone();
        move |msg: &str| {
            toast_overlay.add_toast(adw::Toast::new(msg));
        }
    };

    // Start / Restart stream helper
    let launch_stream = {
        let state = state.clone();
        let show_toast = show_toast.clone();
        move |override_res: Option<&str>, quick_retried: bool| {
            let (serial, cfg) = {
                let st = state.borrow();
                let Some(sel) = st.selected_serial.clone() else {
                    show_toast("No authorized phone selected.");
                    return;
                };
                let ready = st
                    .devices
                    .iter()
                    .any(|d| d.serial == sel && d.state == "device");
                if !ready {
                    show_toast("Authorize USB debugging on your phone first.");
                    return;
                }
                (sel, st.cfg.clone())
            };

            let busy = video_busy_apps();
            if !busy.is_empty() {
                if let Some(cur_sz) = v4l2_current_size() {
                    let target = override_res.unwrap_or(&cfg.resolution);
                    if cur_sz != target {
                        show_toast(&format!(
                            "Warning: {} holds /dev/video0 at {}",
                            busy.join(", "),
                            cur_sz
                        ));
                    }
                }
            }

            match start_scrcpy(&serial, &cfg, override_res, quick_retried) {
                Ok(handle) => {
                    let mut st = state.borrow_mut();
                    st.stream = Some(handle);
                    st.user_stopped = false;
                    st.reconnect_at = None;
                }
                Err(err) => {
                    show_toast(&err);
                }
            }
        }
    };

    let sync_ui = {
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let status_row = status_row.clone();
        let start_stop_btn = start_stop_btn.clone();
        let warning_row = warning_row.clone();
        let fix_v4l2_inline_btn = fix_v4l2_inline_btn.clone();
        let dev_model = dev_model.clone();
        let dev_row = dev_row.clone();
        let wifi_switch_btn = wifi_switch_btn.clone();
        let v4l2_sys_row = v4l2_sys_row.clone();
        let persist_row = persist_row.clone();
        let persist_btn = persist_btn.clone();
        let mic_sys_row = mic_sys_row.clone();
        let torch_row = torch_row.clone();
        let log_view = log_view.clone();
        let auto_rotate_row = auto_rotate_row.clone();
        let rot_row = rot_row.clone();
        let history_group = history_group.clone();
        let history_rows = history_rows.clone();
        let poll_tick_holder = poll_tick_holder.clone();
        let show_toast = show_toast.clone();
        move || {
            updating_ui.set(true);

            // Update device dropdown
            while dev_model.n_items() > 0 {
                dev_model.remove(0);
            }
            let (devices, selected_serial, stream_active, cfg) = {
                let st = state.borrow();
                (
                    st.devices.clone(),
                    st.selected_serial.clone(),
                    st.stream.is_some(),
                    st.cfg.clone(),
                )
            };

            for d in &devices {
                let kind = if d.is_wifi { "Wi-Fi" } else { "USB" };
                let label = if d.state == "device" {
                    format!("{} ({kind})", d.model)
                } else {
                    format!("{} ({})", d.model, d.state)
                };
                dev_model.append(&label);
            }
            if devices.is_empty() {
                dev_model.append("No phone detected");
                dev_row.set_selected(0);
            } else if let Some(sel) = &selected_serial {
                if let Some(pos) = devices.iter().position(|d| &d.serial == sel) {
                    dev_row.set_selected(pos as u32);
                }
            }

            let cur_dev = selected_serial
                .as_ref()
                .and_then(|s| devices.iter().find(|d| &d.serial == s))
                .cloned();

            let is_live = stream_active || read_external_pid().is_some();
            let v4l_ok = video_exists();
            let busy = if !is_live && v4l_ok {
                video_busy_apps()
            } else {
                Vec::new()
            };

            // Status row + Start/Stop button
            if is_live {
                status_row.set_title("Camera &amp; Mic Live");
                let lens_name = if cfg.camera_id == "1" {
                    "Front"
                } else {
                    "Back"
                };
                let mic_name = if cfg.mic_mode == "off" {
                    "Mic Off"
                } else {
                    "PhoneMic"
                };
                status_row.set_subtitle(&format!(
                    "{lens_name} • {} @ {} fps • {mic_name}",
                    cfg.resolution, cfg.fps
                ));
                start_stop_btn.set_label("Stop Camera");
                start_stop_btn.remove_css_class("suggested-action");
                start_stop_btn.add_css_class("destructive-action");
                start_stop_btn.set_sensitive(true);
            } else if let Some(dev) = &cur_dev {
                if dev.state == "device" {
                    status_row.set_title(&format!("Ready — {}", dev.model));
                    status_row.set_subtitle(if dev.is_wifi {
                        "Connected over Wi-Fi"
                    } else {
                        "Connected over USB"
                    });
                    start_stop_btn.set_sensitive(v4l_ok);
                } else if dev.state == "unauthorized" {
                    status_row.set_title("Authorize Phone");
                    status_row.set_subtitle("Tap 'Allow USB debugging' on your phone screen");
                    start_stop_btn.set_sensitive(false);
                } else {
                    status_row.set_title(&format!("Device {}", dev.state));
                    status_row.set_subtitle(&dev.serial);
                    start_stop_btn.set_sensitive(false);
                }
                start_stop_btn.set_label("Start Camera");
                start_stop_btn.remove_css_class("destructive-action");
                start_stop_btn.add_css_class("suggested-action");
            } else {
                status_row.set_title("No Phone Connected");
                status_row.set_subtitle("Plug in USB or pair over Wi-Fi in Connect tab");
                start_stop_btn.set_label("Start Camera");
                start_stop_btn.remove_css_class("destructive-action");
                start_stop_btn.add_css_class("suggested-action");
                start_stop_btn.set_sensitive(false);
            }

            // Warning banner
            if !v4l_ok {
                warning_row.set_title("/dev/video0 is not loaded");
                warning_row.set_subtitle("Load v4l2loopback to expose AvyxCam");
                fix_v4l2_inline_btn.set_visible(true);
                warning_row.set_visible(true);
            } else if !busy.is_empty() {
                warning_row.set_title(&format!("{} is using /dev/video0", busy.join(", ")));
                warning_row
                    .set_subtitle("Close it there if changing resolution produces a green screen");
                fix_v4l2_inline_btn.set_visible(false);
                warning_row.set_visible(true);
            } else {
                warning_row.set_visible(false);
            }

            // Wi-Fi button state
            if let Some(dev) = &cur_dev {
                wifi_switch_btn.set_sensitive(dev.state == "device");
                wifi_switch_btn.set_label(if dev.is_wifi {
                    "Disconnect Wi-Fi"
                } else {
                    "Switch to Wi-Fi"
                });
            } else {
                wifi_switch_btn.set_sensitive(false);
                wifi_switch_btn.set_label("Switch to Wi-Fi");
            }

            torch_row.set_sensitive(cfg.camera_id == "0");

            // System tab status
            if v4l_ok {
                let fmt = v4l2_current_size().unwrap_or_else(|| "idle".to_string());
                v4l2_sys_row.set_subtitle(&format!("Active ({fmt})"));
            } else {
                v4l2_sys_row.set_subtitle("Not loaded");
            }
            if boot_persistence_installed() {
                persist_row.set_subtitle("Installed (/etc/modules-load.d/avyx-cam.conf)");
                persist_btn.set_label("Reinstall");
            } else {
                persist_row.set_subtitle("Not installed (loads manually after reboot)");
                persist_btn.set_label("Install");
            }
            mic_sys_row.set_subtitle(if mic_ready() {
                "Active (PhoneMic ready)"
            } else {
                "Not loaded"
            });

            // Logs
            let txt = read_log();
            let buf = log_view.buffer();
            let current = buf.text(&buf.start_iter(), &buf.end_iter(), false);
            if current.as_str() != txt {
                buf.set_text(&txt);
            }

            // Sync auto-rotate and rotation state
            auto_rotate_row.set_active(cfg.auto_rotate);
            rot_row.set_sensitive(!cfg.auto_rotate);
            if let Some(pos) = ROTATIONS.iter().position(|(_, v)| *v == cfg.rotation) {
                rot_row.set_selected(pos as u32);
            }
            if cfg.auto_rotate {
                rot_row.set_subtitle("Managed by auto-rotate");
            } else {
                rot_row.set_subtitle("");
            }

            // Rebuild history list
            for row in history_rows.borrow_mut().drain(..) {
                history_group.remove(&row);
            }
            if cfg.history.is_empty() {
                let empty_row = adw::ActionRow::builder()
                    .title("No saved devices yet")
                    .subtitle("Connected phones will be remembered here for easy reconnect")
                    .build();
                history_group.add(&empty_row);
                history_rows.borrow_mut().push(empty_row);
            } else {
                for dev in &cfg.history {
                    let is_active = devices.iter().any(|d| {
                        d.serial == dev.serial
                            || (!dev.address.is_empty() && d.serial == dev.address)
                    });
                    let row = adw::ActionRow::builder()
                        .title(&dev.model)
                        .subtitle(format!(
                            "{} • Last seen {}",
                            if dev.address.is_empty() {
                                &dev.serial
                            } else {
                                &dev.address
                            },
                            dev.last_seen
                        ))
                        .build();

                    if is_active {
                        let active_badge = gtk4::Label::builder()
                            .label("Connected")
                            .valign(gtk4::Align::Center)
                            .css_classes(["success"])
                            .build();
                        row.add_suffix(&active_badge);
                    } else if dev.is_wifi && !dev.address.is_empty() {
                        let conn_btn = gtk4::Button::builder()
                            .label("Connect")
                            .valign(gtk4::Align::Center)
                            .css_classes(["suggested-action"])
                            .build();
                        let target_addr = dev.address.clone();
                        let show_toast = show_toast.clone();
                        let p_holder = poll_tick_holder.clone();
                        conn_btn.connect_clicked(move |_| {
                            let target = target_addr.clone();
                            let show_toast = show_toast.clone();
                            let p_holder = p_holder.clone();
                            let (sender, receiver) = mpsc::channel();
                            thread::spawn(move || {
                                let res = connect_wifi(&target);
                                let _ = sender.send(res);
                            });
                            glib::timeout_add_local(Duration::from_millis(80), move || {
                                if let Ok(res) = receiver.try_recv() {
                                    match res {
                                        Ok(ep) => {
                                            show_toast(&format!("Connected to {ep}"));
                                            if let Some(f) = p_holder.borrow().as_ref() {
                                                f();
                                            }
                                        }
                                        Err(e) => {
                                            show_toast(&format!("Failed: {e}"));
                                        }
                                    }
                                    glib::ControlFlow::Break
                                } else {
                                    glib::ControlFlow::Continue
                                }
                            });
                        });
                        row.add_suffix(&conn_btn);
                    }

                    let forget_btn = gtk4::Button::builder()
                        .icon_name("user-trash-symbolic")
                        .valign(gtk4::Align::Center)
                        .css_classes(["flat"])
                        .tooltip_text("Forget this device")
                        .build();
                    let del_serial = dev.serial.clone();
                    let state = state.clone();
                    let p_holder = poll_tick_holder.clone();
                    forget_btn.connect_clicked(move |_| {
                        state.borrow_mut().cfg.forget_device(&del_serial);
                        save_config(&state.borrow().cfg);
                        if let Some(f) = p_holder.borrow().as_ref() {
                            f();
                        }
                    });
                    row.add_suffix(&forget_btn);

                    history_group.add(&row);
                    history_rows.borrow_mut().push(row);
                }
            }

            updating_ui.set(false);
        }
    };

    // Poll devices, supervise scrcpy, handle auto-start & auto-reconnect
    let poll_tick = {
        let state = state.clone();
        let launch_stream = launch_stream.clone();
        let sync_ui = sync_ui.clone();
        let refresh_fps_model = refresh_fps_model.clone();
        let show_toast = show_toast.clone();
        let reconnect_cancel = reconnect_cancel.clone();
        let reconnect_in_flight = reconnect_in_flight.clone();
        move || {
            let devs = list_adb_devices();
            let mut should_auto_start = false;
            let mut crash_retry: Option<(Option<String>, bool)> = None;
            let mut probe_target: Option<String> = None;

            {
                let mut st = state.borrow_mut();
                st.devices = devs;

                // Record ready devices to history
                let ready_devs: Vec<(String, String, bool)> = st
                    .devices
                    .iter()
                    .filter(|d| d.state == "device")
                    .map(|d| (d.serial.clone(), d.model.clone(), d.is_wifi))
                    .collect();
                for (ser, model, is_wifi) in ready_devs {
                    st.cfg.record_device(
                        &ser,
                        &model,
                        is_wifi,
                        if is_wifi { Some(&ser) } else { None },
                    );
                    save_config(&st.cfg);
                }

                // Phone auto-rotate sync
                if st.cfg.auto_rotate {
                    if let Some(ser) = st.selected_serial.clone() {
                        let is_ready = st
                            .devices
                            .iter()
                            .any(|d| d.serial == ser && d.state == "device");
                        if is_ready {
                            if let Some(deg) = get_device_orientation(&ser) {
                                let prev = st.last_detected_orientation;
                                if prev != Some(deg) {
                                    st.last_detected_orientation = Some(deg);
                                    let opt_rot = optimal_camera_rotation(&st.cfg.camera_id, deg);
                                    if st.cfg.rotation != opt_rot {
                                        st.cfg.rotation = opt_rot.to_string();
                                        save_config(&st.cfg);
                                        if st.stream.is_some() {
                                            crash_retry = Some((None, false));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Auto-reconnect to remembered devices if idle
                let has_ready_device = st.devices.iter().any(|d| d.state == "device");
                if st.user_stopped
                    || !st.cfg.auto_reconnect
                    || has_ready_device
                    || st.stream.is_some()
                {
                    reconnect_cancel.store(true, Ordering::Relaxed);
                } else {
                    reconnect_cancel.store(false, Ordering::Relaxed);
                }
                if st.cfg.auto_reconnect
                    && !has_ready_device
                    && st.stream.is_none()
                    && !st.user_stopped
                    && !reconnect_in_flight.load(Ordering::Relaxed)
                {
                    let now = Instant::now();
                    let should_check = match st.last_history_reconnect_attempt {
                        None => true,
                        Some(t) => now.duration_since(t) > Duration::from_secs(8),
                    };
                    if should_check {
                        st.last_history_reconnect_attempt = Some(now);
                        reconnect_in_flight.store(true, Ordering::Relaxed);
                        let hist = st.cfg.history.clone();
                        let cancel = reconnect_cancel.clone();
                        let in_flight = reconnect_in_flight.clone();
                        thread::spawn(move || {
                            let _ = try_auto_reconnect_history(&hist, || {
                                !cancel.load(Ordering::Relaxed)
                            });
                            in_flight.store(false, Ordering::Relaxed);
                        });
                    }
                }

                // Keep selected_serial valid or pick best ready device
                let valid = st
                    .selected_serial
                    .as_ref()
                    .is_some_and(|s| st.devices.iter().any(|d| &d.serial == s));
                if !valid {
                    st.selected_serial = st
                        .devices
                        .iter()
                        .find(|d| d.state == "device")
                        .or_else(|| st.devices.first())
                        .map(|d| d.serial.clone());
                }

                // Probe cameras once per connected phone
                if let Some(sel) = st.selected_serial.clone() {
                    if st.probed_serial.as_deref() != Some(sel.as_str())
                        && st
                            .devices
                            .iter()
                            .any(|d| d.serial == sel && d.state == "device")
                    {
                        st.probed_serial = Some(sel.clone());
                        probe_target = Some(sel);
                    }
                }

                // USB auto-start edge detection
                let usb_ready = st
                    .devices
                    .iter()
                    .find(|d| !d.is_wifi && d.state == "device")
                    .map(|d| d.serial.clone());
                if let Some(usb_ser) = usb_ready {
                    if !st.last_usb_seen
                        && st.cfg.auto_start_usb
                        && st.stream.is_none()
                        && read_external_pid().is_none()
                        && !st.user_stopped
                        && video_exists()
                    {
                        st.selected_serial = Some(usb_ser.clone());
                        st.auto_started_usb_serial = Some(usb_ser);
                        should_auto_start = true;
                    }
                    st.last_usb_seen = true;
                } else {
                    st.last_usb_seen = false;
                }

                // Supervise active stream
                let mut crashed_info: Option<(Instant, String, bool)> = None;
                if let Some(handle) = st.stream.as_mut() {
                    if let Ok(Some(status)) = handle.child.try_wait() {
                        let started = handle.started_at;
                        let res = handle.resolution.clone();
                        let retried = handle.quick_retried;
                        st.stream = None;
                        if !status.success() && !st.user_stopped {
                            crashed_info = Some((started, res, retried));
                        }
                    }
                }

                if let Some((started, res, retried)) = crashed_info {
                    let log = read_log();
                    if let Some(reason) = classify_crash(&log) {
                        show_toast(reason);
                    } else if st.cfg.auto_reconnect {
                        if ran_long_enough(started) || !retried {
                            crash_retry = Some((Some(res), true));
                        } else if let Some(fb) = next_fallback_resolution(&res) {
                            show_toast(&format!("Falling back to {fb}"));
                            st.cfg.resolution = fb.to_string();
                            save_config(&st.cfg);
                            crash_retry = Some((Some(fb.to_string()), false));
                        } else {
                            show_toast("Stream failed. Check System -> Logs.");
                        }
                    }
                }

                if let Some(at) = st.reconnect_at {
                    if st.cfg.auto_reconnect
                        && Instant::now() >= at
                        && st.stream.is_none()
                        && !st.user_stopped
                    {
                        st.reconnect_at = None;
                        crash_retry = Some((None, false));
                    }
                }
            }

            if let Some(ser) = probe_target {
                let cams = probe_cameras(&ser);
                if let Some(back) = cams.iter().find(|c| c.id == "0") {
                    state.borrow_mut().back_has_high_speed = back.has_high_speed;
                    refresh_fps_model();
                }
            }

            if should_auto_start {
                show_toast("USB phone detected — starting camera");
                launch_stream(None, false);
            } else if let Some((res, retried)) = crash_retry {
                launch_stream(res.as_deref(), retried);
            }

            sync_ui();
        }
    };

    *poll_tick_holder.borrow_mut() = Some(Rc::new(poll_tick.clone()));

    poll_tick();

    glib::timeout_add_local(Duration::from_millis(1500), {
        let poll_tick = poll_tick.clone();
        move || {
            poll_tick();
            glib::ControlFlow::Continue
        }
    });

    // Battery queries have a separate cadence and never run on the GTK thread.
    let mut battery_selection: Option<String> = None;
    let mut battery_generation = 0_u64;
    let mut last_battery_query: Option<Instant> = None;
    let mut pending_battery: Option<(u64, mpsc::Receiver<Result<BatteryStatus, String>>)> = None;
    glib::timeout_add_local(Duration::from_millis(250), {
        let state = state.clone();
        let window = window.downgrade();
        move || {
            if window.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            let selected = {
                let st = state.borrow();
                st.selected_serial
                    .as_ref()
                    .filter(|serial| {
                        st.devices
                            .iter()
                            .any(|d| &d.serial == *serial && d.state == "device")
                    })
                    .cloned()
            };
            if selected != battery_selection {
                battery_selection = selected;
                battery_generation = battery_generation.wrapping_add(1);
                last_battery_query = None;
                battery_row.set_subtitle(if battery_selection.is_some() {
                    "Reading phone battery…"
                } else {
                    "Connect a phone to view battery status"
                });
            }
            if let Some((generation, receiver)) = &pending_battery {
                match receiver.try_recv() {
                    Ok(result) => {
                        if *generation == battery_generation {
                            match result {
                                Ok(status) => battery_row.set_subtitle(&status.subtitle()),
                                Err(_) => battery_row.set_subtitle(
                                    "Battery data unavailable; retrying automatically",
                                ),
                            }
                        }
                        pending_battery = None;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if *generation == battery_generation {
                            battery_row
                                .set_subtitle("Battery data unavailable; retrying automatically");
                        }
                        pending_battery = None;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if pending_battery.is_none()
                && last_battery_query.is_none_or(|at| at.elapsed() >= Duration::from_secs(20))
            {
                if let Some(serial) = battery_selection.clone() {
                    let (sender, receiver) = mpsc::channel();
                    pending_battery = Some((battery_generation, receiver));
                    last_battery_query = Some(Instant::now());
                    thread::spawn(move || {
                        let _ = sender.send(query_battery(&serial));
                    });
                }
            }
            glib::ControlFlow::Continue
        }
    });

    // Helper: save settings & restart if currently live
    let apply_setting_change = {
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let launch_stream = launch_stream.clone();
        let sync_ui = sync_ui.clone();
        move || {
            {
                if updating_ui.get() {
                    return;
                }
                save_config(&state.borrow().cfg);
            }
            let live = state.borrow().stream.is_some() || read_external_pid().is_some();
            if live {
                launch_stream(None, false);
            }
            sync_ui();
        }
    };

    // Start / Stop button
    start_stop_btn.connect_clicked({
        let state = state.clone();
        let launch_stream = launch_stream.clone();
        let sync_ui = sync_ui.clone();
        let reconnect_cancel = reconnect_cancel.clone();
        move |_| {
            let live = state.borrow().stream.is_some() || read_external_pid().is_some();
            if live {
                reconnect_cancel.store(true, Ordering::Relaxed);
                {
                    let mut st = state.borrow_mut();
                    st.user_stopped = true;
                    st.reconnect_at = None;
                    if let Some(mut h) = st.stream.take() {
                        let _ = h.child.kill();
                        let _ = h.child.wait();
                    }
                }
                stop_all_streams();
                sync_ui();
            } else {
                launch_stream(None, false);
                sync_ui();
            }
        }
    });

    // Lens segmented buttons
    back_btn.connect_toggled({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let refresh_fps_model = refresh_fps_model.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |btn| {
            if !btn.is_active() || updating_ui.get() {
                return;
            }
            state.borrow_mut().cfg.camera_id = "0".to_string();
            refresh_fps_model();
            apply_setting_change();
        }
    });
    front_btn.connect_toggled({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let refresh_fps_model = refresh_fps_model.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |btn| {
            if !btn.is_active() || updating_ui.get() {
                return;
            }
            state.borrow_mut().cfg.camera_id = "1".to_string();
            refresh_fps_model();
            apply_setting_change();
        }
    });

    res_row.connect_selected_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            if let Some(&val) = RESOLUTIONS.get(row.selected() as usize) {
                state.borrow_mut().cfg.resolution = val.to_string();
                apply_setting_change();
            }
        }
    });

    fps_row.connect_selected_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            let is_back = state.borrow().cfg.camera_id == "0" && state.borrow().back_has_high_speed;
            let mut vals = vec!["30"];
            if is_back {
                vals.push("120");
            }
            vals.extend(["24", "15"]);
            if let Some(&val) = vals.get(row.selected() as usize) {
                state.borrow_mut().cfg.fps = val.to_string();
                apply_setting_change();
            }
        }
    });

    rot_row.connect_selected_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            if let Some(&(_, val)) = ROTATIONS.get(row.selected() as usize) {
                state.borrow_mut().cfg.rotation = val.to_string();
                apply_setting_change();
            }
        }
    });

    auto_rotate_row.connect_active_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let rot_row = rot_row.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            let active = row.is_active();
            state.borrow_mut().cfg.auto_rotate = active;
            rot_row.set_sensitive(!active);
            apply_setting_change();
        }
    });

    flip_row.connect_active_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            state.borrow_mut().cfg.flip = row.is_active();
            apply_setting_change();
        }
    });

    zoom_scale.connect_value_changed({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let zoom_row = zoom_row.clone();
        move |sc| {
            let v = (sc.value() * 2.0).round() / 2.0;
            zoom_row.set_subtitle(&format!("{v:.1}x"));
            if !updating_ui.get() {
                state.borrow_mut().cfg.zoom = v;
                save_config(&state.borrow().cfg);
            }
        }
    });
    // Restart on zoom drag release / click if live
    let zoom_gesture = gtk4::GestureClick::new();
    zoom_gesture.connect_released({
        let apply_setting_change = apply_setting_change.clone();
        move |_, _, _, _| {
            apply_setting_change();
        }
    });
    zoom_scale.add_controller(zoom_gesture);

    torch_row.connect_active_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            state.borrow_mut().cfg.torch = row.is_active();
            apply_setting_change();
        }
    });

    mic_row.connect_selected_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            if let Some(&(_, val)) = MIC_MODES.get(row.selected() as usize) {
                state.borrow_mut().cfg.mic_mode = val.to_string();
                apply_setting_change();
            }
        }
    });

    mixer_btn.connect_clicked(|_| {
        let _ = Command::new("pavucontrol").spawn();
    });

    preview_row.connect_active_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            state.borrow_mut().cfg.preview = row.is_active();
            apply_setting_change();
        }
    });

    bitrate_row.connect_selected_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            if let Some(&(_, val)) = BITRATES.get(row.selected() as usize) {
                state.borrow_mut().cfg.bitrate = val.to_string();
                apply_setting_change();
            }
        }
    });

    buffer_row.connect_selected_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            if let Some(&(_, val)) = BUFFERS.get(row.selected() as usize) {
                state.borrow_mut().cfg.v4l2_buffer = val.to_string();
                apply_setting_change();
            }
        }
    });

    // Connect tab handlers
    dev_row.connect_selected_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let sync_ui = sync_ui.clone();
        move |row| {
            if updating_ui.get() {
                return;
            }
            let idx = row.selected() as usize;
            let serial = state.borrow().devices.get(idx).map(|d| d.serial.clone());
            if let Some(s) = serial {
                state.borrow_mut().selected_serial = Some(s);
                sync_ui();
            }
        }
    });

    refresh_btn.connect_clicked({
        let poll_tick = poll_tick.clone();
        move |_| poll_tick()
    });

    wifi_switch_btn.connect_clicked({
        let state = state.clone();
        let poll_tick = poll_tick.clone();
        let show_toast = show_toast.clone();
        let ip_row = ip_row.clone();
        move |_| {
            let sel = {
                let st = state.borrow();
                st.selected_serial
                    .as_ref()
                    .and_then(|s| st.devices.iter().find(|d| &d.serial == s))
                    .cloned()
            };
            let Some(dev) = sel else { return };
            if dev.is_wifi {
                match disconnect_wifi(&dev.serial) {
                    Ok(()) => show_toast("Disconnected Wi-Fi device"),
                    Err(e) => show_toast(&e),
                }
                poll_tick();
            } else {
                show_toast("Switching phone to Wi-Fi...");
                match switch_usb_to_wifi(&dev.serial, 5555) {
                    Ok(ep) => {
                        {
                            let mut st = state.borrow_mut();
                            st.cfg.wireless_ip = ep.clone();
                            st.selected_serial = Some(ep.clone());
                            save_config(&st.cfg);
                        }
                        ip_row.set_text(&ep);
                        show_toast(&format!("Wi-Fi connected ({ep}) - safe to unplug USB"));
                    }
                    Err(e) => show_toast(&e),
                }
                poll_tick();
            }
        }
    });

    ip_row.connect_apply({
        let state = state.clone();
        let poll_tick = poll_tick.clone();
        let show_toast = show_toast.clone();
        move |entry| {
            let txt = entry.text().to_string();
            if txt.trim().is_empty() {
                return;
            }
            match connect_wifi(&txt) {
                Ok(ep) => {
                    {
                        let mut st = state.borrow_mut();
                        st.cfg.wireless_ip = ep.clone();
                        st.selected_serial = Some(ep.clone());
                        save_config(&st.cfg);
                    }
                    show_toast(&format!("Connected to {ep}"));
                    poll_tick();
                }
                Err(e) => show_toast(&e),
            }
        }
    });

    auto_usb_row.connect_active_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        move |row| {
            if !updating_ui.get() {
                state.borrow_mut().cfg.auto_start_usb = row.is_active();
                save_config(&state.borrow().cfg);
            }
        }
    });

    auto_reconn_row.connect_active_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let reconnect_cancel = reconnect_cancel.clone();
        move |row| {
            if !updating_ui.get() {
                let active = row.is_active();
                if !active {
                    reconnect_cancel.store(true, Ordering::Relaxed);
                }
                state.borrow_mut().cfg.auto_reconnect = active;
                save_config(&state.borrow().cfg);
            }
        }
    });

    // QR Code pairing dialog
    qr_btn.connect_clicked({
        let window = window.clone();
        let state = state.clone();
        let poll_tick = poll_tick.clone();
        let show_toast = show_toast.clone();
        let ip_row = ip_row.clone();
        move |_| {
            let name = format!("avyx-cam-{}", random_token(6));
            let password = random_token(10);
            let payload = adb_qr_payload(&name, &password);
            let code = QrCode::new(payload).ok();

            let dialog = adw::Dialog::builder()
                .title("Pair Wireless Debugging")
                .content_width(340)
                .build();

            let vbox = gtk4::Box::builder()
                .orientation(gtk4::Orientation::Vertical)
                .spacing(12)
                .margin_top(16)
                .margin_bottom(16)
                .margin_start(16)
                .margin_end(16)
                .build();

            let hint = gtk4::Label::builder()
                .label(
                    "On phone: Developer options -> Wireless debugging -> Pair device with QR code",
                )
                .wrap(true)
                .justify(gtk4::Justification::Center)
                .build();

            let qr_area = gtk4::DrawingArea::builder()
                .content_width(240)
                .content_height(240)
                .halign(gtk4::Align::Center)
                .build();
            qr_area.set_draw_func(move |_, cr, w, h| {
                draw_qr_code(cr, w, h, code.as_ref());
            });

            let status_lbl = gtk4::Label::builder()
                .label("Starting discovery...")
                .wrap(true)
                .justify(gtk4::Justification::Center)
                .build();

            let close_btn = gtk4::Button::builder()
                .label("Cancel")
                .halign(gtk4::Align::Center)
                .build();

            vbox.append(&hint);
            vbox.append(&qr_area);
            vbox.append(&status_lbl);
            vbox.append(&close_btn);

            let dlg_header = adw::HeaderBar::new();
            let dlg_view = adw::ToolbarView::new();
            dlg_view.add_top_bar(&dlg_header);
            dlg_view.set_content(Some(&vbox));
            dialog.set_child(Some(&dlg_view));

            let cancel = Arc::new(AtomicBool::new(false));
            close_btn.connect_clicked({
                let dialog = dialog.clone();
                let cancel = cancel.clone();
                move |_| {
                    cancel.store(true, Ordering::Relaxed);
                    dialog.close();
                }
            });
            dialog.connect_closed({
                let cancel = cancel.clone();
                move |_| cancel.store(true, Ordering::Relaxed)
            });

            let (tx, rx) = mpsc::channel::<(String, Option<Result<String, String>>)>();
            let cancel_bg = cancel.clone();
            thread::spawn(move || {
                let res = pair_and_connect(name, password, cancel_bg, |msg| {
                    let _ = tx.send((msg.to_string(), None));
                });
                let _ = tx.send(("done".to_string(), Some(res)));
            });

            glib::timeout_add_local(Duration::from_millis(120), {
                let dialog = dialog.clone();
                let status_lbl = status_lbl.clone();
                let state = state.clone();
                let poll_tick = poll_tick.clone();
                let show_toast = show_toast.clone();
                let ip_row = ip_row.clone();
                move || {
                    while let Ok((msg, done)) = rx.try_recv() {
                        if let Some(res) = done {
                            match res {
                                Ok(addr) => {
                                    {
                                        let mut st = state.borrow_mut();
                                        st.cfg.wireless_ip = addr.clone();
                                        st.selected_serial = Some(addr.clone());
                                        save_config(&st.cfg);
                                    }
                                    ip_row.set_text(&addr);
                                    show_toast(&format!("Paired & connected to {addr}"));
                                    poll_tick();
                                    dialog.close();
                                }
                                Err(e) => {
                                    if e != "Cancelled" {
                                        status_lbl.set_text(&e);
                                    }
                                }
                            }
                            return glib::ControlFlow::Break;
                        } else {
                            status_lbl.set_text(&msg);
                        }
                    }
                    if cancel.load(Ordering::Relaxed) {
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                }
            });

            dialog.present(Some(&window));
        }
    });

    // System tab buttons
    let handle_v4l2 = {
        let sync_ui = sync_ui.clone();
        let show_toast = show_toast.clone();
        move |reload: bool| {
            match load_or_reload_v4l2(reload) {
                Ok(()) => show_toast("AvyxCam (/dev/video0) ready"),
                Err(e) => show_toast(&e),
            }
            sync_ui();
        }
    };
    fix_v4l2_inline_btn.connect_clicked({
        let handle_v4l2 = handle_v4l2.clone();
        move |_| handle_v4l2(false)
    });
    reload_v4l2_btn.connect_clicked({
        let handle_v4l2 = handle_v4l2.clone();
        move |_| handle_v4l2(true)
    });

    persist_btn.connect_clicked({
        let sync_ui = sync_ui.clone();
        let show_toast = show_toast.clone();
        move |_| {
            match install_boot_persistence() {
                Ok(()) => show_toast("Boot persistence installed"),
                Err(e) => show_toast(&e),
            }
            sync_ui();
        }
    });

    fix_mic_btn.connect_clicked({
        let sync_ui = sync_ui.clone();
        let show_toast = show_toast.clone();
        move |_| {
            match ensure_virtual_mic() {
                Ok(()) => show_toast("PhoneMic ready"),
                Err(e) => show_toast(&e),
            }
            sync_ui();
        }
    });

    // Cleanly handle window close (keep background stream running if preview is off? Stop if preview is on or user wants)
    window.connect_close_request({
        let state = state.clone();
        move |_| {
            let _ = state.borrow();
            glib::Propagation::Proceed
        }
    });

    // Unused gio import warning prevention
    let _ = gio::ApplicationFlags::empty();

    window.present();
}
