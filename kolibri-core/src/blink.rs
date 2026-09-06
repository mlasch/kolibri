//! Blinking an LED, at a rate another task can change while it runs.

use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Ticker};
use embedded_hal::digital::StatefulOutputPin;

/// How long the LED stays in each state.
pub const PERIOD: Duration = Duration::from_millis(500);

/// Blink period [`crate::heartbeat`] switches to, to demonstrate retuning a
/// running task from another one.
pub const PERIOD_FAST: Duration = Duration::from_millis(100);

/// Requests a new blink period from [`run`].
///
/// This is the canonical Embassy way to talk to a running task: a `static`
/// `Signal` holding the latest value, with no locking on the reader side. Delete
/// it (and the `select` in [`run`]) if you only ever need a fixed rate.
pub static PERIOD_REQUEST: Signal<CriticalSectionRawMutex, Duration> = Signal::new();

/// Toggles `led` forever, at whatever period was last requested.
///
/// The loop owns its pin by value. There is no `static mut` and no global
/// `Mutex<RefCell<..>>` here, because the pin has exactly one owner -- reach for
/// a shared mutex only when two tasks genuinely need the same peripheral.
///
/// Generic over [`StatefulOutputPin`] rather than over a HAL type, so the same
/// loop drives an `esp_hal::gpio::Output`, an `embassy_stm32::gpio::Output` or
/// a pin behind an I2C expander. The board crate pins the type down when it
/// wraps this in an `#[embassy_executor::task]`.
pub async fn run<P: StatefulOutputPin>(mut led: P) {
    let mut period = PERIOD;
    // `Ticker` schedules against absolute deadlines. A `Timer::after` loop would
    // instead drift by however long each iteration's work takes.
    let mut ticker = Ticker::every(period);

    loop {
        // Waiting on both means a period change takes effect immediately rather
        // than after the current tick has expired.
        match select(ticker.next(), PERIOD_REQUEST.wait()).await {
            Either::First(()) => match led.toggle() {
                // A GPIO toggle is infallible on every HAL worth using, but the
                // trait is fallible for the pins that hang off a bus.
                Ok(()) => log::info!(
                    "led {}",
                    if led.is_set_high().unwrap_or(false) {
                        "on"
                    } else {
                        "off"
                    }
                ),
                Err(e) => log::warn!("led toggle failed: {e:?}"),
            },
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
