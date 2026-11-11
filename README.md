# Avyx-cam

Native Linux desktop application built with Rust, GTK4, and Libadwaita to turn your Android phone into a high-performance webcam and virtual microphone.

No Android app installation required. Uses native Android ADB media streaming via `scrcpy`, `v4l2loopback`, and `PipeWire`.

---

## Features

- **Zero Phone App**: No third-party apps required on your phone. Works over native Android developer debugging.
- **Wireless QR Pairing**: Pair phone and laptop over Wi-Fi without ever plugging in a cable (Android 11+).
- **Auto-Rotate Stream**: Checks Android display orientation and restarts the stream when rotation changes.
- **Saved Devices & Auto-Reconnect**: Remembers phones and Wi-Fi endpoints. Automatic retries honor the Auto-reconnect setting and Stop button; saved Wi-Fi retries use remembered addresses.
- **Battery Monitoring**: Shows the selected phone's battery percentage, charging state, and battery temperature. Refreshes about every 20 seconds; missing data does not prevent streaming.
- **High Frame Rates**: The GUI offers up to 120 fps on supported back cameras. Available modes and delivered frame rates depend on the phone.
- **PipeWire Virtual Microphone**: Low-latency `PhoneMic` source selectable in Zoom, Google Meet, Discord, and OBS.
- **Full Lens & Hardware Controls**: Front/back switch, zoom slider, flashlight/torch, flip/un-mirror, bitrate, and v4l2 buffering.
- **Background Mode**: Disable live preview window to run purely as a headless system webcam.
- **CLI & GUI Modes**: Control completely through the GTK4 GUI or use command-line shortcuts (`avyx-cam back`, `avyx-cam stop`).

---

## Requirements

### Linux System Dependencies

- **scrcpy** with camera capture and `--capture-orientation` support (use a current upstream release)
- **android-tools** (`adb`)
- **v4l2loopback** kernel module
- **pipewire** / **pipewire-pulse**
- **libadwaita-1** & **gtk4**

#### Arch Linux / Manjaro
```bash
sudo pacman -S scrcpy android-tools v4l2loopback-dkms libadwaita gtk4 qrencode
```

#### Ubuntu / Debian / Fedora

These distributions need prerequisites before `./install.sh --install-deps`: Ubuntu/Debian need current upstream scrcpy; Fedora needs scrcpy from COPR and `akmod-v4l2loopback` from RPM Fusion Free. Follow the [distribution setup instructions](docs/installation.md#install). Ubuntu 24.04's packaged scrcpy is too old for camera streaming.

---

## Setup & First Run

1. **Load Virtual Camera Device**:
   ```bash
   sudo modprobe v4l2loopback devices=1 video_nr=0 card_label="AvyxCam" exclusive_caps=1
   ```
   *(Or click "Install" in Avyx-cam System tab to make it load automatically on boot).*

2. **Build and install Avyx-cam** from the downloaded source:
   ```bash
   ./install.sh --install-deps
   ```
   Already have the dependencies? Use `./install.sh`. This builds with Cargo, installs to `~/.local/bin`, and adds the application-menu launcher. Python/uv is not required.

   Open **Avyx-cam** from the application menu. See [installation details](docs/installation.md) for supported package managers, custom paths, updates, and removal. Downloading source alone does not run a build; prebuilt release downloads are planned.

---

## Connecting Your Phone

See the complete **[Connection Guide](docs/connection.md)** for detailed walkthroughs:

- **Wireless QR Pairing**: Go to *Connect* -> *Show QR Code*. On phone: *Developer options* -> *Wireless debugging* -> *Pair device with QR code*.
- **One-Click Wi-Fi**: Plug USB -> Click *Switch to Wi-Fi* -> Unplug USB.
- **Direct USB**: Plug phone in -> Tap *Allow USB debugging*.

---

## Command Line Interface (CLI)

Avyx-cam includes CLI commands for scripting and keyboard shortcuts (ensure `~/.local/bin` is on your `PATH`, or run `~/.local/bin/avyx-cam`):

```bash
avyx-cam                 # Launch GTK4 control center
avyx-cam back            # Start back camera stream
avyx-cam front           # Start front camera stream
avyx-cam stop            # Stop all active streams
avyx-cam wifi            # Switch plugged USB phone to Wi-Fi
avyx-cam status          # Show stream and device status
avyx-cam --help          # Show CLI options
```

---

## Development

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The optional GUI smoke test uses simulated Android and streaming commands. It requires a graphical session, `/dev/video0`, and no active webcam stream:

```bash
cargo test gui_smoke -- --ignored --test-threads=1
```

See [optimization scope](docs/optimization-plan.md) for planned milestones and validation criteria. Battery temperature comes from Android's battery service; it does not measure camera or CPU temperature. Monitoring is read-only and does not control charging or stream quality.

## Architecture

```
 Android Phone (Camera2 / Audio API)
               │
          ADB (USB or Wi-Fi)
               │
          scrcpy engine
         ┌─────┴─────┐
         ▼           ▼
   /dev/video0    PipeWire
    (AvyxCam)    (PhoneMic)
         │           │
         ▼           ▼
   Webcam Apps   Voice / VoIP
(Zoom, OBS, etc)
```

---

## License

MIT License. See [LICENSE](LICENSE) for details.
