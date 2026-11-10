mod battery;
mod config;
mod devices;
#[cfg(test)]
mod gui_tests;
mod qr;
mod stream;
mod ui;

use config::{load_config, save_config};
use devices::{list_adb_devices, switch_usb_to_wifi, video_exists, VIDEO_DEV};
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use std::env;
use stream::{read_external_pid, start_scrcpy, stop_all_streams};

fn print_usage() {
    println!(
        "Avyx-cam - Use your phone as a webcam & virtual microphone\n\n\
        Usage:\n  \
        avyx-cam                 Launch graphical control center\n  \
        avyx-cam back            Start streaming back camera\n  \
        avyx-cam front           Start streaming front camera\n  \
        avyx-cam stop            Stop all active streams\n  \
        avyx-cam wifi            Switch connected USB phone to Wi-Fi\n  \
        avyx-cam status          Show current stream & device status\n  \
        avyx-cam --help          Show this help message"
    );
}

fn handle_cli(cmd: &str) -> i32 {
    match cmd {
        "help" | "--help" | "-h" => {
            print_usage();
            0
        }
        "stop" => {
            stop_all_streams();
            println!("Avyx-cam stopped.");
            0
        }
        "status" => {
            let pid = read_external_pid();
            if let Some(p) = pid {
                println!("Avyx-cam: ACTIVE (PID {p})");
            } else {
                println!("Avyx-cam: IDLE");
            }
            println!("Device: /dev/video0 exists = {}", video_exists());
            let devs = list_adb_devices();
            println!("Phones connected: {}", devs.len());
            for d in devs {
                println!(
                    "  - {} [{}] ({})",
                    d.model,
                    d.state,
                    if d.is_wifi { "Wi-Fi" } else { "USB" }
                );
            }
            0
        }
        "wifi" => {
            let devs = list_adb_devices();
            let usb = devs.iter().find(|d| !d.is_wifi && d.state == "device");
            let Some(u) = usb else {
                eprintln!("No authorized USB phone found.");
                return 1;
            };
            println!("Switching {} to Wi-Fi...", u.model);
            match switch_usb_to_wifi(&u.serial, 5555) {
                Ok(ep) => {
                    let mut cfg = load_config();
                    cfg.wireless_ip = ep.clone();
                    save_config(&cfg);
                    println!("Connected over Wi-Fi: {ep}\nYou can unplug the USB cable now.");
                    0
                }
                Err(e) => {
                    eprintln!("Failed: {e}");
                    1
                }
            }
        }
        "front" | "back" => {
            let mut cfg = load_config();
            cfg.camera_id = if cmd == "front" {
                "1".to_string()
            } else {
                "0".to_string()
            };
            save_config(&cfg);

            let devs = list_adb_devices();
            let dev = devs.iter().find(|d| d.state == "device");
            let Some(d) = dev else {
                eprintln!("No authorized phone found via USB or Wi-Fi.");
                return 1;
            };

            println!(
                "Starting {} camera on {}...",
                if cmd == "front" { "front" } else { "back" },
                d.model
            );
            match start_scrcpy(&d.serial, &cfg, None, false) {
                Ok(handle) => {
                    println!(
                        "Started! Streaming to {VIDEO_DEV} (PID {})",
                        handle.child.id()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    1
                }
            }
        }
        other => {
            eprintln!("Unknown command: {other}");
            print_usage();
            1
        }
    }
}

fn main() -> glib::ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        let code = handle_cli(&args[1]);
        return if code == 0 {
            glib::ExitCode::SUCCESS
        } else {
            glib::ExitCode::FAILURE
        };
    }

    let app = adw::Application::builder()
        .application_id("com.local.avyxcam")
        .build();

    app.connect_activate(ui::build_ui);
    app.run_with_args(&Vec::<String>::new())
}
