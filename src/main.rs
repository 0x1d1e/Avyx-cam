mod battery;
mod config;
mod devices;
#[cfg(test)]
mod gui_tests;
mod process;
mod qr;
mod session;
mod stream;
mod ui;

use devices::{list_adb_devices, switch_usb_to_wifi, video_exists, VIDEO_DEV};
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use session::{request, Request};
use std::env;

fn print_usage() {
    println!(
        "Avyx-cam - Use your phone as a webcam & virtual microphone\n\n\
        Usage:\n  \
        avyx-cam                 Launch graphical control center\n  \
        avyx-cam back            Start streaming back camera\n  \
        avyx-cam front           Start streaming front camera\n  \
        avyx-cam stop            Stop the shared Avyx-cam session\n  \
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
        "stop" => match request(Request::Stop) {
            Ok(_) => {
                println!("Avyx-cam stopped.");
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        "status" => match request(Request::Status) {
            Ok(s) => {
                println!("Avyx-cam: {}", s.phase.to_uppercase());
                println!(
                    "Desired: {}; explicitly stopped: {}",
                    s.desired, s.user_stopped
                );
                if let Some(pid) = s.pid {
                    println!("Owned process: {pid} (frame delivery unverified)");
                }
                if let Some(error) = s.error {
                    println!("Error: {error}");
                }
                println!("Device: {VIDEO_DEV} exists = {}", video_exists());
                for d in s.devices {
                    println!("  - {} [{}] ({})", d.model, d.state, d.serial);
                }
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
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
                    let mut cfg = match request(Request::Status) {
                        Ok(snapshot) => snapshot.cfg,
                        Err(error) => {
                            eprintln!("{error}");
                            return 1;
                        }
                    };
                    cfg.wireless_ip = ep.clone();
                    cfg.record_device(&ep, &u.model, true, Some(&ep));
                    if let Err(error) = request(Request::Configure { cfg }) {
                        eprintln!("{error}");
                        return 1;
                    }
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
            let mut cfg = match request(Request::Status) {
                Ok(snapshot) => snapshot.cfg,
                Err(error) => {
                    eprintln!("{error}");
                    return 1;
                }
            };
            cfg.camera_id = if cmd == "front" {
                "1".to_string()
            } else {
                "0".to_string()
            };
            match request(Request::Start { serial: None, cfg }) {
                Ok(_) => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
                    loop {
                        match request(Request::Status) {
                            Ok(s) if s.phase == "streaming" => {
                                println!(
                                    "Started scrcpy (PID {}). Frame delivery is not verified.",
                                    s.pid.unwrap_or_default()
                                );
                                return 0;
                            }
                            Ok(s) if s.phase != "starting" => {
                                eprintln!(
                                    "{}: {}",
                                    s.phase,
                                    s.error.unwrap_or_else(|| "Start cancelled".into())
                                );
                                return 1;
                            }
                            Err(e) => {
                                eprintln!("{e}");
                                return 1;
                            }
                            _ => (),
                        }
                        if std::time::Instant::now() >= deadline {
                            eprintln!("Start is still pending; inspect 'avyx-cam status'. Use 'avyx-cam stop' to cancel.");
                            return 1;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
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
    if args.get(1).is_some_and(|arg| arg == "--session-owner") {
        return match session::run_owner() {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                glib::ExitCode::FAILURE
            }
        };
    }
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
