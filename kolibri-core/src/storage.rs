//! A small record kept in NOR flash across resets, power cuts and reflashes.
//!
//! One record, rewritten whole, in two alternating erase-block slots. A save
//! erases the older slot, writes the payload, and writes the header last, so an
//! interrupted save leaves the previous record intact. [`Store::load`] takes the
//! valid slot with the higher sequence number.
//!
//! Each slot starts with a [`HEADER_LEN`]-byte little-endian header:
//!
//! ```text
//! 0..4    magic, b"KLBS"
//! 4       format version
//! 5       reserved, zero
//! 6..8    payload length in bytes
//! 8..12   sequence number
//! 12..16  CRC-32 of bytes 0..12 followed by the whole padded payload block
//! ```
//!
//! The payload block is always `N` bytes so every access stays aligned. Some
//! drivers (esp-storage) copy unaligned buffers through an erase-block-sized
//! stack buffer, so a load can cost 4 KiB of stack.

use embedded_storage::nor_flash::NorFlash;

/// Bytes of bookkeeping in front of each stored payload.
pub const HEADER_LEN: usize = 16;

const MAGIC: [u8; 4] = *b"KLBS";

/// An older format reads back as "no record".
const FORMAT: u8 = 1;

const SLOTS: usize = 2;

/// Why a load or save could not be completed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error<E> {
    /// The underlying flash refused a read, write or erase.
    Flash(E),
    /// The region is smaller than two erase blocks.
    TooSmall,
    /// A record does not fit an erase block, or `N` is not write-granular.
    Layout,
    /// The payload handed to [`Store::save`] is longer than `N`.
    TooLarge,
}

/// A slot header that looks like ours; the CRC is checked after the payload is read.
#[derive(Clone, Copy)]
struct Header {
    raw: [u8; HEADER_LEN],
    seq: u32,
    len: usize,
    crc: u32,
}

/// A single `N`-byte record kept in the first two erase blocks of `flash`.
pub struct Store<F, const N: usize> {
    flash: F,
}

impl<F: NorFlash, const N: usize> Store<F, N> {
    /// Takes ownership of a flash region and checks it can hold the record.
    pub fn new(flash: F) -> Result<Self, Error<F::Error>> {
        let granular =
            |len: usize| len.is_multiple_of(F::READ_SIZE) && len.is_multiple_of(F::WRITE_SIZE);
        if !granular(HEADER_LEN) || !granular(N) || HEADER_LEN + N > F::ERASE_SIZE {
            return Err(Error::Layout);
        }
        if flash.capacity() < SLOTS * F::ERASE_SIZE {
            return Err(Error::TooSmall);
        }
        Ok(Self { flash })
    }

    /// Reads the current record into `into` and returns its length, or
    /// `Ok(None)` if there is no valid record.
    pub fn load(&mut self, into: &mut [u8; N]) -> Result<Option<usize>, Error<F::Error>> {
        // Newest first: a corrupt slot read after the good one would clobber `into`.
        let mut order = [0, 1];
        let headers = [self.header(0)?, self.header(1)?];
        if supersedes(headers[1], headers[0]) {
            order.swap(0, 1);
        }

        for slot in order {
            let Some(header) = headers[slot] else {
                continue;
            };
            self.flash
                .read(Self::payload_offset(slot), into)
                .map_err(Error::Flash)?;
            if crc32(&header.raw[..12], into) == header.crc {
                return Ok(Some(header.len));
            }
        }
        Ok(None)
    }

    /// Replaces the current record with `payload`, writing to the slot that
    /// does not hold it.
    pub fn save(&mut self, payload: &[u8]) -> Result<(), Error<F::Error>> {
        if payload.len() > N {
            return Err(Error::TooLarge);
        }

        let headers = [self.header(0)?, self.header(1)?];
        // With nothing stored yet, start at slot 0; otherwise use the other slot.
        let current = match (headers[0], headers[1]) {
            (None, None) => None,
            (a, b) => Some(usize::from(supersedes(b, a))),
        };
        let slot = current.map_or(0, |slot| 1 - slot);
        let seq = current
            .and_then(|slot| headers[slot])
            .map_or(0, |header| header.seq.wrapping_add(1));

        let mut block = [0u8; N];
        block[..payload.len()].copy_from_slice(payload);

        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4] = FORMAT;
        header[6..8].copy_from_slice(&(payload.len() as u16).to_le_bytes());
        header[8..12].copy_from_slice(&seq.to_le_bytes());
        let crc = crc32(&header[..12], &block);
        header[12..].copy_from_slice(&crc.to_le_bytes());

