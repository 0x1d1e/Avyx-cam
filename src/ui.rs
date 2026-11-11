use crate::battery::{query_battery, BatteryStatus};
use crate::config::{load_config, AppConfig};
use crate::devices::{
    boot_persistence_installed, connect_wifi, disconnect_wifi, ensure_virtual_mic,
    install_boot_persistence, load_or_reload_v4l2, mic_ready, switch_usb_to_wifi,
    v4l2_current_size, video_busy_apps, video_exists, AdbDevice,
};
use crate::qr::{adb_qr_payload, draw_qr_code, pair_and_connect, random_token};
use crate::session::{self, Request, Snapshot};
use crate::stream::read_log;
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
    session: Snapshot,
    system: SystemStatus,
    back_has_high_speed: bool,
}

#[derive(Default, PartialEq, Eq)]
struct SystemStatus {
    video: bool,
    busy: Vec<String>,
    format: Option<String>,
    persistent: bool,
    mic: bool,
    log: String,
}

// One FIFO preserves intent order. Poll replies carry the last processed UI revision,
// so an older reply cannot overwrite a newer click or settings change.
struct SessionClient {
    tx: mpsc::Sender<(u64, Request)>,
    revision: Cell<u64>,
}
impl SessionClient {
    fn send(&self, request: Request) {
        let revision = self.revision.get().wrapping_add(1);
        self.revision.set(revision);
        let _ = self.tx.send((revision, request));
    }
}

