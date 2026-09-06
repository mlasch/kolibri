//! A small record that survives a power cycle, kept in NOR flash.
//!
//! Firmware needs somewhere to remember a handful of bytes -- Wi-Fi
//! credentials, a calibration offset, which screen was last shown -- across
//! resets and reflashes. This module is that somewhere, written against
//! [`embedded_storage::nor_flash::NorFlash`] so it works on any chip whose HAL
//! can hand over an erasable flash region.
//!
//! # What it is not
//!
//! It is not a filesystem and not a key-value store. There is exactly one
//! record, it is a byte string, and every save rewrites all of it. If you need
//! independently updatable keys, wear levelling across a large partition, or
//! records bigger than an erase block, reach for `sequential-storage` instead;
//! this is deliberately a few hundred bytes of flash rather than a few
//! thousand.
//!
//! # How a half-finished write is survived
//!
//! NOR flash cannot be updated in place: a sector is erased to all-ones and
//! then bits are cleared. Erasing the only copy of the record and losing power
//! mid-write would therefore lose the data. So the store keeps *two* slots, one
//! erase block each, and alternates between them:
//!
//! ```text
//! slot 0 [ header | payload ]  <- seq 6, current
//! slot 1 [ header | payload ]  <- seq 5, previous; next save lands here
//! ```
//!
//! A save erases the older slot, writes the payload, and writes the header
//! last. The header is the commit point: until its magic, sequence number and
//! CRC are on flash the slot does not count, so an interrupted save leaves the
//! previous record exactly as it was. [`Store::load`] takes the valid slot with
//! the higher sequence number.
//!
//! # Layout
//!
//! Each slot starts with a [`HEADER_LEN`]-byte header, all little-endian:
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
//! The payload block that follows is always `N` bytes -- the capacity, not the
//! length -- because flash writes have a granularity (four bytes on an ESP32)
//! and padding to a fixed size keeps every read, write and erase this module
//! issues trivially aligned.
//!
//! # Stack
//!
//! Some drivers want a word-aligned *buffer* as well as a word-aligned offset,
//! and copy through a scratch buffer as large as an erase block when they do
//! not get one -- esp-storage does exactly that, so a load costs a transient
//! 4 KiB of stack there. Call this from `main`, or from a task whose stack has
//! room for it.

use embedded_storage::nor_flash::NorFlash;

/// Bytes of bookkeeping in front of each stored payload.
///
/// A record therefore occupies `HEADER_LEN + N` bytes of its slot, and that
/// has to fit in one erase block.
pub const HEADER_LEN: usize = 16;

/// Identifies a slot as ours rather than as whatever the partition held before.
const MAGIC: [u8; 4] = *b"KLBS";

/// Bumped only if the layout above changes incompatibly. An older format reads
/// back as "no record", which is the safe answer: the firmware falls back to
/// its defaults and the next save rewrites the slot.
const FORMAT: u8 = 1;

/// Two is the minimum that keeps one intact copy at all times, and the maximum
/// that is worth the flash: this record is rewritten rarely, so wear is not the
/// constraint that would justify more.
const SLOTS: usize = 2;

/// Why a load or save could not be completed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error<E> {
    /// The underlying flash refused a read, write or erase.
    Flash(E),
    /// The region is smaller than the two erase blocks the store needs.
    TooSmall,
    /// `N` does not suit this flash: a record does not fit in an erase block,
    /// or the payload capacity is not a multiple of the read/write granularity.
    Layout,
    /// The payload handed to [`Store::save`] is longer than `N`.
    TooLarge,
}

/// The parsed header of one slot. Present only means "plausibly ours"; the CRC
/// is checked later, once the payload has been read.
#[derive(Clone, Copy)]
struct Header {
    raw: [u8; HEADER_LEN],
    seq: u32,
    len: usize,
    crc: u32,
}

/// A single `N`-byte record kept in the first two erase blocks of `flash`.
///
/// `N` is the payload *capacity* and must be a multiple of the flash's write
/// granularity; [`Store::new`] rejects anything else rather than failing
/// obscurely on the first save.
pub struct Store<F, const N: usize> {
    flash: F,
}

