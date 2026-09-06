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

[Unreleased]: https://github.com/marc/kolibri/commits/main
