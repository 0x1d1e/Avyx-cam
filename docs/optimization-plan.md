# Avyx-cam optimization scope

Status: implementation plan. This document records milestone scope, not current feature availability.

## Objective

Make long webcam calls predictable, then measure Avyx-cam against alternatives on the same phone, network, and receiving application. No competitive ranking is established yet.

## Milestones

| Milestone | Scope | Completion evidence |
| --- | --- | --- |
| 1. Battery visibility and reconnect intent | Read-only battery status, bounded background queries, Stop and Auto-reconnect safeguards | Automated parsing/policy/process tests, GUI smoke check, hardware validation recorded separately |
| 2. Session ownership | Shared GUI/CLI session lifecycle, owned processes, bounded commands, serialized reconnect with backoff | Stop/disconnect/restart scenarios through production entry points |
| 3. Device capabilities and memory | Discover valid camera modes, stable phone identity across transports, restore saved preferences, atomic configuration writes | Multiple-phone compatibility checks and persistence tests |
| 4. Call continuity and efficiency | Stable output dimensions, fewer stream restarts, audio synchronization controls, incremental UI/log updates | Receiving-app interruption and latency measurements |
| 5. Power management | Validated screen-off behavior, optional power presets, Android thermal status where supported | Hour-long USB/Wi-Fi runs; battery and thermal measurements |
| 6. Image features and distribution | Exposure/focus controls where supported, framing/background effects, installation and dependency diagnostics | Image comparisons, resource measurements, clean-system installation checks |

Milestones are ordered by dependency. Each gets a separate issue and PR when implementation starts. Avoid moving every source file in milestone 1; extract ownership when the relevant behavior moves.

## Milestone 1 scope of work

### Behavior

- Show battery percentage, charging/full/discharging state, and battery temperature for the selected authorized phone on the Camera page.
- Label temperature as battery temperature. It is not a CPU/camera temperature or a universal thermal safety signal.
- Read Android battery data on selection and approximately every 20 seconds. Perform subprocess work outside GTK, with at most one battery query active.
- Give queries a five-second deadline, terminate and reap a timed-out child, and bound captured output.
- Clear stale readings on disconnect or selection changes; discard results for a previous selection. Missing/invalid fields remain unavailable.
- Keep monitoring read-only. No charging limits, screen changes, quality changes, automatic shutdown, or temperature-based interventions.
- Stop remains effective until the user explicitly starts again during this app session. Saved-device retries require Auto-reconnect enabled, no ready/authorized phone currently present, and at most one in-flight retry worker.
- Each `adb connect` attempt has an explicit five-second timeout; retrying through saved addresses aborts if Auto-reconnect is turned off, Stop is pressed, a phone connects, or a stream starts.
- Remembered-address retries do not connect to arbitrary mDNS advertisements. Reliable discovery across changing ports belongs to milestone 3.

### Design decisions

A battery module owns Android parsing and the query lifecycle. GTK renders results received through a channel. Monitoring uses its own slow cadence instead of adding another command to the existing synchronous device polling loop.

Reconnect eligibility remains at the existing GUI scheduling point, with regression checks through the running application. Broader session orchestration stays in milestone 2.

Battery reads are optional: failure cannot prevent camera use. No new runtime dependency or configuration migration is required.

### Acceptance and verification

- Test representative Android output, partial/invalid readings, charging/full states, and temperature units.
- Exercise the real query path with a fake adb executable for success, failure, bounded output, and timeout cleanup.
- Exercise reconnect preference and Stop behavior through the running GUI with controlled external commands.
- Run Cargo tests, formatting, Clippy, and release build.
- Smoke-test the GUI with no phone and, if available, a connected phone. Explicitly report untested hardware paths.

### Validation record

Battery parsing and real subprocess tests passed, including deadline cleanup and output bounds. The GTK smoke test passed with simulated Android/streaming commands: preference gating, Stop after Wi-Fi disconnect, stale-result rejection, and a responsive event loop during delayed battery reads. A rendered window was visually inspected. Formatting, strict Clippy, and release build passed.

No phone was connected during validation. Real Android battery output, USB/Wi-Fi streaming continuity, and hour-long thermal behavior remain unverified. These checks establish milestone behavior, not competitive performance.

### Risks and exclusions

Existing device discovery, orientation, camera probing, and some system commands still block GTK; milestone 1 only makes the new battery path asynchronous. Device-specific Android output may omit fields. Battery temperature cannot establish overall device thermal health.

Automatic reconnect cancellation, stable device identity, complete camera capability discovery, screen-off support, thermal thresholds, and end-to-end performance claims are excluded from this PR.

## Benchmark protocol for later milestones

Use the same phone, camera, capture mode, USB cable or Wi-Fi access point, and receiving app. Record versions and run length. Measure delivered FPS, freezes, end-to-end video delay, audio/video offset, reconnect time, host CPU/memory, battery change, and reported battery temperature. Separate requested capture FPS from delivered FPS. Include at least a 60-minute call and network interruption scenarios before claiming reliability improvements.
