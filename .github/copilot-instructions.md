# Copilot instructions for opendeck-mountain_displaypad

Purpose: guide Copilot sessions to work effectively on this Rust-based OpenDeck plugin for the Mountain DisplayPad (6x2 display buttons).

Build, test, and lint (how to run):
- Build (debug): cargo build --workspace
- Build (release): cargo build --workspace --release
- Run all tests: cargo test --workspace
- Run a single test (by name): cargo test <filter>
- Run tests for a specific crate: cargo test -p driver <filter> or cargo test -p adapter <filter>
- Format (check): cargo fmt --all -- --check
- Format (fix): cargo fmt --all
- Lint (check): cargo clippy --all -- -D warnings
- Cross-platform build (Docker): docker run --rm -v "${PWD}:/workspace" -w /workspace opendeck-devcontainer bash scripts/build-all.sh

High-level architecture (short):
- Workspace with two primary crates:
  - driver: HID device driver with state machine transfer protocol, parsing raw HID reports, and managing image transfers to the 102x102 pixel LCD buttons. Should NOT depend on OpenDeck specifics.
  - adapter: OpenDeck integration layer that consumes driver events, decodes/resizes images, and translates them into OpenDeck plugin actions/commands.
- HID/USB layer:
  - Windows/macOS: uses the hidapi crate to enumerate/connect to the device.
  - Linux: drives BOTH USB interfaces via raw libusb (the rusb crate), NOT hidapi. hidapi on Linux strips the leading HID report-ID byte, producing 63-byte packets the device NAKs for image (IMG_MSG) transfers. rusb finds the device by VID/PID and writes full report-ID-prefixed packets.
  - Two interfaces: Interface 1 (display - pixel data writes) and Interface 3 (device - commands + reads). Endpoints on Linux: display OUT=0x02, device OUT=0x04, device IN=0x83.
  - Linux cold-plug wake: an LED report to keyboard interface 0 (capslock bit) nudges an asleep device to initialize. It is only sent if the device fails to ack INIT within ~1.5s, because waking an already-awake device reboots it and drops pushed images.
- Hardware: 6x2 display buttons (6 columns x 2 rows), 102x102 pixels each, BGR pixel format.
- Reference implementations: ReversingForFun/MountainDisplayPadPy (Python/libusb) and SytxLabs/DisplayPad (Python) for protocol details; JeLuF/mountain-displaypad (Node.js) confirms the INIT/IMG_MSG hex constants and chunked pixel writes.

Key repository conventions:
- Keep driver code portable and testable without OpenDeck:
  - Unit-test parsing/mapping in driver crate with cargo test.
  - Adapter crate adds OpenDeck-specific glue and higher-level integration tests.
- Use cargo workspace for building/testing across crates.
- Enforce formatting and linting in CI: cargo fmt and cargo clippy (CI uses clippy warnings as errors).
- CI is defined in .github/workflows/ci.yml (cargo fmt, clippy, cargo test, plus a release build on Linux/Windows/macOS runners).
- Release workflow in .github/workflows/release.yml (tag-triggered, builds Linux/Windows/macOS).
- Devcontainer (.devcontainer) provides Rust, rustfmt, clippy, system HID libraries, and mingw-w64 for Windows cross-compilation.

Files of interest (quick pointers):
- Cargo.toml (workspace members: driver, adapter)
- driver/src/lib.rs (HID driver, state machine transfer protocol)
- adapter/src/main.rs (OpenDeck integration, image decoding/resizing)
- adapter/src/lib.rs (minimal report handling glue)
- manifest.json (OpenDeck plugin manifest)
- assets/ (plugin icons)
- scripts/build-all.sh (cross-platform build and packaging)
- .github/workflows/ci.yml (CI steps)
- .github/workflows/release.yml (release workflow)
- .devcontainer/* (development container)
