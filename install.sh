#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf '%s\n' 'Usage: ./install.sh [--install-deps] [--prefix ABSOLUTE_PATH] [--data-home ABSOLUTE_PATH]' \
        'Build locked Rust dependencies and install Avyx-cam for the current user.' \
        'Defaults: ~/.local/bin/avyx-cam and ${XDG_DATA_HOME:-~/.local/share}/applications/com.local.avyxcam.desktop' \
        '--install-deps installs distribution packages using sudo; no repositories are added.'
}
fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }

install_prefix="${HOME}/.local"
install_data_home="${XDG_DATA_HOME:-${HOME}/.local/share}"
install_deps=false
while (($#)); do
    case "$1" in
        --help|-h) usage; exit 0 ;;
        --install-deps) install_deps=true; shift ;;
        --prefix|--data-home)
            (($# >= 2)) || fail "$1 requires an absolute path"
            case "$2" in /*) ;; *) fail "$1 requires an absolute path" ;; esac
            if [[ "$1" == --prefix ]]; then install_prefix="$2"; else install_data_home="$2"; fi
            shift 2 ;;
        *) fail "Unknown option: $1 (see --help)" ;;
    esac
done
[[ "$install_prefix" == /* && "$install_data_home" == /* ]] || fail 'Installation paths must be absolute'
for install_path in "$install_prefix" "$install_data_home"; do
    [[ ! "$install_path" =~ [[:cntrl:]] ]] || fail 'Installation paths cannot contain control characters'
done
[[ "$install_prefix" != *%* ]] || fail 'Install prefix cannot contain percent signs (desktop launcher limitation)'
[[ "$install_prefix" != *=* ]] || fail 'Install prefix cannot contain equals signs (desktop launcher limitation)'
[[ "$(uname -s)" == Linux ]] || fail 'Avyx-cam currently supports Linux only'
((EUID != 0)) || fail 'Run as your desktop user; --install-deps invokes sudo only for packages'
source_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
[[ -f "$source_dir/Cargo.toml" && -f "$source_dir/Cargo.lock" ]] || fail 'Run this installer from a complete Avyx-cam source checkout'

check_scrcpy() {
    command -v scrcpy >/dev/null || fail 'Install current scrcpy first: https://github.com/Genymobile/scrcpy/blob/master/doc/linux.md . Then rerun this installer.'
    local scrcpy_help option
    scrcpy_help="$(scrcpy --help)" || fail 'Cannot query scrcpy capabilities'
    for option in --video-source --camera-id --camera-fps --capture-orientation --v4l2-sink --audio-source; do
        [[ "$scrcpy_help" == *"$option"* ]] || fail "scrcpy lacks $option. Install a current version: https://github.com/Genymobile/scrcpy/blob/master/doc/linux.md . Then rerun this installer."
    done
}

if "$install_deps"; then
    command -v sudo >/dev/null || fail 'sudo is required for --install-deps'
    [[ -r /etc/os-release ]] || fail 'Cannot detect distribution; install dependencies manually'
    # The operating system owns this shell-format file.
    . /etc/os-release
    case " ${ID:-} ${ID_LIKE:-} " in
        *arch*)
            sudo pacman -S --needed base-devel curl pkgconf gtk4 libadwaita android-tools scrcpy \
                libpulse v4l2loopback-dkms v4l-utils polkit psmisc ;;
        *debian*|*ubuntu*)
            # Stable distro scrcpy packages may lack camera/orientation support.
            check_scrcpy
            sudo apt-get update
            sudo apt-get install build-essential curl pkg-config libgtk-4-dev libadwaita-1-dev \
                adb pulseaudio-utils v4l2loopback-dkms v4l-utils pkexec psmisc ;;
        *fedora*)
            command -v scrcpy >/dev/null || fail 'Install Fedora scrcpy first: sudo dnf copr enable zeno/scrcpy && sudo dnf install scrcpy. Then rerun this installer.'
            check_scrcpy
            if ! rpm -q akmod-v4l2loopback >/dev/null 2>&1; then
                fail 'Install Fedora virtual-camera support first: enable RPM Fusion Free (https://rpmfusion.org/Configuration), then sudo dnf install akmod-v4l2loopback. Then rerun this installer.'
            fi
            sudo dnf install gcc curl pkgconf-pkg-config gtk4-devel libadwaita-devel android-tools \
                pulseaudio-utils v4l-utils polkit psmisc ;;
        *) fail 'Unsupported package manager; install dependencies manually, then run ./install.sh' ;;
    esac
fi

missing=()
for tool in cc pkg-config adb scrcpy pactl v4l2-ctl pkexec fuser; do
    command -v "$tool" >/dev/null || missing+=("$tool")
done
((${#missing[@]} == 0)) || fail "Missing commands: ${missing[*]}. Run ./install.sh --install-deps or install them manually."
pkg-config --atleast-version=1.5 libadwaita-1 || fail 'libadwaita development files >= 1.5 are required'
pkg-config --exists gtk4 || fail 'GTK4 development files are required'
check_scrcpy

cargo_command=(cargo)
rustc_command=(rustc)
recent_toolchain() {
    local version_text
    version_text="$("$@" --version 2>/dev/null)" || return 1
    [[ "$version_text" =~ \ ([0-9]+)\.([0-9]+)\. ]] || return 1
    ((BASH_REMATCH[1] > 1 || (BASH_REMATCH[1] == 1 && BASH_REMATCH[2] >= 85)))
}
if ! recent_toolchain "${cargo_command[@]}" || ! recent_toolchain "${rustc_command[@]}"; then
    "$install_deps" || fail 'Rust and Cargo >= 1.85 are required. Install via https://rustup.rs or rerun with --install-deps.'
    if command -v rustup >/dev/null; then
        rustup_command="$(command -v rustup)"
    elif [[ -x "${CARGO_HOME:-$HOME/.cargo}/bin/rustup" ]]; then
        rustup_command="${CARGO_HOME:-$HOME/.cargo}/bin/rustup"
    else
        command -v curl >/dev/null || fail 'curl is required to install Rust from rustup.rs'
        rustup_script="$(mktemp)"
        trap 'rm -f -- "$rustup_script"' EXIT
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$rustup_script"
        RUSTUP_INIT_SKIP_PATH_CHECK=yes sh "$rustup_script" -y --profile minimal --default-toolchain none --no-modify-path
        rm -f -- "$rustup_script"
        trap - EXIT
        rustup_command="${CARGO_HOME:-$HOME/.cargo}/bin/rustup"
    fi
    "$rustup_command" toolchain install stable --profile minimal
    cargo_command=("$rustup_command" run stable cargo)
    rustc_command=("$rustup_command" run stable rustc)
fi
recent_toolchain "${cargo_command[@]}" && recent_toolchain "${rustc_command[@]}" || fail 'Rust toolchain >= 1.85 is required'

printf 'Building Avyx-cam with Cargo...\n'
# An explicit host target prevents Cargo config/env from selecting a different
# artifact directory and accidentally reinstalling an older default-target build.
compiler_info="$("${rustc_command[@]}" -vV)"
host_target=''
while IFS= read -r compiler_line; do
    if [[ "$compiler_line" == 'host: '* ]]; then host_target="${compiler_line#host: }"; fi
done <<< "$compiler_info"
[[ -n "$host_target" ]] || fail 'Cannot determine the Rust host target'
"${cargo_command[@]}" build --manifest-path "$source_dir/Cargo.toml" --release --locked \
    --target "$host_target" --target-dir "$source_dir/target"
built_binary="$source_dir/target/$host_target/release/avyx-cam"
[[ -x "$built_binary" ]] || fail 'Cargo did not produce the expected Linux executable'
"$built_binary" --help >/dev/null

install -d -- "$install_prefix/bin" "$install_data_home/applications"
binary_dest="$install_prefix/bin/avyx-cam"
desktop_dest="$install_data_home/applications/com.local.avyxcam.desktop"
[[ ! -d "$binary_dest" ]] || fail "Destination is a directory, cannot install executable: $binary_dest"
[[ ! -d "$desktop_dest" ]] || fail "Destination is a directory, cannot install desktop launcher: $desktop_dest"

binary_stage="$(mktemp "$install_prefix/bin/.avyx-cam.XXXXXX")"
desktop_stage=''
binary_backup=''
cleanup() {
    rm -f -- "$binary_stage"
    if [[ -n "$desktop_stage" ]]; then rm -f -- "$desktop_stage"; fi
    if [[ -n "$binary_backup" ]]; then rm -f -- "$binary_backup"; fi
}
trap cleanup EXIT
desktop_stage="$(mktemp --suffix=.desktop "$install_data_home/applications/.com.local.avyxcam.XXXXXX")"
install -m 755 -- "$built_binary" "$binary_stage"

# Desktop Exec first unescapes string values, then parses quoted arguments.
# Escape both layers; percent is separately interpreted as a field code.
exec_path="$binary_dest"
exec_path="${exec_path//\\/\\\\}"
exec_path="${exec_path//\"/\\\"}"
exec_path="${exec_path//\$/\\\$}"
exec_path="${exec_path//\`/\\\`}"
exec_path="${exec_path//\\/\\\\}"
exec_path="${exec_path//%/%%}"
printf '%s\n' '[Desktop Entry]' 'Type=Application' 'Name=Avyx-cam' \
    'Comment=Use your Android phone as a webcam and microphone' \
    "Exec=\"$exec_path\"" 'Icon=camera-web-symbolic' 'Terminal=false' \
    'Categories=AudioVideo;Video;' 'StartupNotify=true' > "$desktop_stage"
chmod 644 "$desktop_stage"
if command -v desktop-file-validate >/dev/null; then desktop-file-validate "$desktop_stage"; fi

had_binary=false
if [[ -e "$binary_dest" || -L "$binary_dest" ]]; then
    had_binary=true
    binary_backup="$(mktemp "$install_prefix/bin/.avyx-cam.backup.XXXXXX")"
    if ! ln -f -- "$binary_dest" "$binary_backup" 2>/dev/null; then
        cp -fPp -- "$binary_dest" "$binary_backup"
    fi
fi

# Rename into place so replacing a running executable does not truncate its file.
mv -fT -- "$binary_stage" "$binary_dest"
if ! mv -fT -- "$desktop_stage" "$desktop_dest"; then
    if "$had_binary" && [[ -n "$binary_backup" && ( -e "$binary_backup" || -L "$binary_backup" ) ]]; then
        mv -fT -- "$binary_backup" "$binary_dest" || true
    elif ! "$had_binary"; then
        rm -f -- "$binary_dest" || true
    fi
    fail "Failed to install desktop launcher: $desktop_dest"
fi
if command -v update-desktop-database >/dev/null; then
    update-desktop-database "$install_data_home/applications" || true
fi
printf 'Installed: %s\nDesktop entry: %s\nLaunch Avyx-cam from your application menu.\n' "$binary_dest" "$desktop_dest"
if [[ ":$PATH:" != *":$install_prefix/bin:"* ]]; then
    printf 'Add %s/bin to PATH to run `avyx-cam` from a terminal.\n' "$install_prefix"
fi
if [[ ! -e /dev/video0 ]]; then
    printf 'First launch: use System to enable /dev/video0 (requires v4l2loopback and matching kernel headers).\n'
fi
printf 'Connect your phone and authorize Android debugging. See docs/connection.md.\n'
