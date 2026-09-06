//! Formatting text without an allocator.

use core::fmt::Write;

/// A fixed-capacity sink for `write!`, so text can be formatted on the stack.
///
/// `heapless::String` does the same job if you would rather take the extra
/// dependency; this exists to keep the tree at four display crates instead of
/// five. A write that would overflow `N` is rejected whole rather than
/// panicking or leaving a truncated fragment behind.
pub struct TextBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> TextBuf<N> {
    /// Creates an empty buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    /// The text written so far.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Only whole `&str` chunks are ever appended, so the prefix is valid
        // UTF-8 by construction.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> Default for TextBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for TextBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = N - self.len;
        if s.len() > room {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}
