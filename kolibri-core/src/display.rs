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

/// Framebuffer size of a 128x64 1-bpp panel (`GraphicsMode`'s default is larger).
pub const BUFFER_BYTES_128X64: usize = 128 * 64 / 8;

/// I2C address of a stock SH1106 module (`0x3D` with SA0 bridged).
pub const DEFAULT_ADDRESS: u8 = 0x3C;

/// SH1106 control byte marking the following bytes as pixel data.
pub const DATA_BYTE: u8 = 0x40;

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

/// Draws the boot splash and leaves it up for [`SPLASH_PERIOD`].
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

    Timer::after(SPLASH_PERIOD).await;
}

/// Renders the latest [`temperature::LATEST`] reading once per [`PERIOD`].
pub async fn run<D: Panel>(mut display: D, labels: Labels) {
    let text = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    // ISO 8859-1 so the degree sign is a real glyph.
    let reading = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let border = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

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

        if let Err(e) = display.flush().await {
            log::warn!("display flush failed: {e:?}");
        }
    }
}
