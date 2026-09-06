//! kolibri on the Seeed Studio XIAO ESP32-C3.
//!
//! This crate is the board half of the project: the HAL, the entry point, the
//! pin map and the on-chip temperature sensor. The tasks it spawns are all
//! generic loops from [`kolibri_core`], which knows nothing about Espressif --
//! see `boards/README.md` for what a second board has to supply.
//!
//! The SH1106 OLED on `D4`/`D5` shows the chip temperature; an LED on `D10`
//! blinks alongside it to show the tasks really do run concurrently.
//!
//! The XIAO ESP32-C3 has **no user-controllable onboard LED** -- the two LEDs on
//! the board are a hardwired power indicator and a battery-charge indicator. Wire
//! an LED and a ~150 Ohm resistor between `D10` and `GND` to see it blink:
//!
//! ```text
//! GPIO10 (D10) --[150 Ohm]--|>|-- GND
//! ```
//!
//! Everything on the screen is also logged over USB serial, so the firmware is
//! verifiable with nothing attached but the USB-C cable.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::Duration;
use esp_backtrace as _;
use esp_hal::{
    Async,
    gpio::{Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    peripherals::TSENS,
    time::Rate,
    timer::timg::TimerGroup,
    tsens::{Config as TsensConfig, ConfigError, TemperatureSensor},
};
use kolibri_core::{
    display::{Labels, Sh1106I2c},
    temperature::{Celsius, Source},
};

// Emits the esp-idf application descriptor the second-stage bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

// ---------------------------------------------------------------------------
// Board configuration -- the only things you normally need to edit.
//
// Timings are not here: they are application policy and live in kolibri-core
// (`blink::PERIOD`, `display::PERIOD`, `temperature::PERIOD`, ...) so that every
// board agrees on them.
// ---------------------------------------------------------------------------

/// I2C address of the SH1106 module. Almost all of them are `0x3C`; boards with
/// the SA0 jumper bridged answer on `0x3D` instead.
const DISPLAY_ADDRESS: u8 = kolibri_core::display::DEFAULT_ADDRESS;

/// I2C bus speed. The default 100 kHz would take ~100 ms to push a full frame;
/// 400 kHz brings that down to ~26 ms.
const I2C_FREQUENCY: Rate = Rate::from_khz(400);

/// What the screen says it is running on.
const LABELS: Labels = Labels {
    board: "esp32-c3",
    source: <Sensor as Source>::LABEL,
};

/// The fully-applied display type, needed because Embassy tasks cannot take
/// `impl Trait` arguments.
type Display = Sh1106I2c<I2c<'static, Async>>;

/// Where the status screen gets its temperature. Point this at a different
/// [`Source`] impl -- an `SHT4x` or `BME280` on the same `D4`/`D5` bus, say --
/// to change sensors; nothing in `kolibri-core` needs editing.
type Sensor = InternalSensor;

// The LED pin is selected where `Output::new` is called in `main`, because
// peripheral singletons cannot be named in a `const`. On the XIAO ESP32-C3:
//
//   D10 = GPIO10   <- LED, used here
//   D4 = GPIO6     <- I2C SDA, used here
//   D5 = GPIO7     <- I2C SCL, used here
//   D0..D3 = GPIO2..GPIO5, D6 = GPIO21, D7 = GPIO20,
//   D8 = GPIO8, D9 = GPIO9
//
// Avoid GPIO2, GPIO8 and GPIO9: they are strapping pins and driving them at
// boot can put the chip into the wrong boot mode.

// ---------------------------------------------------------------------------
// Temperature source
// ---------------------------------------------------------------------------

/// The ESP32-C3's built-in temperature sensor.
///
/// **This is die temperature, not room temperature.** Espressif's own docs note
/// that the internal reading runs above ambient, and how far above depends on
/// clock speed, I/O load and especially radio activity -- 10-20 C over the room
/// is normal on an idle board. esp-hal 1.2 also leaves the calibration offset
/// hardcoded (`tsens.rs` still carries a `TODO Address multiple temperature
/// ranges and offsets`), so the absolute number is indicative and the *changes*
/// are what mean something. Hence the `chip` label on screen: the display
/// should not claim to be a thermometer.
struct InternalSensor {
    sensor: TemperatureSensor<'static>,
}

impl InternalSensor {
    /// Powers the sensor up. `ConfigError` is an empty enum today so this cannot
    /// actually fail, but it is `#[non_exhaustive]` and so stays a `Result`.
    fn new(peripheral: TSENS<'static>) -> Result<Self, ConfigError> {
        Ok(Self {
            sensor: TemperatureSensor::new(peripheral, TsensConfig::default())?,
        })
    }
}

impl Source for InternalSensor {
    const LABEL: &'static str = "chip";
    /// The TRM asks for a few hundred microseconds of settling after power-up.
    const WARMUP: Duration = Duration::from_micros(200);

    // Nothing to await for a register read, but the trait has to stay async for
    // the I2C sensors this is meant to be swappable with.
    #[allow(clippy::unused_async_trait_impl)]
    async fn read(&mut self) -> Option<Celsius> {
        Some(Celsius::from_degrees(
            self.sensor.get_temperature().to_celsius(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tasks
//
// One line each: `#[embassy_executor::task]` needs a concrete type to size the
// task's static storage, which is why the loops themselves are generic
// functions in kolibri-core rather than tasks.
// ---------------------------------------------------------------------------

/// Blinks the LED on `D10`.
#[embassy_executor::task]
async fn blink(led: Output<'static>) {
    kolibri_core::blink::run(led).await;
}

/// Logs a heartbeat and retunes the blink rate.
#[embassy_executor::task]
async fn heartbeat() {
    kolibri_core::heartbeat::run().await;
}

/// Samples the on-chip sensor.
///
/// Add `#[embassy_executor::task(pool_size = 2)]` if you ever need two
/// instances of a task; the default pool holds exactly one.
#[embassy_executor::task]
async fn temperature(sensor: Sensor) {
    kolibri_core::temperature::run(sensor).await;
}

/// Draws the splash and then the status screen.
#[embassy_executor::task]
async fn display(panel: Display) {
    kolibri_core::display::run(panel, LABELS).await;
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Brings the chip up, starts the scheduler, and spawns the application tasks.
///
/// Because `main` is `async`, `#[esp_hal::main]` wraps it in an
/// `esp_rtos::embassy::Executor` running on the main thread. On a non-Espressif
/// HAL this would be `#[embassy_executor::main]` instead, and the time driver
/// would come from the HAL's own `time-driver-*` feature rather than from a
/// call like `esp_rtos::start`.
#[esp_hal::main]
async fn main(spawner: Spawner) {
    // Reads the ESP_LOG env var baked in at build time (see .cargo/config.toml).
    esp_println::logger::init_logger_from_env();

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // The scheduler needs a hardware timer and the FROM_CPU_INTR0 software
    // interrupt. This also installs the embassy-time driver.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    log::info!("kolibri starting on XIAO ESP32-C3");

    // D10 on the XIAO silkscreen. Starts low, so a fresh boot begins with the
    // LED off.
    let led = Output::new(peripherals.GPIO10, Level::Low, OutputConfig::default());

    // D4/D5 on the silkscreen. `into_async` is what makes the display driver
    // await its transfers instead of spinning on the bus, so a ~26 ms frame
    // push does not stall `blink`.
    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(I2C_FREQUENCY),
    )
    .expect("i2c config rejected")
    .with_sda(peripherals.GPIO6)
    .with_scl(peripherals.GPIO7)
    .into_async();

    // esp-hal's `I2c<Async>` implements `embedded_hal_async::i2c::I2c`, which is
    // all kolibri-core asks for.
    let oled: Display = kolibri_core::display::sh1106_i2c(i2c, DISPLAY_ADDRESS);
    log::info!("display on 0x{DISPLAY_ADDRESS:02x}");

    // The on-chip sensor is an ordinary peripheral singleton, so pointing
    // `Sensor` at an external part means building that here instead and handing
    // it to `temperature`.
    let sensor = Sensor::new(peripherals.TSENS).expect("temperature sensor config rejected");

    // A task function returns Err only when its pool is already full, which for
    // a single-instance task spawned once can never happen.
    spawner.spawn(blink(led).expect("blink task pool exhausted"));
    spawner.spawn(heartbeat().expect("heartbeat task pool exhausted"));
    spawner.spawn(temperature(sensor).expect("temperature task pool exhausted"));
    spawner.spawn(display(oled).expect("display task pool exhausted"));

    // Nothing left to do here. Returning from an Embassy `main` is fine -- the
    // executor keeps running the spawned tasks. Never busy-wait or call a
    // blocking `Delay` in async context: that would stall every other task.
}
