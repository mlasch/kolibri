//! A liveness log line, and proof that tasks really do run concurrently.

use embassy_time::{Duration, Instant, Ticker};

use crate::blink;

/// How often the heartbeat reports in.
pub const PERIOD: Duration = Duration::from_secs(5);

/// Logs that the executor is alive, and alternates the blink rate.
///
/// Its only real job is to prove concurrency: it sleeps for seconds at a time
/// without ever holding up [`blink::run`].
pub async fn run() {
    let mut ticker = Ticker::every(PERIOD);
    let mut fast = false;

    loop {
        ticker.next().await;
        fast = !fast;
        log::info!("heartbeat (uptime {} s)", Instant::now().as_secs());
        blink::PERIOD_REQUEST.signal(if fast {
            blink::PERIOD_FAST
        } else {
            blink::PERIOD
        });
    }
}
