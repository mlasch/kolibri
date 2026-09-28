//! The status screen, and the panel abstraction that keeps it portable.

use core::fmt::Write;

use display_interface::{AsyncWriteOnlyDataCommand, DisplayError};
use display_interface_i2c::I2CInterface;
use embassy_time::{Duration, Instant, Ticker, Timer};
use embedded_graphics::{
    draw_target::DrawTarget,
    image::Image,
    mono_font::{MonoTextStyle, ascii::FONT_6X10, iso_8859_1::FONT_10X20},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
use oled_async::{Builder, display::DisplayVariant, displays::sh1106::Sh1106_128_64, prelude::*};

use crate::{logo, temperature, text::TextBuf};

/// How often the panel is redrawn.
pub const PERIOD: Duration = Duration::from_secs(1);

/// How long the boot splash stays up.
pub const SPLASH_PERIOD: Duration = Duration::from_secs(2);

/// How far the bird moves per animation frame, in pixels.
pub const FLIGHT_STEP: i32 = 4;

/// Time between animation frames; a full-frame flush takes ~26 ms at 400 kHz.
pub const FLIGHT_FRAME: Duration = Duration::from_millis(40);

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

/// One wingbeat: (index into `logo::*_WINGS`, body offset in large-bird pixels).
/// The body rises on the downstroke; the small bird bobs half as far.
const FLAP: [(usize, i32); 4] = [(0, 0), (1, -1), (2, -2), (1, -1)];

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

/// Positions from `from` (exclusive) to `to` (inclusive), `step` apart, the last
/// one clamped to land exactly on `to`.
fn flight(from: i32, to: i32, step: i32) -> impl Iterator<Item = i32> {
    let mut x = from;
    core::iter::from_fn(move || {
        if x == to {
            return None;
        }
        x = if (to - x).abs() <= step {
            to
        } else {
            x + (to - x).signum() * step
        };
        Some(x)
    })
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

/// The wing pose and bird position for animation frame `n`.
fn wingbeat(n: usize, at: Point, bob_scale: i32) -> (usize, Point) {
    let (pose, bob) = FLAP[n % FLAP.len()];
    (pose, at + Point::new(0, bob / bob_scale))
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
    pose: usize,
    bird: Point,
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
    Rectangle::new(Point::zero(), Size::new(128, 64))
        .into_styled(BORDER)
        .draw(display)
        .and_then(|()| Image::new(&logo::SMALL_WINGS[pose], bird).draw(display))
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
    for x in flight(SPLASH_BIRD.x, -SPLASH_BIRD_WIDTH, FLIGHT_STEP) {
        let (pose, bird) = wingbeat(frame, Point::new(x, SPLASH_BIRD.y), 1);
        if !show(display, |d| draw_splash(d, labels.board, pose, bird)).await {
            return;
        }
        frame += 1;
        ticker.next().await;
    }
    for x in flight(PANEL_WIDTH, STATUS_BIRD.x, FLIGHT_STEP) {
        let (pose, bird) = wingbeat(frame, Point::new(x, STATUS_BIRD.y), 2);
        if !show(display, |d| draw_status(d, labels, pose, bird)).await {
            return;
        }
        frame += 1;
        ticker.next().await;
    }
    // Settle into the resting pose rather than holding a mid-flap frame.
    show(display, |d| draw_status(d, labels, 0, STATUS_BIRD)).await;
}

/// Renders the latest [`temperature::LATEST`] reading once per [`PERIOD`].
pub async fn run<D: Panel>(mut display: D, labels: Labels) {
    if let Err(e) = display.init().await {
        log::error!("display init failed: {e:?} (wrong address? check SDA/SCL)");
        return;
    }
    log::info!("display ready");

    intro(&mut display, labels).await;

    let mut ticker = Ticker::every(PERIOD);
    loop {
        ticker.next().await;
        show(&mut display, |d| draw_status(d, labels, 0, STATUS_BIRD)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn flight_lands_exactly_on_target() {
        let path: Vec<i32> = flight(128, 86, 6).collect();
        assert_eq!(path, [122, 116, 110, 104, 98, 92, 86]);
    }

    #[test]
    fn flight_clamps_the_last_step() {
        let path: Vec<i32> = flight(4, -64, 6).collect();
        assert_eq!(path.last(), Some(&-64));
        let mut prev = 4;
        for x in path {
            assert!(x < prev && prev - x <= 6);
            prev = x;
        }
    }

    #[test]
    fn a_wingbeat_starts_at_rest_and_cycles() {
        let at = Point::new(10, 20);
        assert_eq!(wingbeat(0, at, 1), (0, at));
        assert_eq!(wingbeat(2, at, 1), (2, Point::new(10, 18)));
        assert_eq!(wingbeat(2, at, 2), (2, Point::new(10, 19)));
        assert_eq!(wingbeat(FLAP.len(), at, 1), wingbeat(0, at, 1));
        assert!(FLAP.iter().all(|&(pose, _)| pose < logo::LARGE_WINGS.len()));
    }

    #[test]
    fn flight_to_where_it_already_is_is_empty() {
        assert_eq!(flight(86, 86, 6).count(), 0);
    }
}
