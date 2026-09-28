//! The MCU-independent half of the kolibri firmware.
//!
//! Everything here is written against traits, not a HAL. Embassy tasks cannot
//! be generic, so the loops are generic `async fn`s that each board wraps in
//! one-line tasks over its own types:
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

// Host unit tests only; the library itself never uses `std`.
#[cfg(test)]
extern crate std;

pub mod blink;
pub mod display;
pub mod flight;
pub mod heartbeat;
pub mod logo;
pub mod storage;
pub mod temperature;
pub mod text;
