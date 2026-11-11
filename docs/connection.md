# Connection Guide - Avyx-cam

Avyx-cam connects your Android phone to Linux as a virtual webcam (`/dev/video0`) and virtual PipeWire microphone (`PhoneMic`). No companion app required on the phone.

---

## Prerequisites (Phone Setup)

1. Open Android **Settings** -> **About phone**.
2. Tap **Build number** 7 times until Developer options unlock.
3. Go to **Settings** -> **System** (or **Additional settings**) -> **Developer options**.
4. Enable **USB debugging**.
5. Enable **Wireless debugging** (Android 11+).

---

## Method 1: Wireless QR Pairing (Recommended - No Cable)

Connect without ever plugging in a USB cable:

1. Connect laptop and phone to the same Wi-Fi network.
2. Launch **Avyx-cam** and open the **Connect** tab.
3. Click **Show QR Code** under *Wireless Pairing*.
4. On your phone:
   - Open **Settings** -> **Developer options**.
   - Tap directly on the text **Wireless debugging** to open its sub-menu.
   - Tap **Pair device with QR code**.
5. Point your phone camera at the QR code on your laptop screen.
6. Avyx-cam detects pairing over local mDNS, saves the pairing keys, and connects immediately.

---

## Method 2: One-Click USB to Wi-Fi Switch

If your phone is already connected via USB:

1. Plug phone into your laptop via USB.
2. In Avyx-cam **Connect** tab, click **Switch to Wi-Fi**.
3. Once the notification confirms connection, unplug the USB cable.

---

## Method 3: Manual Wi-Fi IP and Port

1. In phone **Developer options** -> **Wireless debugging**, note the IP address and 5-digit port (e.g. `192.168.1.44:39495`).
2. In Avyx-cam **Connect** tab, enter `IP:PORT` into the **Wi-Fi IP:Port** field and click apply.

---

## Method 4: Direct USB Cable

1. Plug phone via USB.
2. When prompted on phone, tap **Allow USB debugging** and check **Always allow from this computer**.
3. Avyx-cam detects the phone during background discovery. If **Auto-start on USB Plug-in** is enabled, a new USB connection starts streaming unless you previously pressed Stop. A newly started session owner begins idle, even with a phone already connected. Stop is remembered in the per-login runtime directory, so an owner crash or restart does not undo it; logging out or rebooting clears it.

---

## Saved Devices & Auto-Reconnect

- Avyx-cam remembers previously paired USB and Wi-Fi devices in `~/.config/avyx-cam/config.json`.
- When opening Avyx-cam, click **Connect** next to any remembered device in the **Saved Devices** list for 1-click reconnect.
- Auto-reconnect retries an explicitly started session using remembered addresses and authorized devices. Turning it off cancels pending retry work and leaves a healthy stream running.

## Session Controls

GUI and CLI share one session per Linux user. `avyx-cam back` and `avyx-cam front` work without a graphical display. Closing the GUI leaves the session supervised; reopening it attaches to the same session. Changing camera settings replaces the owned stream in sequence and may interrupt frames.

**Stop** and `avyx-cam stop` cancel pending starts and reconnects as well as the active stream. Unplugging/replugging a phone or enabling Auto-reconnect does not undo Stop. Click Start or run a CLI camera command to resume.

**Concurrent clients:** the GUI and CLI send only the settings they changed, and the owner merges them, so one client cannot overwrite another's unrelated settings. If two clients change the same setting, the last request wins.

**USB to Wi-Fi handoff:** after a successful switch, the owner rebinds the active session to the Wi-Fi endpoint without interrupting the USB stream. When you unplug USB, the stream resumes over Wi-Fi through normal Auto-reconnect.

`avyx-cam status` reports session intent, process state, and errors. A running scrcpy process does not prove that frames reach `/dev/video0` or your meeting app. Authorization, camera privacy, and camera-in-use errors require action on the phone followed by an explicit restart.

The owner starts on demand and begins idle after a crash. USB auto-start responds to new connections after initial discovery. Session logs are in `$XDG_RUNTIME_DIR/avyx-cam/owner.log` and `stream.log`; when XDG_RUNTIME_DIR is absent, they are under `${TMPDIR:-/tmp}/avyx-cam-<uid>/`. Inspect stream logs in the System tab.

**Upgrading from an older version:** existing standalone scrcpy streams are not adopted or stopped automatically. Stop them using their original launcher or terminal before starting a new Avyx-cam session. Legacy PID files and process-name matches never authorize termination.

---

## Troubleshooting

### Phone says "Unauthorized"
- Unlock phone screen.
- Look for prompt: "Allow USB debugging?".
- Check "Always allow from this computer" and tap **Allow**.

### QR Pairing times out
- Verify phone and laptop are on the same Wi-Fi subnet (disable VPN/client isolation on guest Wi-Fi).
- On Android, ensure you tapped **Pair device with QR code** (not standard camera app).

### Virtual Camera device `/dev/video0` missing
- Click **Load / Reload** in the **System** tab, or run:
  ```bash
  sudo modprobe v4l2loopback devices=1 video_nr=0 card_label=AvyxCam exclusive_caps=1
  ```
- Click **Install** under *Load AvyxCam on Boot* to make persistence permanent across reboots.

### Microphone not showing in apps
- In Discord, Zoom, OBS, or browser settings, select **PhoneMic** or **Monitor of PhoneMic_sink** as your input source.
