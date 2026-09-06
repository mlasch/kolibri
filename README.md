<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/kolibri-wordmark-dark.svg">
  <img src="assets/kolibri-wordmark.svg" alt="kolibri" width="260">
</picture>

Async firmware boilerplate for the **Seeed Studio XIAO ESP32-C3**, built on
[esp-hal](https://github.com/esp-rs/esp-hal) and [Embassy](https://embassy.dev).
Builds on **stable Rust** — no nightly, no `-Z build-std`, no Xtensa toolchain.

The application lives in a HAL-independent crate and the chip-specific parts in
a board crate, so moving to another MCU is a port of the board crate rather than
a rewrite — see [`boards/README.md`](boards/README.md).

*Kolibri* is German for hummingbird — hence the bird, which the firmware also
draws on the OLED.

[![CI](https://github.com/marc/kolibri/actions/workflows/ci.yml/badge.svg)](https://github.com/marc/kolibri/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#licence)

The example firmware runs four concurrent Embassy tasks: one sampling the
chip's temperature sensor, one drawing that to the OLED, one blinking an LED on
`D10`, and one logging a heartbeat and retuning the blink rate through an
`embassy_sync::Signal`.

```
kolibri-core/          portable: the four task loops, the screen, the sensor trait
boards/xiao-esp32c3/   chip-specific: esp-hal, entry point, pins, tsens, runner
```

---

## Hardware

> **The XIAO ESP32-C3 has no user-controllable onboard LED.** Its two LEDs are a
> hardwired power indicator and a battery-charge indicator — neither is reachable
> from software. Seeed's own Blink example wires an external LED, and so does
> this one.

Wire an LED and a current-limiting resistor between **D10** and **GND**:

```
  GPIO10 (D10) ──────[ 150 Ω ]──────▶|──────  GND
                                    LED
                              (long leg → resistor)
```

**You do not need the LED to check the firmware works.** Every transition is
logged over USB serial, so `cargo run` on a bare board still shows `led on` /
`led off` scrolling past.

### Pin reference

| Silkscreen | GPIO | Notes |
|---|---|---|
| D0–D3 | 2, 3, 4, 5 | ADC-capable. **GPIO2 is a strapping pin.** |
| D4 / D5 | 6, 7 | I²C SDA / SCL — **used for the SH1106 OLED here** |
| D6 / D7 | 21, 20 | UART TX / RX |
| D8 / D9 | 8, 9 | SPI SCK / MISO. **Both are strapping pins** (GPIO9 = BOOT). |
| D10 | 10 | SPI MOSI — **used for the LED here** |

Avoid GPIO2, GPIO8 and GPIO9 for outputs: driving them at reset can put the chip
into the wrong boot mode.

### OLED display

An SH1106 128x64 I²C module wires to `D4` (SDA), `D5` (SCL), `3V3` and `GND`.
Use the module's own pull-ups — the C3's internal ones are ~45 kΩ, too weak for
the 400 kHz the firmware configures. The default address is `0x3C`.

On boot the firmware puts up a two-second splash and then switches to the
status screen. Both are rendered here pixel-for-pixel as the firmware draws
them, from the same font and the same bitmaps:

![Two 128x64 OLED screens: the boot splash with the hummingbird logo beside the words kolibri and esp32-c3, and the status screen showing 41.7 degrees Celsius in a large font with a smaller hummingbird beside it](assets/oled-preview.png)

That image is generated too, by
[`tools/gen-oled-preview.py`](tools/gen-oled-preview.py), from
embedded-graphics' own font sheets and the bitmaps in
[`kolibri-core/src/logo.rs`](kolibri-core/src/logo.rs) at the coordinates
[`kolibri-core/src/display.rs`](kolibri-core/src/display.rs) actually uses — so
it cannot quietly drift away from what the panel shows:

```sh
python3 tools/gen-oled-preview.py   # needs Pillow; rerun after changing the layout
```

The bird is the repository logo, rasterised from the same vector master. A
two-colour panel has no grey to model shape with, so the artwork is a
silhouette and the outline has to carry the whole bird.
[`tools/gen-logo.py`](tools/gen-logo.py) renders
[`assets/kolibri.svg`](assets/kolibri.svg) at the two sizes the firmware draws
and writes [`kolibri-core/src/logo.rs`](kolibri-core/src/logo.rs):

```sh
python3 tools/gen-logo.py   # needs Inkscape and Pillow; rerun after editing the SVG
```

The packing is what `embedded_graphics::image::ImageRaw` expects — one bit per
pixel, rows padded to whole bytes, most significant bit leftmost. A set bit is
a *lit* pixel, so the bird glows against the panel's own black rather than
being punched out of a lit rectangle. Both widths are multiples of eight, so
every row is a whole number of bytes and the generated lines correspond one to
one with display rows. The 40x30 glyph is thresholded fatter than the 64x48
one: at that size the beak is a single pixel wide and a neutral threshold drops
it.

The driver is [`oled_async`](https://crates.io/crates/oled_async), not the more
familiar `ssd1306` crate. The SH1106 has **no horizontal addressing mode**: its
column pointer wraps within the current page rather than advancing to the next
one, so `ssd1306`'s `flush` — which streams all 8 pages back-to-back and relies
on that auto-advance — rewrites page 0 eight times instead of drawing a frame.
`oled_async` re-sends the page and column address per page, and its
`Sh1106_128_64` variant carries the `COLUMN_OFFSET = 2` that the controller's
132-column RAM requires.

---

## Prerequisites

The Rust toolchain and the `riscv32imc-unknown-none-elf` target install
themselves from [`rust-toolchain.toml`](rust-toolchain.toml) the first time you
run a `cargo` command. You only need the flashing tools:

```sh
cargo install espflash --locked        # flash + serial monitor
cargo install probe-rs-tools --locked  # breakpoint debugging (optional)
```

### Linux: USB permissions

The XIAO's USB-C port is wired straight to the ESP32-C3's built-in USB
Serial/JTAG peripheral (there is no CP210x/CH340 bridge). Without a udev rule,
flashing fails with a permission error — this is by far the most common setup
problem.

```sh
sudo tee /etc/udev/rules.d/99-esp32c3.rules >/dev/null <<'EOF'
# Espressif USB JTAG/serial debug unit (ESP32-C3, -C6, -S3, ...)
SUBSYSTEM=="usb", ATTR{idVendor}=="303a", ATTR{idProduct}=="1001", MODE="0666", TAG+="uaccess"
SUBSYSTEM=="tty", ATTRS{idVendor}=="303a", ATTRS{idProduct}=="1001", MODE="0666", TAG+="uaccess"
EOF

sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG dialout "$USER"   # then log out and back in
```

Verify the board is seen: `lsusb | grep 303a` should show
`Espressif USB JTAG/serial debug unit`.

---

## Build, flash, run

Build from **inside the board directory** — that is how Cargo picks up
[`boards/xiao-esp32c3/.cargo/config.toml`](boards/xiao-esp32c3/.cargo/config.toml),
and with it the target triple, the linker script and the flashing runner:

```sh
cd boards/xiao-esp32c3

cargo build                # debug
cargo build --release      # optimised (opt-level = "s", fat LTO)

cargo run --release        # flash over USB-C and open the serial monitor
```

At the repository root, a bare `cargo build` builds only `kolibri-core`, for the
host — the standing check that the portable crate is still portable.

Build output goes to `target/` at the repository root either way: workspace
members share one target directory, and the board's binary is named `kolibri`
whatever the board.

`cargo run` works because the board's config sets
`runner = "espflash flash --monitor"`. Expected output:

```
INFO - kolibri starting on XIAO ESP32-C3
INFO - display on 0x3c
INFO - display ready
INFO - chip temperature 41.7 C
INFO - led on
INFO - led off
...
INFO - heartbeat (uptime 5 s)
INFO - blink period -> 100 ms
```

Change the log level per invocation: `ESP_LOG=debug cargo run --release`
(the default, `info`, is baked in via the board's `.cargo/config.toml`).

### If flashing fails to start

espflash resets the chip over USB automatically, but if that ever fails
(a half-bricked image, a flaky cable), force download mode by hand:

1. Hold **BOOT** (the button next to the USB port, GPIO9).
2. Tap **RESET**.
3. Release **BOOT**, then run `cargo run` again.

A cable that only carries power and no data looks exactly like a dead board —
if `lsusb` shows nothing, try another cable before anything else.

---

## Debugging

### Breakpoints (probe-rs, VS Code)

The C3's built-in USB-JTAG means **no external debug probe is needed** — the
same USB-C cable does it. Press **F5** in VS Code and pick
*Debug (probe-rs, built-in USB-JTAG)*; see [`.vscode/launch.json`](.vscode/launch.json).
There is also an *Attach* configuration that connects to already-running
firmware without reflashing or resetting.

> **Only one tool can hold the USB device at a time.** Stop any running
> `cargo run` / espflash monitor before starting a debug session, and vice
> versa. "Probe not found" almost always means a monitor is still attached.

From the command line:

```sh
probe-rs run --chip esp32c3 target/riscv32imc-unknown-none-elf/debug/kolibri
probe-rs info --chip esp32c3
```

The ESP32-C3 has 4 hardware breakpoints. Debug info is kept in release builds
too (`debug = 2`), so release binaries stay steppable — it costs flash nothing,
since debug sections are not loaded onto the device.

### Panics

`esp-backtrace` prints a panic message and a stack trace over the same serial
link. `espflash monitor` resolves the addresses against the ELF automatically,
so a panic looks like real function names rather than raw hex. The
`-C force-frame-pointers` flag in the board's `.cargo/config.toml` is what makes
that unwinding possible — don't remove it.

### Logging

`esp-println` writes plain UTF-8 to the USB Serial/JTAG peripheral, so any
terminal can read it — `espflash monitor`, `screen /dev/ttyACM0 115200`, or the
VS Code terminal. Use `log::{trace,debug,info,warn,error}!` as usual.

---

## Project layout

A Cargo workspace: one portable crate, one crate per board.

| Path | What it is |
|---|---|
| [`Cargo.toml`](Cargo.toml) | Workspace root — shared dependency versions, profiles, lints |
| [`kolibri-core/src/lib.rs`](kolibri-core/src/lib.rs) | The portable crate: what a board has to provide |
| [`kolibri-core/src/blink.rs`](kolibri-core/src/blink.rs) | LED loop, generic over `StatefulOutputPin` |
| [`kolibri-core/src/heartbeat.rs`](kolibri-core/src/heartbeat.rs) | Liveness log, retunes the blink rate |
| [`kolibri-core/src/temperature.rs`](kolibri-core/src/temperature.rs) | `Celsius`, the `Source` trait, and the sampling loop |
| [`kolibri-core/src/display.rs`](kolibri-core/src/display.rs) | The `Panel` trait, the SH1106 helper, and the screen |
| [`kolibri-core/src/logo.rs`](kolibri-core/src/logo.rs) | Generated 1-bpp hummingbird bitmaps for the OLED |
| [`boards/README.md`](boards/README.md) | **How to add a board, and what porting actually costs** |
| [`boards/xiao-esp32c3/src/main.rs`](boards/xiao-esp32c3/src/main.rs) | Entry point, pin map, on-chip sensor, task declarations |
| [`boards/xiao-esp32c3/.cargo/config.toml`](boards/xiao-esp32c3/.cargo/config.toml) | Target, linker flags, `cargo run` runner |
| [`assets/`](assets/) | Logo master, wordmarks, and the OLED preview |
| [`tools/gen-logo.py`](tools/gen-logo.py) | Regenerates `kolibri-core/src/logo.rs` from the logo master |
| [`tools/gen-oled-preview.py`](tools/gen-oled-preview.py) | Regenerates the README's OLED preview from the real layout |
| [`rust-toolchain.toml`](rust-toolchain.toml) | Pinned toolchain + one target per board |
| [`deny.toml`](deny.toml) | Dependency licence / advisory policy |
| [`.vscode/`](.vscode/) | Settings, tasks, extensions, debug configs |
| [`.github/workflows/ci.yml`](.github/workflows/ci.yml) | fmt, clippy, per-board build, image, cargo-deny |

### Supporting another MCU

[`boards/README.md`](boards/README.md) is the guide: which layers Embassy makes
free (the executor, timers, `Signal`/`Watch`, and every driver that speaks
`embedded-hal-async`), which it does not (the HAL, the arch port and time
driver, the panic handler, the linker script), and the six steps a new board
crate has to cover. The short version is that another Espressif RISC-V chip is
close to free, and a different vendor is a port of `boards/<name>/` alone.

### Changing the temperature source

The reading comes from the ESP32-C3's on-chip sensor, through esp-hal's
`tsens` driver. **It measures the die, not the room.** Espressif's own
documentation notes that the internal temperature runs above ambient, and how
far above depends on clock speed, I/O load and especially radio activity —
10–20 °C over the room is normal on an idle board. esp-hal 1.2 also leaves the
calibration offset hardcoded, so treat the absolute number as indicative and
the *changes* as the meaningful part. That is why the screen labels it `chip`
rather than claiming to be a thermometer.

For actual ambient temperature, hang an I²C sensor off the same `D4`/`D5` bus —
an SHT4x (`0x44`), a BME280 (`0x76`/`0x77`) or an AHT20 (`0x38`); none of them
collide with the panel at `0x3C`. The firmware is arranged so that this is a
small change: implement `kolibri_core::temperature::Source` for the new part and
point the board's `Sensor` type alias at it. Nothing in `kolibri-core` needs
editing. Note that sharing the bus with the display means wrapping the `I2c` in
an `embassy_sync::mutex::Mutex` and handing each task an `I2cDevice`.

`Celsius::from_tenths` is there so a part that reports raw integer ticks never
has to touch a float — these chips have no FPU.

### Changing the pins, or the timings

They live in different places on purpose. **Pins and addresses are board facts**,
at the top of
[`boards/xiao-esp32c3/src/main.rs`](boards/xiao-esp32c3/src/main.rs): the
`peripherals.GPIO10` argument to `Output::new`, the `GPIO6`/`GPIO7` I²C pair,
`DISPLAY_ADDRESS` and `I2C_FREQUENCY`.

**Timings are application policy**, so every board agrees on them and they live
in `kolibri-core`: `blink::PERIOD` and `blink::PERIOD_FAST`,
`heartbeat::PERIOD`, `display::PERIOD` and `display::SPLASH_PERIOD`, and
`temperature::PERIOD`.

---

## Crate versions

The Embassy versions are pinned once in `[workspace.dependencies]`; the
`esp-*` crates are pinned in the board's own manifest, so a second board for
another vendor touches nothing outside its directory.

The esp-rs crates are a **matched set** — `esp-rtos` 0.4 requires `esp-hal` ~1.2
and `embassy-sync` ^0.8. Bumping one alone will not resolve.

| Crate | Version |
|---|---|
| `esp-hal` | 1.2 |
| `esp-rtos` | 0.4 |
| `esp-bootloader-esp-idf` | 0.6 |
| `esp-println` / `esp-backtrace` | 0.18 / 0.20 |
| `embassy-executor` / `-time` / `-sync` | 0.10 / 0.5 / 0.8 |

> **Note on `esp-hal-embassy`:** it no longer exists. As of esp-hal 1.2 the
> Embassy time driver and executor moved into **`esp-rtos`** (`embassy` feature),
> and `#[esp_hal::main] async fn main(spawner: Spawner)` expands into an
> `esp_rtos::embassy::Executor`. Most tutorials and templates online still use
> the old crate and will not compile against esp-hal 1.2.

---

## CI

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on every push and PR.
A `portable core` job runs `cargo fmt --check` and clippy over `kolibri-core`
**for the host target** — the standing check that the portable crate has not
grown a dependency on a HAL. A `board` job then runs clippy and a release build
for each entry in its matrix, one per directory under `boards/`. `cargo-deny`
(advisories, licences, bans, sources) runs alongside. Each board uploads a
flashable `firmware.bin` (bootloader + partition table + app, ~195 KB) as a build
artifact:

```sh
espflash write-bin 0x0 firmware.bin   # flash a CI artifact directly
```

Each board also reports what it costs the chip — the app image against the
partition it has to fit in, and static SRAM against the regions the linker was
given. The report goes to the job summary, into the artifact as
`size-report.md`, and onto the pull request as a comment that is rewritten in
place on every push. Run it yourself with:

```sh
python3 tools/fw-size.py \
    target/riscv32imc-unknown-none-elf/release/kolibri --chip esp32c3
```

### Comparing against main

The same step writes `size-metadata.json`: the measurements, what was measured
(board, chip, target, profile), and the commit, toolchain and CI run behind
them. Every push to `main` publishes it as the `size-baseline-<board>`
artifact, and every pull request downloads the newest one and reports the
change against it — so a PR that costs 4 KB of flash says so, in the comment,
before it is merged.

A baseline is only used when it describes the same board, chip, target and
profile; anything else is ignored with a note rather than subtracted. A missing
baseline — the first run, or one aged out past GitHub's 90-day artifact
retention — drops the change column and nothing else. To compare two builds
locally:

```sh
python3 tools/fw-size.py … --json before.json   # on main
python3 tools/fw-size.py … --baseline before.json
```

Dependabot proposes weekly `cargo` and `github-actions` updates, grouped so the
esp-rs and Embassy crates move together. One `cargo` entry covers the whole
workspace.

---

## Getting this onto a remote

The repo is initialised locally with no remote configured:

```sh
git remote add origin git@github.com:<you>/kolibri.git
git push -u origin main
```

Update the `repository` field in [`Cargo.toml`](Cargo.toml) and the badge URLs
above to match.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: `cargo fmt`,
`cargo clippy -D warnings` on the core and on the board, and flash it to a real
board before opening a PR.

## Licence

Licensed under the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)).