fn background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(work());
    });
    let mut done = Some(done);
    glib::timeout_add_local(Duration::from_millis(80), move || match rx.try_recv() {
        Ok(result) => {
            done.take().unwrap()(result);
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
    });
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
        session: Snapshot::default(),
        system: SystemStatus::default(),
        back_has_high_speed: true,
    }));

    let poll_tick_holder: PollCallback = Rc::new(RefCell::new(None));
    let history_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let closed = Arc::new(AtomicBool::new(false));
    let (command_tx, command_rx) = mpsc::channel();
    let (session_tx, session_rx) = mpsc::channel();
    let client = Rc::new(SessionClient {
        tx: command_tx,
        revision: Cell::new(0),
    });
    thread::spawn({
        let closed = closed.clone();
        move || {
            let mut revision = 0;
            while !closed.load(Ordering::Relaxed) {
                let request = match command_rx.recv_timeout(Duration::from_millis(300)) {
                    Ok((next, request)) => {
                        revision = next;
                        request
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => Request::Status,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if session_tx
                    .send((revision, session::request(request)))
                    .is_err()
                {
                    break;
                }
            }
        }
    });

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

    let launch_stream = {
        let state = state.clone();
        let client = client.clone();
        move || {
            let mut st = state.borrow_mut();
            client.send(Request::Start {
                serial: st.selected_serial.clone(),
                cfg: st.cfg.clone(),
            });
            st.session.desired = true;
            st.session.phase = "starting".into();
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
        let back_btn = back_btn.clone();
        let front_btn = front_btn.clone();
        let res_row = res_row.clone();
        let flip_row = flip_row.clone();
        let zoom_scale = zoom_scale.clone();
        let torch_row_sync = torch_row.clone();
        let mic_row = mic_row.clone();
        let preview_row = preview_row.clone();
        let bitrate_row = bitrate_row.clone();
        let buffer_row = buffer_row.clone();
        let auto_usb_row = auto_usb_row.clone();
        let auto_reconn_row = auto_reconn_row.clone();
        let client = client.clone();
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
                    st.session.pid.is_some(),
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

            let st = state.borrow();
            let is_live = stream_active;
            let v4l_ok = st.system.video;
            let busy = st.system.busy.clone();
            let phase = st.session.phase.clone();
            let desired = st.session.desired;
            let error = st.session.error.clone();
            drop(st);

            // Status row + Start/Stop button
            if desired && !is_live {
                status_row.set_title(match phase.as_str() {
                    "starting" => "Starting Camera",
                    "waiting" => "Waiting to Reconnect",
                    _ => "Camera Needs Attention",
                });
                status_row.set_subtitle(error.as_deref().unwrap_or("Preparing camera session"));
                start_stop_btn.set_label("Stop Camera");
                start_stop_btn.remove_css_class("suggested-action");
                start_stop_btn.add_css_class("destructive-action");
                start_stop_btn.set_sensitive(true);
            } else if phase == "stopping" {
                status_row.set_title("Stopping Camera");
                status_row.set_subtitle("Cancelling pending starts and reconnects");
                start_stop_btn.set_label("Stopping…");
                start_stop_btn.set_sensitive(false);
            } else if is_live {
                status_row.set_title("Camera Running");
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
                    status_row.set_title(&format!("Ready - {}", dev.model));
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
                let fmt = state
                    .borrow()
                    .system
                    .format
                    .clone()
                    .unwrap_or_else(|| "idle".to_string());
                v4l2_sys_row.set_subtitle(&format!("Active ({fmt})"));
            } else {
                v4l2_sys_row.set_subtitle("Not loaded");
            }
            if state.borrow().system.persistent {
                persist_row.set_subtitle("Installed (/etc/modules-load.d/avyx-cam.conf)");
                persist_btn.set_label("Reinstall");
            } else {
                persist_row.set_subtitle("Not installed (loads manually after reboot)");
                persist_btn.set_label("Install");
            }
            mic_sys_row.set_subtitle(if state.borrow().system.mic {
                "Active (PhoneMic ready)"
            } else {
                "Not loaded"
            });

            // Logs
            let txt = state.borrow().system.log.clone();
            let buf = log_view.buffer();
            let current = buf.text(&buf.start_iter(), &buf.end_iter(), false);
            if current.as_str() != txt {
                buf.set_text(&txt);
            }

            // Apply the owner's settings when another client changes the session.
            back_btn.set_active(cfg.camera_id != "1");
            front_btn.set_active(cfg.camera_id == "1");
            if let Some(pos) = RESOLUTIONS
                .iter()
                .position(|&value| value == cfg.resolution)
            {
                res_row.set_selected(pos as u32);
            }
            flip_row.set_active(cfg.flip);
            zoom_scale.set_value(cfg.zoom);
            torch_row_sync.set_active(cfg.torch);
            preview_row.set_active(cfg.preview);
            auto_usb_row.set_active(cfg.auto_start_usb);
            auto_reconn_row.set_active(cfg.auto_reconnect);
            for (row, values, current) in [
                (&mic_row, MIC_MODES, cfg.mic_mode.as_str()),
                (&bitrate_row, BITRATES, cfg.bitrate.as_str()),
                (&buffer_row, BUFFERS, cfg.v4l2_buffer.as_str()),
            ] {
                if let Some(pos) = values.iter().position(|(_, value)| *value == current) {
                    row.set_selected(pos as u32);
                }
            }
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
                    let client = client.clone();
                    forget_btn.connect_clicked(move |_| {
                        let mut st = state.borrow_mut();
                        st.cfg.forget_device(&del_serial);
                        client.send(Request::Configure {
                            cfg: st.cfg.clone(),
                        });
                    });
                    row.add_suffix(&forget_btn);

                    history_group.add(&row);
                    history_rows.borrow_mut().push(row);
                }
            }

            updating_ui.set(false);
        }
    };

    // Refresh asks the owner; GTK never discovers devices or supervises children.
    let poll_tick = {
        let client = client.clone();
        move || client.send(Request::Status)
    };
    *poll_tick_holder.borrow_mut() = Some(Rc::new(poll_tick.clone()));

    let (system_tx, system_rx) = mpsc::channel();
    thread::spawn({
        let closed = closed.clone();
        move || {
            while !closed.load(Ordering::Relaxed) {
                let status = SystemStatus {
                    video: video_exists(),
                    busy: video_busy_apps(),
                    format: v4l2_current_size(),
                    persistent: boot_persistence_installed(),
                    mic: mic_ready(),
                    log: read_log(),
                };
                if system_tx.send(status).is_err() {
                    break;
                }
                for _ in 0..20 {
                    if closed.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });

    glib::timeout_add_local(Duration::from_millis(80), {
        let state = state.clone();
        let client = client.clone();
        let sync_ui = sync_ui.clone();
        let refresh_fps_model = refresh_fps_model.clone();
        let show_toast = show_toast.clone();
        let window = window.downgrade();
        let closed = closed.clone();
        move || {
            if closed.load(Ordering::Relaxed) || window.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            let mut changed = false;
            while let Ok((revision, result)) = session_rx.try_recv() {
                if revision != client.revision.get() {
                    continue;
                }
                match result {
                    Ok(snapshot) => {
                        let mut st = state.borrow_mut();
                        if st.session == snapshot {
                            continue;
                        }
                        st.devices = snapshot.devices.clone();
                        let valid = st
                            .selected_serial
                            .as_ref()
                            .is_some_and(|serial| st.devices.iter().any(|d| &d.serial == serial));
                        if !valid {
                            st.selected_serial = snapshot.serial.clone().or_else(|| {
                                st.devices
                                    .iter()
                                    .find(|d| d.state == "device")
                                    .or_else(|| st.devices.first())
                                    .map(|d| d.serial.clone())
                            });
                        }
                        st.back_has_high_speed = snapshot
                            .cameras
                            .iter()
                            .find(|c| c.id == "0")
                            .is_none_or(|c| c.has_high_speed);
                        st.cfg = snapshot.cfg.clone();
                        st.session = snapshot;
                        changed = true;
                    }
                    Err(error) => {
                        show_toast(&error);
                        let mut st = state.borrow_mut();
                        st.session.phase = "failed".into();
                        st.session.error = Some(error);
                        changed = true;
                    }
                }
            }
            while let Ok(system) = system_rx.try_recv() {
                let mut st = state.borrow_mut();
                if st.system != system {
                    st.system = system;
                    changed = true;
                }
            }
            if changed {
                refresh_fps_model();
                sync_ui();
            }
            glib::ControlFlow::Continue
        }
    });
    sync_ui();

    // Battery queries have a separate cadence and never run on the GTK thread.
    let mut battery_selection: Option<String> = None;
    let mut battery_generation = 0_u64;
    let mut last_battery_query: Option<Instant> = None;
    let mut pending_battery: Option<(u64, mpsc::Receiver<Result<BatteryStatus, String>>)> = None;
    glib::timeout_add_local(Duration::from_millis(250), {
        let state = state.clone();
        let window = window.downgrade();
        let closed = closed.clone();
        move || {
            if closed.load(Ordering::Relaxed) || window.upgrade().is_none() {
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
        let client = client.clone();
        let sync_ui = sync_ui.clone();
        move || {
            if updating_ui.get() {
                return;
            }
            client.send(Request::Configure {
                cfg: state.borrow().cfg.clone(),
            });
            sync_ui();
        }
    };

    // Start / Stop button
    start_stop_btn.connect_clicked({
        let state = state.clone();
        let launch_stream = launch_stream.clone();
        let sync_ui = sync_ui.clone();
        let client = client.clone();
        move |_| {
            let active = {
                let st = state.borrow();
                st.session.desired || st.session.pid.is_some()
            };
            if active {
                client.send(Request::Stop);
                let mut st = state.borrow_mut();
                st.session.desired = false;
                st.session.phase = "stopping".into();
            } else {
                launch_stream();
            }
            sync_ui();
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
        let apply_setting_change = apply_setting_change.clone();
        let pending = Rc::new(RefCell::new(None::<glib::SourceId>));
        move |sc| {
            let v = (sc.value() * 2.0).round() / 2.0;
            zoom_row.set_subtitle(&format!("{v:.1}x"));
            if !updating_ui.get() {
                state.borrow_mut().cfg.zoom = v;
                if let Some(timer) = pending.borrow_mut().take() {
                    timer.remove();
                }
                let apply = apply_setting_change.clone();
                let finished = pending.clone();
                *pending.borrow_mut() = Some(glib::timeout_add_local_once(
                    Duration::from_millis(250),
                    move || {
                        finished.borrow_mut().take();
                        apply();
                    },
                ));
            }
        }
    });

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
        let client = client.clone();
        move |_| {
            let sel = {
                let st = state.borrow();
                st.selected_serial
                    .as_ref()
                    .and_then(|s| st.devices.iter().find(|d| &d.serial == s))
                    .cloned()
            };
            let Some(dev) = sel else { return };
            let state = state.clone();
            let client = client.clone();
            let ip_row = ip_row.clone();
            let show_toast = show_toast.clone();
            let poll_tick = poll_tick.clone();
            let was_wifi = dev.is_wifi;
            show_toast(if was_wifi {
                "Disconnecting Wi-Fi..."
            } else {
                "Switching phone to Wi-Fi..."
            });
            background(
                move || {
                    if was_wifi {
                        disconnect_wifi(&dev.serial).map(|_| None)
                    } else {
                        switch_usb_to_wifi(&dev.serial, 5555).map(Some)
                    }
                },
                move |result| {
                    match result {
                        Ok(Some(ep)) => {
                            let mut st = state.borrow_mut();
                            st.cfg.wireless_ip = ep.clone();
                            st.selected_serial = Some(ep.clone());
                            client.send(Request::Configure {
                                cfg: st.cfg.clone(),
                            });
                            ip_row.set_text(&ep);
                            show_toast(&format!("Wi-Fi connected ({ep}) - safe to unplug USB"));
                        }
                        Ok(None) => show_toast("Disconnected Wi-Fi device"),
                        Err(e) => show_toast(&e),
                    }
                    poll_tick();
                },
            );
        }
    });

    ip_row.connect_apply({
        let state = state.clone();
        let poll_tick = poll_tick.clone();
        let show_toast = show_toast.clone();
        let client = client.clone();
        move |entry| {
            let txt = entry.text().to_string();
            if txt.trim().is_empty() {
                return;
            }
            let state = state.clone();
            let client = client.clone();
            let show_toast = show_toast.clone();
            let poll_tick = poll_tick.clone();
            background(
                move || connect_wifi(&txt),
                move |result| {
                    match result {
                        Ok(ep) => {
                            let mut st = state.borrow_mut();
                            st.cfg.wireless_ip = ep.clone();
                            st.selected_serial = Some(ep.clone());
                            client.send(Request::Configure {
                                cfg: st.cfg.clone(),
                            });
                            show_toast(&format!("Connected to {ep}"));
                        }
                        Err(e) => show_toast(&e),
                    }
                    poll_tick();
                },
            );
        }
    });

    auto_usb_row.connect_active_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if !updating_ui.get() {
                state.borrow_mut().cfg.auto_start_usb = row.is_active();
                apply_setting_change();
            }
        }
    });

    auto_reconn_row.connect_active_notify({
        let state = state.clone();
        let updating_ui = updating_ui.clone();
        let apply_setting_change = apply_setting_change.clone();
        move |row| {
            if !updating_ui.get() {
                state.borrow_mut().cfg.auto_reconnect = row.is_active();
                apply_setting_change();
            }
        }
    });

    // QR Code pairing dialog
    qr_btn.connect_clicked({
        let client = client.clone();
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
                let client = client.clone();
                move || {
                    while let Ok((msg, done)) = rx.try_recv() {
                        if let Some(res) = done {
                            match res {
                                Ok(addr) => {
                                    {
                                        let mut st = state.borrow_mut();
                                        st.cfg.wireless_ip = addr.clone();
                                        st.selected_serial = Some(addr.clone());
                                        client.send(Request::Configure {
                                            cfg: st.cfg.clone(),
                                        });
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
            let show_toast = show_toast.clone();
            let sync_ui = sync_ui.clone();
            background(
                move || load_or_reload_v4l2(reload),
                move |result| {
                    match result {
                        Ok(()) => show_toast("AvyxCam (/dev/video0) ready"),
                        Err(e) => show_toast(&e),
                    }
                    sync_ui();
                },
            );
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
            let show_toast = show_toast.clone();
            let sync_ui = sync_ui.clone();
            background(install_boot_persistence, move |result| {
                match result {
                    Ok(()) => show_toast("Boot persistence installed"),
                    Err(e) => show_toast(&e),
                }
                sync_ui();
            });
        }
    });

    fix_mic_btn.connect_clicked({
        let sync_ui = sync_ui.clone();
        let show_toast = show_toast.clone();
        move |_| {
            let show_toast = show_toast.clone();
            let sync_ui = sync_ui.clone();
            background(ensure_virtual_mic, move |result| {
                match result {
                    Ok(()) => show_toast("PhoneMic ready"),
                    Err(e) => show_toast(&e),
                }
                sync_ui();
            });
        }
    });

    // The owner keeps supervising when this client closes.
    window.connect_close_request({
        let closed = closed.clone();
        let poll_tick_holder = poll_tick_holder.clone();
        move |_| {
            closed.store(true, Ordering::Relaxed);
            poll_tick_holder.borrow_mut().take();
            glib::Propagation::Proceed
        }
    });

    // Unused gio import warning prevention
    let _ = gio::ApplicationFlags::empty();

    window.present();
}
