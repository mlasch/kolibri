//! Blinking an LED, at a rate another task can change while it runs.

use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Ticker};
use embedded_hal::digital::StatefulOutputPin;

/// How long the LED stays in each state.
pub const PERIOD: Duration = Duration::from_millis(500);

/// The faster period [`crate::heartbeat`] alternates to.
pub const PERIOD_FAST: Duration = Duration::from_millis(100);

/// Requests a new blink period from [`run`].
pub static PERIOD_REQUEST: Signal<CriticalSectionRawMutex, Duration> = Signal::new();

/// Toggles `led` forever, at whatever period was last requested.
pub async fn run<P: StatefulOutputPin>(mut led: P) {
    let mut period = PERIOD;
    let mut ticker = Ticker::every(period);

    loop {
        // A period change takes effect immediately, not after the current tick.
        match select(ticker.next(), PERIOD_REQUEST.wait()).await {
            Either::First(()) => match led.toggle() {
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
