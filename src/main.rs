//! Async firmware for the Seeed Studio XIAO ESP32-C3.
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

use core::fmt::Write;

use display_interface_i2c::I2CInterface;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal, watch::Watch};
use embassy_time::{Duration, Ticker, Timer};
use embedded_graphics::{
    image::Image,
    mono_font::{MonoTextStyle, ascii::FONT_6X10, iso_8859_1::FONT_10X20},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
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
use oled_async::{Builder, displays::sh1106::Sh1106_128_64, mode::GraphicsMode};

mod logo;

// Emits the esp-idf application descriptor the second-stage bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

// ---------------------------------------------------------------------------
// Board configuration -- the only things you normally need to edit.
// ---------------------------------------------------------------------------

/// How long the LED stays in each state.
const BLINK_PERIOD: Duration = Duration::from_millis(500);

/// Blink period the heartbeat task switches to, to demonstrate retuning a
/// running task from another one.
const BLINK_PERIOD_FAST: Duration = Duration::from_millis(100);

/// How often the heartbeat task reports in.
const HEARTBEAT_PERIOD: Duration = Duration::from_secs(5);

/// How often the OLED is redrawn. Every redraw pushes the whole 1 KiB frame
/// over I2C, so this is the main cost of having a display attached.
const DISPLAY_PERIOD: Duration = Duration::from_secs(1);

/// How long the boot splash stays up before the status screen replaces it.
const SPLASH_PERIOD: Duration = Duration::from_secs(2);

/// How often the temperature source is sampled. Deliberately not the same as
/// [`DISPLAY_PERIOD`]: sampling a sensor and repainting a panel are separate
/// jobs, and neither should be pinned to the other's rate.
const TEMPERATURE_PERIOD: Duration = Duration::from_secs(2);

/// I2C address of the SH1106 module. Almost all of them are `0x3C`; boards with
/// the SA0 jumper bridged answer on `0x3D` instead.
const DISPLAY_ADDRESS: u8 = 0x3C;

/// SH1106 control byte that marks the following bytes as pixel data rather than
/// commands.
const DISPLAY_DATA_BYTE: u8 = 0x40;

/// I2C bus speed. The default 100 kHz would take ~100 ms to push a full frame;
/// 400 kHz brings that down to ~26 ms.
const I2C_FREQUENCY: Rate = Rate::from_khz(400);

/// Framebuffer size: one bit per pixel over a 128x64 panel.
///
/// Passed explicitly because `GraphicsMode`'s default is sized for a 160x160
/// panel and would waste 2 KiB of RAM on this one.
const DISPLAY_BUFFER_BYTES: usize = 128 * 64 / 8;

/// The fully-applied display type, needed because Embassy tasks cannot take
/// `impl Trait` arguments.
type Display = GraphicsMode<Sh1106_128_64, I2CInterface<I2c<'static, Async>>, DISPLAY_BUFFER_BYTES>;

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

/// Requests a new blink period from the [`blink`] task.
///
/// This is the canonical Embassy way to talk to a running task: a `static`
/// `Signal` holding the latest value, with no locking on the reader side. Delete
/// it (and the `select` in [`blink`]) if you only ever need a fixed rate.
static BLINK_PERIOD_REQUEST: Signal<CriticalSectionRawMutex, Duration> = Signal::new();

/// The most recent temperature reading, written by [`temperature`] and read by
/// [`display`].
///
/// A `Watch` rather than a `Signal` because a redraw wants *the current value*,
/// not a one-shot notification: `Signal::wait` consumes what it returns, so the
/// reading would vanish from the frame after next. `Watch::try_get` only looks.
static TEMPERATURE: Watch<CriticalSectionRawMutex, Celsius, 1> = Watch::new();

/// A fixed-capacity sink for `write!`, so text can be formatted on the stack.
///
/// `heapless::String` does the same job if you would rather take the extra
/// dependency; this exists to keep the tree at four display crates instead of
/// five. Writes past `N` bytes are dropped rather than panicking.
struct TextBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> TextBuf<N> {
    const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        // Only whole `&str` chunks are ever appended, so the prefix is valid
        // UTF-8 by construction.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> Write for TextBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = N - self.len;
        if s.len() > room {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Temperature
// ---------------------------------------------------------------------------

/// A temperature, in tenths of a degree Celsius.
///
/// Integer tenths rather than `f32` on purpose: the C3 has no FPU, and `core`'s
/// float formatter is several KiB of flash to print a number this screen shows
/// to one decimal place anyway.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Celsius(i16);

impl Celsius {
    /// Truncates toward zero, which is well inside the sensor's accuracy.
    fn from_degrees(degrees: f32) -> Self {
        Self((degrees * 10.0) as i16)
    }
}

impl core::fmt::Display for Celsius {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let whole = self.0 / 10;
        let frac = (self.0 % 10).unsigned_abs();
        // Integer division drops the sign between -0.9 and 0.0, so write it out.
        let sign = if self.0 < 0 && whole == 0 { "-" } else { "" };
        write!(f, "{sign}{whole}.{frac}")
    }
}

/// Something that can be asked for a temperature.
///
/// The point of the trait is that [`Source`] is the only line that has to
/// change to read from somewhere else -- an `SHT4x` or `BME280` hanging off the
/// same D4/D5 bus, say. `read` is `async` for that reason: the on-chip sensor
/// is a register read, but an I2C part is not.
trait TemperatureSource {
    /// What the reading actually describes. It goes on the status screen, so
    /// keep it short and keep it honest.
    const LABEL: &'static str;

    /// How long to wait after power-up before the first reading is trustworthy.
    const WARMUP: Duration;

    /// Returns `None` if this particular reading could not be taken. A source is
    /// expected to survive that, so the caller retries instead of giving up.
    async fn read(&mut self) -> Option<Celsius>;
}

/// Where the status screen gets its temperature. Point this at a different
/// [`TemperatureSource`] impl to change sensors; nothing else needs editing.
type Source = InternalSensor;

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

impl TemperatureSource for InternalSensor {
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
// ---------------------------------------------------------------------------

/// Toggles the LED forever, at whatever period was last requested.
///
/// The task owns its `Output` by value. There is no `static mut` and no global
/// `Mutex<RefCell<..>>` here, because the pin has exactly one owner -- reach for
/// a shared mutex only when two tasks genuinely need the same peripheral.
///
/// Add `#[embassy_executor::task(pool_size = 4)]` if you need several instances
/// of a task; the default pool holds exactly one.
#[embassy_executor::task]
async fn blink(mut led: Output<'static>) {
    let mut period = BLINK_PERIOD;
    // `Ticker` schedules against absolute deadlines. A `Timer::after` loop would
    // instead drift by however long each iteration's work takes.
    let mut ticker = Ticker::every(period);

    loop {
        // Waiting on both means a period change takes effect immediately rather
        // than after the current tick has expired.
        match select(ticker.next(), BLINK_PERIOD_REQUEST.wait()).await {
            Either::First(()) => {
                led.toggle();
                log::info!("led {}", if led.is_set_high() { "on" } else { "off" });
            }
            Either::Second(new_period) => {
                if new_period != period {
                    period = new_period;
                    ticker = Ticker::every(period);
                    log::info!("blink period -> {} ms", period.as_millis());
                }
            }
        }
    }
}

/// Logs that the executor is alive, and alternates the blink rate.
///
/// Its only real job is to prove that tasks run concurrently: it sleeps for
/// seconds at a time without ever holding up [`blink`].
#[embassy_executor::task]
async fn heartbeat() {
    let mut ticker = Ticker::every(HEARTBEAT_PERIOD);
    let mut fast = false;

    loop {
        ticker.next().await;
        fast = !fast;
        log::info!(
            "heartbeat (uptime {} s)",
            embassy_time::Instant::now().as_secs()
        );
        BLINK_PERIOD_REQUEST.signal(if fast {
            BLINK_PERIOD_FAST
        } else {
            BLINK_PERIOD
        });
    }
}

/// Samples the temperature source and publishes each reading to [`TEMPERATURE`].
///
/// Kept out of [`display`] so the sampling and redraw rates stay independent,
/// and so a sensor that stalls cannot take the screen down with it. Swapping in
/// an I2C part means it has to share the bus with the panel: wrap the `I2c` in
/// an `embassy_sync::mutex::Mutex` and hand each task an `I2cDevice` at that
/// point.
#[embassy_executor::task]
async fn temperature(mut source: Source) {
    let sender = TEMPERATURE.sender();

    // Sensors need a moment after power-up before the first reading means
    // anything. Awaited, so the other tasks keep running through it.
    Timer::after(Source::WARMUP).await;

    let mut ticker = Ticker::every(TEMPERATURE_PERIOD);

    loop {
        match source.read().await {
            Some(reading) => {
                log::info!("{} temperature {reading} C", Source::LABEL);
                sender.send(reading);
            }
            // Leaves the last good value on screen rather than blanking it.
            None => log::warn!("temperature read failed"),
        }
        ticker.next().await;
    }
}

/// Draws the boot splash -- the hummingbird the project is named after -- and
/// leaves it up for [`SPLASH_PERIOD`].
///
/// The bitmap is 1 bit per pixel with a set bit meaning a *lit* pixel, so the
/// bird glows against the panel's own black rather than being punched out of a
/// lit rectangle. That is the whole reason the artwork is a silhouette: on a
/// two-colour panel there is no grey to model shape with, so the outline has to
/// carry it. See [`logo`], generated from `assets/kolibri.svg` by
/// `tools/gen-logo.py`.
async fn splash(display: &mut Display) {
    let text = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);

    display.clear();

    let drawn = Image::new(&logo::LARGE, Point::new(4, 8))
        .draw(display)
        .and_then(|()| {
            Text::with_baseline("kolibri", Point::new(76, 20), text, Baseline::Top)
                .draw(display)
                .map(|_| ())
        })
        .and_then(|()| {
            Text::with_baseline("esp32-c3", Point::new(76, 34), text, Baseline::Top)
                .draw(display)
                .map(|_| ())
        });

    if let Err(e) = drawn {
        log::warn!("splash draw failed: {e:?}");
        return;
    }
    if let Err(e) = display.flush().await {
        log::warn!("splash flush failed: {e:?}");
        return;
    }

    // A `Timer`, not a blocking delay: `blink` and `heartbeat` keep running for
    // the two seconds the splash is up.
    Timer::after(SPLASH_PERIOD).await;
}

/// Renders the temperature to the OLED once per second.
///
/// Redrawing is a two-stage affair: `embedded-graphics` calls mutate an
/// in-memory framebuffer, and nothing reaches the panel until `flush`. Drawing
/// into RAM is infallible here, so only the I2C transfers can actually fail.
#[embassy_executor::task]
async fn display(mut display: Display) {
    let text = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    // The reading gets the big font, and an ISO 8859-1 one rather than ASCII so
    // that the degree sign is a real glyph instead of the replacement box. That
    // costs 4.8 KiB of flash against 2.4 KiB for the ASCII sheet; the rest of
    // the screen stays on the 720-byte 6x10 ASCII font.
    let reading = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let border = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // `init` is where a wrong I2C address or unpowered panel shows up, so it is
    // worth failing loudly rather than drawing into the void for ever.
    if let Err(e) = display.init().await {
        log::error!("display init failed: {e:?} (wrong address? check SDA/SCL)");
        return;
    }
    log::info!("display ready on 0x{DISPLAY_ADDRESS:02x}");

    splash(&mut display).await;

    let mut ticker = Ticker::every(DISPLAY_PERIOD);

    loop {
        ticker.next().await;

        display.clear();

        // `try_get` reads the latest reading without consuming it, so every
        // frame sees it until a newer one arrives. `None` only until the first
        // sample lands, which happens during the splash -- so in practice the
        // placeholder is only ever seen if the very first read fails.
        let mut value = TextBuf::<16>::new();
        let _ = match TEMPERATURE.try_get() {
            Some(celsius) => write!(value, "{celsius} \u{00b0}C"),
            None => write!(value, "--.- \u{00b0}C"),
        };

        let mut status = TextBuf::<24>::new();
        let _ = write!(
            status,
            "{}  up {} s",
            Source::LABEL,
            embassy_time::Instant::now().as_secs()
        );

        // Layout on the 128x64 panel, everything inside the 1 px border:
        //   y  3..33  the bird, x 86..126
        //   y  4..14  "kolibri"
        //   y 20..40  the reading -- at most 8 chars at 10 px, so x 5..85,
        //             which stays clear of the bird even at "-12.3 C"
        //   y 46..56  source label and uptime
        //
        // Drawing only touches the framebuffer, so these cannot fail in
        // practice -- but embedded-graphics is generic over targets that can.
        let drawn = Rectangle::new(Point::zero(), Size::new(128, 64))
            .into_styled(border)
            .draw(&mut display)
            .and_then(|()| Image::new(&logo::SMALL, Point::new(86, 3)).draw(&mut display))
            .and_then(|()| {
                Text::with_baseline("kolibri", Point::new(5, 4), text, Baseline::Top)
                    .draw(&mut display)
                    .map(|_| ())
            })
            .and_then(|()| {
                Text::with_baseline(value.as_str(), Point::new(5, 20), reading, Baseline::Top)
                    .draw(&mut display)
                    .map(|_| ())
            })
            .and_then(|()| {
                Text::with_baseline(status.as_str(), Point::new(5, 46), text, Baseline::Top)
                    .draw(&mut display)
                    .map(|_| ())
            });

        if let Err(e) = drawn {
            log::warn!("display draw failed: {e:?}");
            continue;
        }

        // The only genuinely fallible step. A yanked cable shows up here, and a
        // transient failure should not kill the task.
        if let Err(e) = display.flush().await {
            log::warn!("display flush failed: {e:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Brings up the chip, starts the scheduler, and spawns the application tasks.
///
/// Because `main` is `async`, `#[esp_hal::main]` wraps it in an
/// `esp_rtos::embassy::Executor` running on the main thread.
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

    // The SH1106 has 132 columns of RAM behind a 128 px panel; `Sh1106_128_64`
    // carries the resulting COLUMN_OFFSET of 2 so the image is not shifted.
    let oled: Display = Builder::new(Sh1106_128_64 {})
        .connect(I2CInterface::new(i2c, DISPLAY_ADDRESS, DISPLAY_DATA_BYTE))
        .into();

    // The on-chip sensor is an ordinary peripheral singleton, so pointing
    // `Source` at an external sensor means building that here instead and
    // handing it to `temperature`.
    let sensor = Source::new(peripherals.TSENS).expect("temperature sensor config rejected");

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