impl<F: NorFlash, const N: usize> Store<F, N> {
    /// Takes ownership of a flash region and checks it can hold the record.
    ///
    /// The region is normally a partition rather than the whole chip: on an
    /// ESP32 that is an entry from the partition table, so the store can never
    /// reach the application image no matter what it is asked to write.
    ///
    /// # Errors
    ///
    /// [`Error::Layout`] if `N` does not suit this flash, [`Error::TooSmall`]
    /// if the region cannot hold two slots.
    pub fn new(flash: F) -> Result<Self, Error<F::Error>> {
        // The store only ever reads or writes a whole header or a whole
        // payload block, so those two lengths are all that has to divide.
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

    /// Reads the current record into `into`.
    ///
    /// Returns the payload length, which is however many leading bytes of
    /// `into` were actually stored; the rest is the zero padding. `Ok(None)`
    /// means there is nothing to read -- a blank partition, a record from an
    /// older format, or one whose CRC does not check out.
    ///
    /// # Errors
    ///
    /// [`Error::Flash`] if the region could not be read.
    pub fn load(&mut self, into: &mut [u8; N]) -> Result<Option<usize>, Error<F::Error>> {
        // Both headers first, then the payloads newest-first: reading a payload
        // overwrites `into`, so a slot that turns out to be corrupt must not be
        // read after the good one.
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

    /// Replaces the current record with `payload`.
    ///
    /// Writes to whichever slot is not current, so the record being replaced
    /// stays readable until this returns.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] if `payload` exceeds `N`, [`Error::Flash`] if the
    /// region could not be erased or written.
    pub fn save(&mut self, payload: &[u8]) -> Result<(), Error<F::Error>> {
        if payload.len() > N {
            return Err(Error::TooLarge);
        }

        let headers = [self.header(0)?, self.header(1)?];
        // With nothing stored yet there is no slot to preserve, so start at the
        // front; otherwise take the one the current record is not in.
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
        // Last, and only now does the slot count as written.
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

/// Whether `a` is the newer of two slots. A slot that is not there at all loses
/// to one that is.
fn supersedes(a: Option<Header>, b: Option<Header>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            // Sequence numbers wrap, so compare the distance rather than the
            // values: `a` is newer while it is less than half the range ahead.
            let ahead = a.seq.wrapping_sub(b.seq);
            ahead != 0 && ahead < 0x8000_0000
        }
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// CRC-32 as used by zip and Ethernet, over `head` followed by `body`.
///
/// Bit-at-a-time rather than table-driven: a few hundred bytes are checksummed
/// once per boot and once per save, so the 1 KiB lookup table would cost more
/// flash than the loop ever costs time.
fn crc32(head: &[u8], body: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in head.iter().chain(body) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            // Branchless: mask is all-ones exactly when the low bit is set.
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

    /// Enough of a NOR flash to be wrong in the same ways a real one is: reads
    /// and writes are word-granular, erase sets ones, and a write can only
    /// clear bits. Writing over live data therefore corrupts it here too,
    /// which is what makes the slot alternation worth testing.
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
        // Five saves into two slots only work if each one lands in the slot the
        // other is not using.
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

        // Flip a payload byte in the newer slot, leaving its header -- and so
        // its sequence number -- intact. Only the CRC can catch this.
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

        // A third save would erase slot 0 and write it. Stop after the erase,
        // which is what losing power mid-save looks like on the flash.
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

    /// The provisioning script writes this same envelope from the host, and
    /// nothing else checks that the two agree. Here it builds an image sized
    /// for the mock flash above, and the store has to read the payload back --
    /// so a change to the format on either side fails here rather than on a
    /// board that quietly ignores the record it was provisioned with.
    #[test]
    fn the_provisioning_script_writes_a_record_this_store_accepts() {
        // `no_std` means no prelude for these, even with std linked for tests.
        use std::{format, string::ToString};

        let out =
            std::env::temp_dir().join(format!("kolibri-provisioned-{}.bin", std::process::id()));
        let status = std::process::Command::new("python3")
            .args(["../tools/mk-settings.py", "--hex", "de ad be ef"])
            .args(["--storage", "src/storage.rs"])
            // Non-zero, so that a header field written at the wrong offset
            // shows up as a difference rather than as another run of zeros.
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
