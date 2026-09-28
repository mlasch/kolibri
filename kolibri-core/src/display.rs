//! The status screen, and the panel abstraction that keeps it portable.

use core::fmt::Write;

use display_interface::{AsyncWriteOnlyDataCommand, DisplayError};
use display_interface_i2c::I2CInterface;
use embassy_time::{Duration, Instant, Ticker, Timer};
use embedded_graphics::{
    draw_target::DrawTarget,
    image::{Image, ImageRaw},
    mono_font::{MonoTextStyle, ascii::FONT_6X10, iso_8859_1::FONT_10X20},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
use oled_async::{Builder, display::DisplayVariant, displays::sh1106::Sh1106_128_64, prelude::*};

use crate::{
    flight::{Facing, Rng, glide, wingbeat},
    logo, temperature,
    text::TextBuf,
};

/// How often the panel is redrawn.
pub const PERIOD: Duration = Duration::from_secs(1);

/// How long the boot splash stays up.
pub const SPLASH_PERIOD: Duration = Duration::from_secs(2);

/// How far the bird moves per animation frame, in pixels.
pub const FLIGHT_STEP: i32 = 4;

/// Time between animation frames; a full-frame flush takes ~26 ms at 400 kHz.
pub const FLIGHT_FRAME: Duration = Duration::from_millis(40);

/// Shortest rest on the status screen between excursions.
pub const REST_MIN: Duration = Duration::from_secs(5);

/// Longest rest on the status screen between excursions.
pub const REST_MAX: Duration = Duration::from_secs(10);

/// How many random waypoints an excursion visits before flying home.
pub const HOPS: (i32, i32) = (2, 4);

/// Framebuffer size of a 128x64 1-bpp panel (`GraphicsMode`'s default is larger).
pub const BUFFER_BYTES_128X64: usize = 128 * 64 / 8;

/// I2C address of a stock SH1106 module (`0x3D` with SA0 bridged).
pub const DEFAULT_ADDRESS: u8 = 0x3C;

/// SH1106 control byte marking the following bytes as pixel data.
pub const DATA_BYTE: u8 = 0x40;

const SMALL: MonoTextStyle<'static, BinaryColor> = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
// ISO 8859-1 so the degree sign is a real glyph.
const LARGE: MonoTextStyle<'static, BinaryColor> = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
const BORDER: PrimitiveStyle<BinaryColor> = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

const PANEL_WIDTH: i32 = 128;
const SPLASH_BIRD: Point = Point::new(4, 8);
const SPLASH_BIRD_WIDTH: i32 = 64;
const STATUS_BIRD: Point = Point::new(86, 3);

/// A framebuffered monochrome panel that has to be told when to push a frame.
///
/// Any `oled_async` display implements it through the blanket impl below.
// Single-threaded executor, so no `Send` bounds needed.
#[allow(async_fn_in_trait)]
pub trait Panel: DrawTarget<Color = BinaryColor, Error: core::fmt::Debug> {
    /// How the panel reports a failed transfer.
    type BusError: core::fmt::Debug;

    /// Brings the controller up.
    async fn init(&mut self) -> Result<(), Self::BusError>;

    /// Blanks the framebuffer.
    fn clear_buffer(&mut self);

    /// Pushes the framebuffer to the panel.
    async fn flush(&mut self) -> Result<(), Self::BusError>;
}

impl<DV, DI, const BS: usize> Panel for GraphicsMode<DV, DI, BS>
where
    DI: AsyncWriteOnlyDataCommand,
    DV: DisplayVariant,
{
    type BusError = DisplayError;

    async fn init(&mut self) -> Result<(), Self::BusError> {
        GraphicsMode::init(self).await
    }

    fn clear_buffer(&mut self) {
        GraphicsMode::clear(self);
    }

    async fn flush(&mut self) -> Result<(), Self::BusError> {
        GraphicsMode::flush(self).await
    }
}

/// The 128x64 SH1106 on an I2C bus.
pub type Sh1106I2c<I2C> = GraphicsMode<Sh1106_128_64, I2CInterface<I2C>, BUFFER_BYTES_128X64>;

/// Builds the SH1106 panel on any async I2C bus.
#[must_use]
pub fn sh1106_i2c<I2C>(i2c: I2C, address: u8) -> Sh1106I2c<I2C>
where
    I2C: embedded_hal_async::i2c::I2c,
{
    Builder::new(Sh1106_128_64 {})
        .connect(I2CInterface::new(i2c, address, DATA_BYTE))
        .into()
}

/// The board-specific strings the screen shows.
#[derive(Clone, Copy, Debug)]
pub struct Labels {
    /// Short board or chip name for the boot splash, e.g. `"esp32-c3"`.
    pub board: &'static str,
    /// What the temperature describes, e.g. `"chip"`.
    pub source: &'static str,
}

fn text<D: Panel>(
    display: &mut D,
    s: &str,
    at: Point,
    style: MonoTextStyle<'static, BinaryColor>,
) -> Result<(), D::Error> {
    Text::with_baseline(s, at, style, Baseline::Top)
        .draw(display)
        .map(|_| ())
}

/// Draws a frame and pushes it to the panel, logging any failure.
async fn show<D: Panel>(
    display: &mut D,
    draw: impl FnOnce(&mut D) -> Result<(), D::Error>,
) -> bool {
    display.clear_buffer();
    if let Err(e) = draw(display) {
        log::warn!("display draw failed: {e:?}");
        return false;
    }
    if let Err(e) = display.flush().await {
        log::warn!("display flush failed: {e:?}");
        return false;
    }
    true
}

/// The small bird's bitmap for a wing pose and heading.
fn small_bird(pose: usize, facing: Facing) -> &'static ImageRaw<'static, BinaryColor> {
    match facing {
        Facing::Left => &logo::SMALL_WINGS[pose],
        Facing::Right => &logo::SMALL_WINGS_RIGHT[pose],
    }
}

fn draw_splash<D: Panel>(
    display: &mut D,
    board: &str,
    pose: usize,
    bird: Point,
) -> Result<(), D::Error> {
    Image::new(&logo::LARGE_WINGS[pose], bird)
        .draw(display)
        .and_then(|()| text(display, "kolibri", Point::new(76, 20), SMALL))
        .and_then(|()| text(display, board, Point::new(76, 34), SMALL))
}

fn draw_status<D: Panel>(
    display: &mut D,
    labels: Labels,
    bird: &ImageRaw<'static, BinaryColor>,
    at: Point,
) -> Result<(), D::Error> {
    let mut value = TextBuf::<16>::new();
    let _ = match temperature::LATEST.try_get() {
        Some(celsius) => write!(value, "{celsius} \u{00b0}C"),
        None => write!(value, "--.- \u{00b0}C"),
    };

    let mut status = TextBuf::<24>::new();
    let _ = write!(
        status,
        "{}  up {} s",
        labels.source,
        Instant::now().as_secs()
    );

    // Layout, inside the 1 px border:
    //   y  3..33  the bird, x 86..126
    //   y  4..14  "kolibri"
    //   y 20..40  the reading, at most 8 chars, x 5..85
    //   y 46..56  source label and uptime
    // tools/gen-oled-preview.py mirrors these; rerun it after changing them.
    //
    // The bird goes first: its bitmap is opaque, so anything drawn before it
    // would be blanked by the unlit pixels around it.
    Image::new(bird, at)
        .draw(display)
        .and_then(|()| {
            Rectangle::new(Point::zero(), Size::new(128, 64))
                .into_styled(BORDER)
                .draw(display)
        })
        .and_then(|()| text(display, "kolibri", Point::new(5, 4), SMALL))
        .and_then(|()| text(display, value.as_str(), Point::new(5, 20), LARGE))
        .and_then(|()| text(display, status.as_str(), Point::new(5, 46), SMALL))
}

/// Shows the splash for [`SPLASH_PERIOD`], then flies the bird out to the left
/// and back in from the right onto the status screen, flapping as it goes.
async fn intro<D: Panel>(display: &mut D, labels: Labels) {
    if !show(display, |d| draw_splash(d, labels.board, 0, SPLASH_BIRD)).await {
        return;
    }
    Timer::after(SPLASH_PERIOD).await;

    let mut ticker = Ticker::every(FLIGHT_FRAME);
    let mut frame = 0;
    let gone = Point::new(-SPLASH_BIRD_WIDTH, SPLASH_BIRD.y);
    for p in glide(SPLASH_BIRD, gone, FLIGHT_STEP) {
        let (pose, bird) = wingbeat(frame, p, 1);
        if !show(display, |d| draw_splash(d, labels.board, pose, bird)).await {
            return;
        }
        frame += 1;
        ticker.next().await;
    }
    let offstage = Point::new(PANEL_WIDTH, STATUS_BIRD.y);
    if fly(
        display,
        labels,
        &mut ticker,
        &mut frame,
        offstage,
        STATUS_BIRD,
    )
    .await
    {
        rest(display, labels).await;
    }
}

/// Flies the small bird over the status screen from `from` to `to`, facing the
/// way it goes. Returns `false` if a frame could not be shown.
async fn fly<D: Panel>(
    display: &mut D,
    labels: Labels,
    ticker: &mut Ticker,
    frame: &mut usize,
    from: Point,
    to: Point,
) -> bool {
    let facing = Facing::toward(from, to).unwrap_or(Facing::Left);
    for p in glide(from, to, FLIGHT_STEP) {
        let (pose, at) = wingbeat(*frame, p, 2);
        if !show(display, |d| {
            draw_status(d, labels, small_bird(pose, facing), at)
        })
        .await
        {
            return false;
        }
        *frame += 1;
        ticker.next().await;
    }
    true
}

/// Draws the status screen with the bird perched at home, wings up, facing left.
async fn rest<D: Panel>(display: &mut D, labels: Labels) {
    show(display, |d| {
        draw_status(d, labels, small_bird(0, Facing::Left), STATUS_BIRD)
    })
    .await;
}

/// Flies the bird through a few random waypoints, some possibly off-screen,
/// and back home.
async fn wander<D: Panel>(display: &mut D, labels: Labels, rng: &mut Rng) {
    let hops = rng.between(HOPS.0, HOPS.1);
    log::info!("bird takes off for {hops} hops");

    let mut ticker = Ticker::every(FLIGHT_FRAME);
    let mut frame = 0;
    let mut at = STATUS_BIRD;
    for hop in 0..=hops {
        let to = if hop == hops {
            STATUS_BIRD
        } else {
            rng.waypoint()
        };
        if !fly(display, labels, &mut ticker, &mut frame, at, to).await {
            return;
        }
        at = to;
    }
    rest(display, labels).await;
}

/// A random rest between [`REST_MIN`] and [`REST_MAX`].
fn rest_period(rng: &mut Rng) -> Duration {
    let spread = (REST_MAX - REST_MIN).as_millis();
    let extra = rng.below(u32::try_from(spread).unwrap_or(u32::MAX).saturating_add(1));
    REST_MIN + Duration::from_millis(u64::from(extra))
}

/// Renders the latest [`temperature::LATEST`] reading once per [`PERIOD`], and
/// every so often sends the bird on an excursion. `seed` drives its choices.
pub async fn run<D: Panel>(mut display: D, labels: Labels, seed: u32) {
    if let Err(e) = display.init().await {
        log::error!("display init failed: {e:?} (wrong address? check SDA/SCL)");
        return;
    }
    log::info!("display ready");

    intro(&mut display, labels).await;

    let mut rng = Rng::new(seed);
    let mut ticker = Ticker::every(PERIOD);
    loop {
        let takeoff = Instant::now() + rest_period(&mut rng);
        while Instant::now() < takeoff {
            ticker.next().await;
            rest(&mut display, labels).await;
        }
        wander(&mut display, labels, &mut rng).await;
        // Skip the ticks missed while flying rather than redrawing in a burst.
        ticker.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_periods_stay_in_range() {
        let mut rng = Rng::new(7);
        for _ in 0..1000 {
            let rest = rest_period(&mut rng);
            assert!((REST_MIN..=REST_MAX).contains(&rest), "{rest:?}");
        }
    }
}
