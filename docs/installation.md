# Source installation

Avyx-cam is a Rust application. Cargo resolves locked dependencies and compiles it; Python and uv are not required. Downloading or extracting the repository does not execute a build.

## Install

From a downloaded or cloned source directory:

```bash
./install.sh --install-deps
```

The installer can install native build/runtime packages through pacman, apt, or dnf, using sudo when needed. It requires Rust/Cargo >= 1.85; with `--install-deps`, an absent or older toolchain is supplemented by stable Rust through rustup. This downloads the official rustup installer when necessary, without changing your shell PATH or existing default toolchain. It does not add package repositories. Distribution packages may be too old or unavailable; in that case it stops with a dependency error. See the [scrcpy installation guide](https://github.com/Genymobile/scrcpy/blob/master/doc/linux.md) for current builds.

Fedora users must install scrcpy first using the upstream-recommended COPR repository (`sudo dnf copr enable zeno/scrcpy`, then `sudo dnf install scrcpy`). Fedora's v4l2loopback availability also depends on your configured repositories. Ubuntu/Debian releases with libadwaita older than 1.5 or outdated scrcpy need newer dependencies; installing distro packages alone will not fix those version limits.

With dependencies already installed, use:

```bash
./install.sh
```

It checks Linux support, Cargo, native GTK/libadwaita development libraries, ADB, scrcpy camera/orientation support, and PipeWire command-line tools. It then runs `cargo build --release --locked` for the compiler's host target, installs the executable to `~/.local/bin/avyx-cam`, and creates an application-menu entry. Run the installer as your regular desktop user. Only the optional package step uses sudo.

Launch **Avyx-cam** from the application menu or run `~/.local/bin/avyx-cam` (add `~/.local/bin` to `PATH` if you want to invoke `avyx-cam` directly). If `/dev/video0` is missing, use the app's System page to enable the virtual camera. Authorize Android USB debugging or pair Wireless debugging as described in the [connection guide](connection.md). These phone authorization steps are required by Android.

A working PipeWire service with PulseAudio compatibility is required for the virtual microphone. Package setup installs audio client tools; it does not replace your audio server, change boot configuration, load kernel modules, or launch a camera stream. On Arch, install headers matching your running kernel for v4l2loopback DKMS; Secure Boot may require signing the module.

## Custom location and removal

```bash
./install.sh --prefix "$HOME/Applications/Avyx-cam" --data-home "$HOME/.local/share"
```

`--prefix` controls the directory containing `bin/avyx-cam`; `--data-home` controls the directory containing `applications/com.local.avyxcam.desktop` (matching the GTK application ID `com.local.avyxcam`). Defaults respect `XDG_DATA_HOME` for desktop data. Use absolute paths without control characters; the install prefix also cannot contain `%` or `=` (Desktop Entry `Exec` specification limits). Spaces and quoted characters are supported. Existing files at these two destinations are replaced after a successful build; if replacing the desktop entry fails after updating the binary, the previous binary is restored.

To remove a default installation:

```bash
rm -- "$HOME/.local/bin/avyx-cam" "$HOME/.local/share/applications/com.local.avyxcam.desktop"
```

This keeps preferences, system dependencies, and kernel configuration. Re-run the installer from updated source to update the app. A running app must be restarted to use the update.

## Scope and verification

This installer is the first distribution step. It builds downloaded source automatically when run. Downloadable prebuilt binaries, signed release archives, CI publishing, and distribution packages remain planned work.

Acceptance: installation from a path containing spaces into an empty directory, installed CLI execution, desktop launcher validation, dependency failure before destination changes, and unsupported-option handling. Tests use a real Cargo release build and controlled missing-dependency scenarios. A clean operating-system installation is a separate validation requirement.

Validated locally: a real locked release build and installed CLI, source/install paths containing spaces and quotes, rejecting `--prefix` with `%` or `=`, `com.local.avyxcam.desktop` validation and `gio launch`, preflight and rollback when the desktop destination cannot be replaced during an upgrade, an overridden cross-compilation environment selecting the correct host artifact, and dependency/toolchain/build failures preserving existing files. Native package installation, downloading/bootstrap of rustup, and interactive GNOME application-menu window grouping were not executed during these checks.
