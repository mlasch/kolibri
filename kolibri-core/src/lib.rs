//! The MCU-independent half of the kolibri firmware.
//!
//! Everything in here is written against traits rather than against a HAL:
//! [`embedded_hal::digital::StatefulOutputPin`] for the LED,
//! [`embedded_hal_async::i2c::I2c`] for the display bus,
//! [`embedded_graphics::draw_target::DrawTarget`] for the panel, and
//! [`temperature::Source`] for the sensor. A board crate under `boards/`
//! supplies the concrete types, spawns the tasks, and owns everything that is
//! genuinely chip-specific -- the HAL, the entry point, the linker script and
//! the flashing runner.
//!
//! # What a board crate has to provide
//!
//! Embassy tasks cannot be generic: `#[embassy_executor::task]` needs a
//! concrete type to size the task's static storage. So the loops live here as
//! ordinary generic `async fn`s, and each board wraps them in one-line tasks
//! over its own types:
//!
//! ```ignore
//! #[embassy_executor::task]
//! async fn blink(led: Output<'static>) {
//!     kolibri_core::blink::run(led).await;
//! }
//! ```
//!
//! The four loops are [`blink::run`], [`heartbeat::run`],
//! [`temperature::run`] and [`display::run`].

#![no_std]

pub mod blink;
pub mod display;
pub mod heartbeat;
pub mod logo;
pub mod temperature;
pub mod text;
