//! Async blinky for the Seeed Studio XIAO ESP32-C3.
//!
//! The XIAO ESP32-C3 has **no user-controllable onboard LED** -- the two LEDs on
//! the board are a hardwired power indicator and a battery-charge indicator. Wire
//! an LED and a ~150 Ohm resistor between `D10` and `GND` to see it blink:
//!
//! ```text
//! GPIO10 (D10) --[150 Ohm]--|>|-- GND
//! ```
//!
//! Every transition is also logged over USB serial, so the firmware is
//! verifiable with nothing attached but the USB-C cable.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Ticker};
use esp_backtrace as _;
use esp_hal::{
    gpio::{Level, Output, OutputConfig},
    timer::timg::TimerGroup,
};

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

// The LED pin is selected where `Output::new` is called in `main`, because
// peripheral singletons cannot be named in a `const`. On the XIAO ESP32-C3:
//
//   D10 = GPIO10   <- used here
//   D0..D3 = GPIO2..GPIO5, D4 = GPIO6, D5 = GPIO7,
//   D6 = GPIO21, D7 = GPIO20, D8 = GPIO8, D9 = GPIO9
//
// Avoid GPIO2, GPIO8 and GPIO9: they are strapping pins and driving them at
// boot can put the chip into the wrong boot mode.

/// Requests a new blink period from the [`blink`] task.
///
/// This is the canonical Embassy way to talk to a running task: a `static`
/// `Signal` holding the latest value, with no locking on the reader side. Delete
/// it (and the `select` in [`blink`]) if you only ever need a fixed rate.
static BLINK_PERIOD_REQUEST: Signal<CriticalSectionRawMutex, Duration> = Signal::new();

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

    // A task function returns Err only when its pool is already full, which for
    // a single-instance task spawned once can never happen.
    spawner.spawn(blink(led).expect("blink task pool exhausted"));
    spawner.spawn(heartbeat().expect("heartbeat task pool exhausted"));

    // Nothing left to do here. Returning from an Embassy `main` is fine -- the
    // executor keeps running the spawned tasks. Never busy-wait or call a
    // blocking `Delay` in async context: that would stall every other task.
}
