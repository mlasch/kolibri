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

/// How often the panel is redrawn. Every redraw pushes the whole 1 KiB frame
/// over the bus, so this is the main cost of having a display attached.
pub const PERIOD: Duration = Duration::from_secs(1);

/// How long the boot splash stays up before the status screen replaces it.
pub const SPLASH_PERIOD: Duration = Duration::from_secs(2);

/// Framebuffer size: one bit per pixel over a 128x64 panel.
///
/// Passed explicitly because `GraphicsMode`'s default is sized for a 160x160
/// panel and would waste 2 KiB of RAM on this one.
pub const BUFFER_BYTES_128X64: usize = 128 * 64 / 8;

/// I2C address of a stock SH1106 module. Boards with the SA0 jumper bridged
/// answer on `0x3D` instead.
pub const DEFAULT_ADDRESS: u8 = 0x3C;

/// SH1106 control byte that marks the following bytes as pixel data rather than
/// commands.
pub const DATA_BYTE: u8 = 0x40;

/// A framebuffered monochrome panel that has to be told when to push a frame.
///
/// [`run`] is written against this rather than against `GraphicsMode` so a
/// board is free to bring a different controller or a different bus -- an
/// SPI SSD1309, say -- without touching the drawing code. Any `oled_async`
/// display already satisfies it through the blanket impl below.
// See the note on `temperature::Source`: single-threaded executor, no `Send`
// bounds needed.
#[allow(async_fn_in_trait)]
pub trait Panel: DrawTarget<Color = BinaryColor, Error: core::fmt::Debug> {
    /// How the panel reports a failed transfer.
    type BusError: core::fmt::Debug;

    /// Brings the controller up. This is where a wrong address or an unpowered
    /// panel shows up.
    async fn init(&mut self) -> Result<(), Self::BusError>;

    /// Blanks the framebuffer. Nothing reaches the panel until [`Panel::flush`].
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

/// The 128x64 SH1106 wired to an I2C bus, whatever HAL the bus came from.
pub type Sh1106I2c<I2C> = GraphicsMode<Sh1106_128_64, I2CInterface<I2C>, BUFFER_BYTES_128X64>;

/// Builds the SH1106 panel this project ships with, on any async I2C bus.
///
/// The SH1106 has 132 columns of RAM behind a 128 px panel; `Sh1106_128_64`
/// carries the resulting `COLUMN_OFFSET` of 2 so the image is not shifted.
#[must_use]
pub fn sh1106_i2c<I2C>(i2c: I2C, address: u8) -> Sh1106I2c<I2C>
where
    I2C: embedded_hal_async::i2c::I2c,
{
    Builder::new(Sh1106_128_64 {})
        .connect(I2CInterface::new(i2c, address, DATA_BYTE))
        .into()
}

/// The two board-specific strings the screen puts up.
///
/// A struct rather than two `&str` arguments so they cannot be swapped by
/// accident.
#[derive(Clone, Copy, Debug)]
pub struct Labels {
    /// Short board or chip name for the boot splash, e.g. `"esp32-c3"`.
    pub board: &'static str,
    /// What the temperature describes, e.g. `"chip"`. Conventionally
    /// `<S as temperature::Source>::LABEL`.
    pub source: &'static str,
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
async fn splash<D: Panel>(display: &mut D, board: &str) {
    let text = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);

    display.clear_buffer();

    let drawn = Image::new(&logo::LARGE, Point::new(4, 8))
        .draw(display)
        .and_then(|()| {
            Text::with_baseline("kolibri", Point::new(76, 20), text, Baseline::Top)
                .draw(display)
                .map(|_| ())
        })
        .and_then(|()| {
            Text::with_baseline(board, Point::new(76, 34), text, Baseline::Top)
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

    // A `Timer`, not a blocking delay: the other tasks keep running for the two
    // seconds the splash is up.
    Timer::after(SPLASH_PERIOD).await;
}

/// Renders the latest [`temperature::LATEST`] reading to the panel once per
/// [`PERIOD`].
///
/// Redrawing is a two-stage affair: `embedded-graphics` calls mutate an
/// in-memory framebuffer, and nothing reaches the panel until
/// [`Panel::flush`]. Drawing into RAM is infallible in practice, so only the
/// bus transfers can really fail.
pub async fn run<D: Panel>(mut display: D, labels: Labels) {
    let text = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    // The reading gets the big font, and an ISO 8859-1 one rather than ASCII so
    // that the degree sign is a real glyph instead of the replacement box. That
    // costs 4.8 KiB of flash against 2.4 KiB for the ASCII sheet; the rest of
    // the screen stays on the 720-byte 6x10 ASCII font.
    let reading = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let border = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Worth failing loudly rather than drawing into the void for ever.
    if let Err(e) = display.init().await {
        log::error!("display init failed: {e:?} (wrong address? check SDA/SCL)");
        return;
    }
    log::info!("display ready");

    splash(&mut display, labels.board).await;

    let mut ticker = Ticker::every(PERIOD);

    loop {
        ticker.next().await;

        display.clear_buffer();

        // `try_get` reads the latest reading without consuming it, so every
        // frame sees it until a newer one arrives. `None` only until the first
        // sample lands, which happens during the splash -- so in practice the
        // placeholder is only ever seen if the very first read fails.
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

        // Layout on the 128x64 panel, everything inside the 1 px border:
        //   y  3..33  the bird, x 86..126
        //   y  4..14  "kolibri"
        //   y 20..40  the reading -- at most 8 chars at 10 px, so x 5..85,
        //             which stays clear of the bird even at "-12.3 C"
        //   y 46..56  source label and uptime
        //
        // tools/gen-oled-preview.py mirrors these coordinates to regenerate the
        // README image; change one and rerun it.
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
