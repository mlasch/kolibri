//! Reading a temperature from wherever the board happens to keep one.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use embassy_time::{Duration, Ticker, Timer};

/// How often the source is sampled.
pub const PERIOD: Duration = Duration::from_secs(2);

/// The most recent reading. A `Watch` so readers can peek without consuming it.
pub static LATEST: Watch<CriticalSectionRawMutex, Celsius, 1> = Watch::new();

/// A temperature in tenths of a degree Celsius; integer to avoid float formatting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Celsius(i16);

impl Celsius {
    /// Wraps a reading that is already in tenths of a degree.
    #[must_use]
    pub const fn from_tenths(tenths: i16) -> Self {
        Self(tenths)
    }

    /// Converts from degrees, truncating toward zero.
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

/// Something that can be asked for a temperature. `async` so I2C parts fit too.
// Single-threaded executor, so no `Send` bounds needed.
#[allow(async_fn_in_trait)]
pub trait Source {
    /// Short on-screen label for what the reading describes.
    const LABEL: &'static str;

    /// Settling time after power-up before the first read.
    const WARMUP: Duration;

    /// Takes one reading; `None` if it failed (the caller retries).
    async fn read(&mut self) -> Option<Celsius>;
}

/// Samples `source` every [`PERIOD`] and publishes each reading to [`LATEST`].
pub async fn run<S: Source>(mut source: S) {
    let sender = LATEST.sender();

    Timer::after(S::WARMUP).await;

    let mut ticker = Ticker::every(PERIOD);

    loop {
        match source.read().await {
            Some(reading) => {
                log::info!("{} temperature {reading} C", S::LABEL);
                sender.send(reading);
            }
            None => log::warn!("temperature read failed"),
        }
        ticker.next().await;
    }
}
