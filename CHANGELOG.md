# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial project scaffold for the Seeed Studio XIAO ESP32-C3.
- Async blinky on `GPIO10` (`D10`) using Embassy, with a concurrent heartbeat
  task and a `Signal` that retunes the blink rate at runtime.
- Toolchain pinned via `rust-toolchain.toml` (Rust 1.98.1,
  `riscv32imc-unknown-none-elf`); builds on stable, no nightly required.
- `cargo run` wired to `espflash flash --monitor`.
- VS Code settings, tasks, extension recommendations, and probe-rs
  launch/attach debug configurations using the chip's built-in USB-JTAG.
- GitHub Actions CI: `fmt`, `clippy -D warnings`, release build, flashable
  image artifact, and `cargo-deny`.
- Dependabot for the `cargo` and `github-actions` ecosystems.
- Project logo: a hummingbird silhouette (`assets/kolibri.svg`) with light and
  dark wordmark lockups for the README.
- `tools/gen-logo.py`, which rasterises the logo master to the 1-bpp bitmaps in
  `src/logo.rs` so the OLED artwork and the repository logo cannot drift apart.
- Two-second boot splash on the OLED, and a small hummingbird on the status
  screen.
- Chip temperature on the OLED, read from the ESP32-C3's on-chip `tsens`
  sensor and published to the display through an `embassy_sync::watch::Watch`.
  The reading is die temperature, not ambient, and is labelled `chip` to say so.
- `temperature::Source` trait and a per-board `Sensor` type alias, so swapping
  the on-chip sensor for an external I²C part is one impl and one line.
- `tools/gen-oled-preview.py`, which regenerates the README's OLED preview from
  the real fonts, bitmaps and layout coordinates.
- `tools/fw-size.py`, which reports flash and RAM consumption from the linked
  ELF and the linker's own `memory.x`. CI runs it per board, writes the report
  to the job summary and the build artifact, and posts it as a pull request
  comment that is rewritten in place on every push.
- `boards/README.md`: which layers Embassy makes portable and which it does not,
  and the six steps to add a board.

### Changed

- The status screen shows the temperature where it used to show the blink rate.
  The LED still blinks and the rate is still logged.
- **Split into a Cargo workspace so the project can support more than one MCU.**
  `kolibri-core` holds the four task loops, the screen layout and the sensor
  abstraction, written against `embedded-hal-async`, `embedded-graphics` and two
  traits of its own (`temperature::Source`, `display::Panel`) rather than
  against a HAL. `boards/xiao-esp32c3` holds esp-hal, the entry point, the pin
  map, the on-chip `tsens` sensor, the target triple and the flashing runner.
  Build a board from inside its directory; a bare `cargo build` at the root
  builds only the portable crate, for the host, which is what keeps it portable.
  The flashed image is unchanged in behaviour and size — the generic loops
  monomorphise to the same code.
- `src/logo.rs` moved to `kolibri-core/src/logo.rs`, and `.cargo/config.toml` to
  `boards/xiao-esp32c3/.cargo/config.toml`. `tools/gen-logo.py` and
  `tools/gen-oled-preview.py` follow the new paths and still run from the
  repository root.
- Timing constants moved into `kolibri-core` as `blink::PERIOD`,
  `blink::PERIOD_FAST`, `heartbeat::PERIOD`, `display::PERIOD`,
  `display::SPLASH_PERIOD` and `temperature::PERIOD`; pins, bus addresses and
  clock rates stay in the board crate.
- CI gained a `portable core` job that lints `kolibri-core` for the host target,
  and the board build became a matrix with one entry per directory under
  `boards/`.

[Unreleased]: https://github.com/marc/kolibri/commits/main