        let base = Self::base(slot);
        self.flash
            .erase(base, base + Self::slot_size())
            .map_err(Error::Flash)?;
        self.flash
            .write(Self::payload_offset(slot), &block)
            .map_err(Error::Flash)?;
        // The header is the commit point: only now does the slot count.
        self.flash.write(base, &header).map_err(Error::Flash)?;
        Ok(())
    }

    fn slot_size() -> u32 {
        F::ERASE_SIZE as u32
    }

    fn base(slot: usize) -> u32 {
        slot as u32 * Self::slot_size()
    }

    fn payload_offset(slot: usize) -> u32 {
        Self::base(slot) + HEADER_LEN as u32
    }

    fn header(&mut self, slot: usize) -> Result<Option<Header>, Error<F::Error>> {
        let mut raw = [0u8; HEADER_LEN];
        self.flash
            .read(Self::base(slot), &mut raw)
            .map_err(Error::Flash)?;

        if raw[..4] != MAGIC || raw[4] != FORMAT {
            return Ok(None);
        }
        let len = usize::from(u16::from_le_bytes([raw[6], raw[7]]));
        if len > N {
            return Ok(None);
        }
        Ok(Some(Header {
            raw,
            seq: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
            len,
            crc: u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]),
        }))
    }
}

/// Whether `a` is newer than `b`. A missing slot loses to a present one.
fn supersedes(a: Option<Header>, b: Option<Header>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            // Sequence numbers wrap: `a` is newer while less than half the range ahead.
            let ahead = a.seq.wrapping_sub(b.seq);
            ahead != 0 && ahead < 0x8000_0000
        }
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// CRC-32 (zip/Ethernet) over `head` followed by `body`. Bitwise rather than
/// table-driven: the 1 KiB table would cost more flash than the loop costs time.
fn crc32(head: &[u8], body: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in head.iter().chain(body) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_storage::nor_flash::{ErrorType, NorFlashError, NorFlashErrorKind, ReadNorFlash};

    const SECTOR: usize = 256;
    const CAPACITY: usize = SECTOR * 2;
    const PAYLOAD: usize = 32;

    /// Mock NOR flash: word-granular, erase sets ones, writes only clear bits.
    struct Ram {
        bytes: [u8; CAPACITY],
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RamError;

    impl NorFlashError for RamError {
        fn kind(&self) -> NorFlashErrorKind {
            NorFlashErrorKind::Other
        }
    }

    impl ErrorType for Ram {
        type Error = RamError;
    }

    impl Ram {
        fn new() -> Self {
            Self {
                bytes: [0xFF; CAPACITY],
            }
        }
    }

    impl ReadNorFlash for Ram {
        const READ_SIZE: usize = 4;

        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), RamError> {
            let at = offset as usize;
            if !at.is_multiple_of(4)
                || !bytes.len().is_multiple_of(4)
                || at + bytes.len() > CAPACITY
            {
                return Err(RamError);
            }
            bytes.copy_from_slice(&self.bytes[at..at + bytes.len()]);
            Ok(())
        }

        fn capacity(&self) -> usize {
            CAPACITY
        }
    }

    impl NorFlash for Ram {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = SECTOR;

        fn erase(&mut self, from: u32, to: u32) -> Result<(), RamError> {
            let (from, to) = (from as usize, to as usize);
            if !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) || to > CAPACITY {
                return Err(RamError);
            }
            self.bytes[from..to].fill(0xFF);
            Ok(())
        }

        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), RamError> {
            let at = offset as usize;
            if !at.is_multiple_of(4)
                || !bytes.len().is_multiple_of(4)
                || at + bytes.len() > CAPACITY
            {
                return Err(RamError);
            }
            for (cell, byte) in self.bytes[at..].iter_mut().zip(bytes) {
                *cell &= byte;
            }
            Ok(())
        }
    }

    fn store() -> Store<Ram, PAYLOAD> {
        Store::new(Ram::new()).expect("the mock flash fits a record")
    }

    #[test]
    fn blank_flash_holds_no_record() {
        let mut store = store();
        let mut buf = [0u8; PAYLOAD];
        assert_eq!(store.load(&mut buf), Ok(None));
    }

    #[test]
    fn a_saved_record_reads_back() {
        let mut store = store();
        store.save(b"hello").unwrap();

        let mut buf = [0u8; PAYLOAD];
        assert_eq!(store.load(&mut buf), Ok(Some(5)));
        assert_eq!(&buf[..5], b"hello");
        // Everything past the payload is padding, not leftovers.
        assert!(buf[5..].iter().all(|&b| b == 0));
    }

    #[test]
    fn saves_alternate_slots_and_the_newest_wins() {
        let mut store = store();
        for round in 0..5u8 {
            store.save(&[round; 4]).unwrap();
            let mut buf = [0u8; PAYLOAD];
            assert_eq!(store.load(&mut buf), Ok(Some(4)));
            assert_eq!(&buf[..4], &[round; 4]);
        }
        // Consecutive saves must land in different slots.
        assert_ne!(
            store.flash.bytes[..HEADER_LEN],
            store.flash.bytes[SECTOR..SECTOR + HEADER_LEN]
        );
    }

    #[test]
    fn a_corrupt_newest_slot_falls_back_to_the_previous_one() {
        let mut store = store();
        store.save(b"old").unwrap();
        store.save(b"new").unwrap();

        // Corrupt the newer payload but not its header; only the CRC catches this.
        store.flash.bytes[SECTOR + HEADER_LEN] ^= 0xFF;

        let mut buf = [0u8; PAYLOAD];
        assert_eq!(store.load(&mut buf), Ok(Some(3)));
        assert_eq!(&buf[..3], b"old");
    }

    #[test]
    fn a_torn_save_leaves_the_previous_record() {
        let mut store = store();
        store.save(b"old").unwrap();
        store.save(b"keep me").unwrap();

        // Power lost right after a third save erased slot 0.
        store.flash.erase(0, SECTOR as u32).unwrap();

        let mut buf = [0u8; PAYLOAD];
        assert_eq!(store.load(&mut buf), Ok(Some(7)));
        assert_eq!(&buf[..7], b"keep me");
    }

    #[test]
    fn a_foreign_partition_is_not_mistaken_for_a_record() {
        let mut store = store();
        store.flash.bytes.fill(0x5A);
        let mut buf = [0u8; PAYLOAD];
        assert_eq!(store.load(&mut buf), Ok(None));
    }

    #[test]
    fn an_oversized_payload_is_refused() {
        let mut store = store();
        assert_eq!(store.save(&[0u8; PAYLOAD + 1]), Err(Error::TooLarge));
    }

    #[test]
    fn a_region_too_small_for_two_slots_is_refused() {
        struct Tiny(Ram);
        impl ErrorType for Tiny {
            type Error = RamError;
        }
        impl ReadNorFlash for Tiny {
            const READ_SIZE: usize = Ram::READ_SIZE;
            fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), RamError> {
                self.0.read(offset, bytes)
            }
            fn capacity(&self) -> usize {
                SECTOR
            }
        }
        impl NorFlash for Tiny {
            const WRITE_SIZE: usize = Ram::WRITE_SIZE;
            const ERASE_SIZE: usize = Ram::ERASE_SIZE;
            fn erase(&mut self, from: u32, to: u32) -> Result<(), RamError> {
                self.0.erase(from, to)
            }
            fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), RamError> {
                self.0.write(offset, bytes)
            }
        }
        assert!(matches!(
            Store::<Tiny, PAYLOAD>::new(Tiny(Ram::new())),
            Err(Error::TooSmall)
        ));
    }

    #[test]
    fn a_payload_that_cannot_be_written_whole_is_refused() {
        // Not a multiple of the four-byte write granularity.
        assert!(matches!(
            Store::<Ram, 30>::new(Ram::new()),
            Err(Error::Layout)
        ));
        // Larger than an erase block, header included.
        assert!(matches!(
            Store::<Ram, SECTOR>::new(Ram::new()),
            Err(Error::Layout)
        ));
    }

    /// `tools/mk-settings.py` must write a record this store accepts.
    #[test]
    fn the_provisioning_script_writes_a_record_this_store_accepts() {
        use std::{format, string::ToString};

        let out =
            std::env::temp_dir().join(format!("kolibri-provisioned-{}.bin", std::process::id()));
        let status = std::process::Command::new("python3")
            .args(["../tools/mk-settings.py", "--hex", "de ad be ef"])
            .args(["--storage", "src/storage.rs"])
            // Non-zero, so a misplaced header field shows up.
            .args(["--sequence", "7"])
            .args(["--erase-size", &SECTOR.to_string()])
            .args(["--capacity", &PAYLOAD.to_string()])
            .arg("--out")
            .arg(&out)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("python3 is needed to check the provisioning script");
        assert!(status.success(), "tools/mk-settings.py failed");

        let image = std::fs::read(&out).expect("the script wrote an image");
        std::fs::remove_file(&out).ok();
        assert_eq!(image.len(), CAPACITY);

        let mut flash = Ram::new();
        flash.bytes.copy_from_slice(&image);

        let mut store = Store::<Ram, PAYLOAD>::new(flash).expect("the image fits the mock flash");
        let mut buf = [0u8; PAYLOAD];
        assert_eq!(store.load(&mut buf), Ok(Some(4)));
        assert_eq!(&buf[..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn crc32_matches_the_reference_check_value() {
        // The standard CRC-32 check value for b"123456789".
        assert_eq!(crc32(b"12345", b"6789"), 0xCBF4_3926);
    }
}
