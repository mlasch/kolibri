//! Reading a temperature from wherever the board happens to keep one.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use embassy_time::{Duration, Ticker, Timer};

/// How often the source is sampled. Deliberately not the same as
/// [`crate::display::PERIOD`]: sampling a sensor and repainting a panel are
/// separate jobs, and neither should be pinned to the other's rate.
pub const PERIOD: Duration = Duration::from_secs(2);

/// The most recent reading, written by [`run`] and read by
/// [`crate::display::run`].
///
/// A `Watch` rather than a `Signal` because a redraw wants *the current value*,
/// not a one-shot notification: `Signal::wait` consumes what it returns, so the
/// reading would vanish from the frame after next. `Watch::try_get` only looks.
pub static LATEST: Watch<CriticalSectionRawMutex, Celsius, 1> = Watch::new();

/// A temperature, in tenths of a degree Celsius.
///
/// Integer tenths rather than `f32` on purpose: most microcontrollers this
/// targets have no FPU, and `core`'s float formatter is several KiB of flash to
/// print a number this screen shows to one decimal place anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Celsius(i16);

impl Celsius {
    /// Wraps a reading that is already in tenths of a degree.
    ///
    /// Most I2C parts report a raw integer that scales to exactly this, so a
    /// [`Source`] impl can usually avoid touching a float at all.
    #[must_use]
    pub const fn from_tenths(tenths: i16) -> Self {
        Self(tenths)
    }

    /// Truncates toward zero, which is well inside any of these sensors'
    /// accuracy.
    #[must_use]
    pub fn from_degrees(degrees: f32) -> Self {
        Self((degrees * 10.0) as i16)
    }

    /// The reading in tenths of a degree.
    #[must_use]
    pub const fn tenths(self) -> i16 {
        self.0
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
/// This is the seam that makes the temperature portable. The on-chip sensor is
/// a different peripheral on every family -- a dedicated `tsens` block on the
/// ESP32-C3, an ADC channel on STM32 and RP2040, a `Temp` peripheral on nRF --
/// and an external I2C part is different again. A board implements this once
/// and [`run`] is unchanged.
///
/// `read` is `async` for that reason: an on-chip sensor is a register read, but
/// an I2C part is a bus transaction.
// Auto trait bounds cannot be named on an `async fn` in a trait, which the
// compiler warns about for public traits. It does not matter here: Embassy
// executors on these targets are single-threaded, so nothing needs `Send`.
#[allow(async_fn_in_trait)]
pub trait Source {
    /// What the reading actually describes. It goes on the status screen, so
    /// keep it short and keep it honest.
    const LABEL: &'static str;

    /// How long to wait after power-up before the first reading is trustworthy.
    const WARMUP: Duration;

    /// Returns `None` if this particular reading could not be taken. A source is
    /// expected to survive that, so the caller retries instead of giving up.
    async fn read(&mut self) -> Option<Celsius>;
}

/// Samples `source` and publishes each reading to [`LATEST`].
///
/// Kept out of [`crate::display::run`] so the sampling and redraw rates stay
/// independent, and so a sensor that stalls cannot take the screen down with
/// it. An I2C sensor that shares the display's bus needs the bus wrapped in an
/// `embassy_sync::mutex::Mutex`, with each task handed an
/// `embassy_embedded_hal::shared_bus::asynch::i2c::I2cDevice`.
pub async fn run<S: Source>(mut source: S) {
    let sender = LATEST.sender();

    // Sensors need a moment after power-up before the first reading means
    // anything. Awaited, so the other tasks keep running through it.
    Timer::after(S::WARMUP).await;

    let mut ticker = Ticker::every(PERIOD);

    loop {
        match source.read().await {
            Some(reading) => {
                log::info!("{} temperature {reading} C", S::LABEL);
                sender.send(reading);
            }
            // Leaves the last good value on screen rather than blanking it.
            None => log::warn!("temperature read failed"),
        }
        ticker.next().await;
    }
}
