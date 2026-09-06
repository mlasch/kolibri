# Boards

One crate per board. Each owns everything that is genuinely chip-specific — the
HAL, the entry point, the pin map, the temperature sensor, the target triple and
the flashing runner — and nothing else. The application itself lives once, in
[`kolibri-core`](../kolibri-core), written against traits.

| Board | MCU | Target | Toolchain |
|---|---|---|---|
| [`xiao-esp32c3`](xiao-esp32c3) | ESP32-C3 (RISC-V) | `riscv32imc-unknown-none-elf` | stable |

**Build from inside the board directory.** Cargo discovers `.cargo/config.toml`
by walking up from the current directory, and that file is what selects the
target and the runner:

```sh
cd boards/xiao-esp32c3
cargo build --release
cargo run --release        # flash + monitor
```

At the repository root, a bare `cargo build` builds only `kolibri-core`, for the
host. That is deliberate: it is the cheapest possible check that the portable
crate has not grown a dependency on a HAL.

---

## What Embassy makes free, and what it does not

`embassy-executor`, `embassy-time`, `embassy-sync` and `embassy-futures` are
chip-independent. Embassy's contract with a HAL is small:

- a [`critical-section`](https://crates.io/crates/critical-section)
  implementation,
- a time driver registered through `embassy_time_driver::time_driver_impl!`,
- an architecture port for the executor.

Every conforming HAL then hands you peripherals implementing the
`embedded-hal-async` traits — which is exactly what `kolibri-core` consumes. So
the layers stack like this:

| Layer | Portable | Where it lives |
|---|---|---|
| tasks, drawing, sensor trait | yes | `kolibri-core` |
| `embassy-{executor,time,sync,futures}` | yes | workspace dependencies |
| `embedded-graphics`, `oled_async`, `display-interface-i2c` | yes | `kolibri-core` |
| arch port + time driver | **no** | board crate |
| HAL / PAC | **no** | board crate |
| panic handler, logging, boot descriptor | **no** | board crate |
| target triple, linker script, runner | **no** | board `.cargo/config.toml` |

---

## Adding a board

### 1. Create the crate

```sh
mkdir -p boards/<name>/{src,.cargo}
```

`boards/*` is a glob in the workspace `members`, so the new directory is picked
up with no edit to the root manifest. Give the package a distinct name and keep
the binary named `kolibri`, so `target/<triple>/<profile>/kolibri` stays the
path that espflash, probe-rs and `.vscode/launch.json` expect:

```toml
[[bin]]
name = "kolibri"
path = "src/main.rs"
```

Shared crates come from `[workspace.dependencies]`; the HAL and its runtime
support are pinned in the board's own manifest, so adding a board touches
nothing outside its directory.

### 2. Write `.cargo/config.toml`

Target triple, linker arguments and the `cargo run` runner. Note that
`-C force-frame-pointers`-style codegen flags have to be `rustflags` here; only
plain link arguments could move into a `build.rs`.

### 3. Wire up the executor

On Espressif, `esp-rtos` supplies the arch port and the time driver, and
`#[esp_hal::main]` expands into its executor — which is why the XIAO board
enables **no** `arch-*` feature on `embassy-executor`. Everywhere else it is the
other way round:

```toml
embassy-executor = { workspace = true, features = ["arch-cortex-m", "executor-thread"] }
embassy-stm32    = { version = "...", features = ["time-driver-any", ...] }
```

and the entry point becomes `#[embassy_executor::main]`, which hands you the
same `Spawner`.

### 4. Declare the tasks

`#[embassy_executor::task]` needs a concrete type to size the task's static
storage, so task functions **cannot be generic** — the macro rejects them with
"task functions must not be generic". That is why the loops in `kolibri-core`
are ordinary generic `async fn`s and each board wraps them:

```rust
#[embassy_executor::task]
async fn blink(led: Output<'static>) {
    kolibri_core::blink::run(led).await;
}
```

The four loops are `blink::run`, `heartbeat::run`, `temperature::run` and
`display::run`.

### 5. Implement `temperature::Source`

The one piece of hardware with no portable equivalent. The on-chip sensor is a
dedicated `tsens` block on the ESP32-C3, an ADC channel on STM32 and RP2040, and
a `Temp` peripheral on nRF; an external I²C part is different again. Implement
the trait, point the board's `Sensor` alias at it, and nothing else changes.

`Celsius::from_tenths` exists so an I²C part that reports raw integer ticks
never has to touch a float.

### 6. Build the panel

If the board carries the same SH1106 on I²C, `kolibri_core::display::sh1106_i2c`
takes any `embedded_hal_async::i2c::I2c` and returns the panel. A different
controller or a different bus means implementing `display::Panel` — three
methods over an `embedded_graphics::DrawTarget` — and the drawing code is
unchanged.

### 7. Register the board

- `rust-toolchain.toml` — add the target triple.
- `deny.toml` — add the triple to `[graph] targets`.
- `.github/workflows/ci.yml` — add a row to the `board` matrix.
- The table at the top of this file.

---

## Porting cost, honestly

**Another Espressif RISC-V chip** (C2, C6, H2) is close to free: swap the chip
feature on the five `esp-*` crates, change the triple (C6 and H2 are
`riscv32imac-unknown-none-elf`), and fix the pin numbers.

**Xtensa parts** (ESP32, S2, S3) work, but need the `espup` toolchain and
`-Z build-std`. That costs the project its "builds on stable Rust" property, so
`rust-toolchain.toml` could no longer pin one channel for everybody.

**A different vendor** is the real port, and it is the six steps above.
Everything from `embassy-sync` upwards — the `Signal`, the `Watch`, `Ticker`,
`select`, the SH1106 driver, the whole screen layout — carries over untouched.
