#!/usr/bin/env python3
"""Build a settings image, so a board can be flashed already provisioned.

Writes the `kolibri_core::storage` record envelope from the host. The format
constants are parsed out of kolibri-core/src/storage.rs, so a format change
that is not mirrored here fails loudly.

    python3 tools/mk-settings.py --boot-count 41
    espflash write-bin 0x9000 settings.bin

Decode what a board carries:

    espflash read-flash 0x9000 0x2000 dump.bin
    python3 tools/mk-settings.py --inspect dump.bin
"""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
import zlib
from dataclasses import dataclass
from pathlib import Path

STORAGE_RS = Path("kolibri-core/src/storage.rs")

BOARD_MAIN = "boards/{board}/src/main.rs"
DEFAULT_BOARD = "xiao-esp32c3"

# `nvs` partition in espflash's default partition table.
DEFAULT_OFFSET = 0x9000

# ESP32 flash sector size, and so the size of one slot.
DEFAULT_ERASE_SIZE = 4096

ERASED = b"\xff"

# Header field offsets; must match storage.rs (HEADER_LEN is checked).
MAGIC_AT = slice(0, 4)
FORMAT_AT = 4
LENGTH_AT = slice(6, 8)
SEQUENCE_AT = slice(8, 12)
CRC_AT = slice(12, 16)
CRC_COVERS = slice(0, 12)
EXPECTED_HEADER_LEN = 16


@dataclass(frozen=True)
class Layout:
    """The record format, as kolibri-core defines it."""

    magic: bytes
    format: int
    header_len: int
    slots: int
    capacity: int
    erase_size: int

    @property
    def size(self) -> int:
        """Bytes the image covers: every slot, used or not."""
        return self.slots * self.erase_size


@dataclass
class Record:
    """What one slot holds, once its header has been believed."""

    slot: int
    sequence: int
    payload: bytes
    intact: bool


def rust_const(source: str, pattern: str, what: str, path: Path) -> str:
    """Pull one constant out of a Rust file, or explain why the build is stale."""
    match = re.search(pattern, source)
    if match is None:
        sys.exit(
            f"{path}: no {what} found. The record format has moved; "
            f"update tools/mk-settings.py to match it."
        )
    return match.group(1)


def read_layout(
    storage: Path, board: str, capacity: int | None, erase_size: int
) -> Layout:
    """Derive the format from the firmware source instead of restating it."""
    if not storage.is_file():
        sys.exit(f"{storage}: not found -- run this from the repository root")
    source = storage.read_text()

    def const(pattern: str, what: str) -> str:
        return rust_const(source, pattern, what, storage)

    layout = Layout(
        magic=const(r'const MAGIC: \[u8; 4\] = \*b"(....)"', "MAGIC").encode(),
        format=int(const(r"const FORMAT: u8 = (\d+)", "FORMAT")),
        header_len=int(const(r"pub const HEADER_LEN: usize = (\d+)", "HEADER_LEN")),
        slots=int(const(r"const SLOTS: usize = (\d+)", "SLOTS")),
        capacity=capacity if capacity is not None else board_capacity(board),
        erase_size=erase_size,
    )

    if layout.header_len != EXPECTED_HEADER_LEN:
        sys.exit(
            f"{storage}: HEADER_LEN is {layout.header_len}, this script writes "
            f"{EXPECTED_HEADER_LEN}. The field offsets have to be updated together."
        )
    if layout.header_len + layout.capacity > layout.erase_size:
        sys.exit(
            f"a {layout.capacity}-byte record does not fit an "
            f"{layout.erase_size}-byte erase block"
        )
    if layout.capacity % 4:
        sys.exit(f"capacity {layout.capacity} is not a multiple of 4")
    return layout


def board_capacity(board: str) -> int:
    """How much of a record that board's firmware is willing to use."""
    path = Path(BOARD_MAIN.format(board=board))
    if not path.is_file():
        sys.exit(f"{path}: not found -- pass --capacity, or --board for another board")
    return int(
        rust_const(
            path.read_text(),
            r"const SETTINGS_CAPACITY: usize = (\d+)",
            "SETTINGS_CAPACITY",
            path,
        )
    )


def slot(payload: bytes, layout: Layout, sequence: int) -> bytes:
    """One slot: header, payload padded to capacity (the CRC covers all of it), erased flash."""
    if len(payload) > layout.capacity:
        sys.exit(f"payload is {len(payload)} bytes, capacity is {layout.capacity}")

    block = payload.ljust(layout.capacity, b"\x00")
    header = bytearray(layout.header_len)
    header[MAGIC_AT] = layout.magic
    header[FORMAT_AT] = layout.format
    header[LENGTH_AT] = len(payload).to_bytes(2, "little")
    header[SEQUENCE_AT] = sequence.to_bytes(4, "little")
    header[CRC_AT] = zlib.crc32(bytes(header[CRC_COVERS]) + block).to_bytes(4, "little")

    return (bytes(header) + block).ljust(layout.erase_size, ERASED)


def image(payload: bytes, layout: Layout, sequence: int) -> bytes:
    """Every slot, the rest erased so no stale record can outrank this one."""
    spare = (layout.slots - 1) * layout.erase_size
    return slot(payload, layout, sequence) + ERASED * spare


def decode(blob: bytes, index: int, layout: Layout) -> Record | None:
    """Read back one slot the way `Store::load` does, CRC included."""
    base = index * layout.erase_size
    header = blob[base : base + layout.header_len]
    if len(header) < layout.header_len:
        return None
    if header[MAGIC_AT] != layout.magic or header[FORMAT_AT] != layout.format:
        return None

    length = int.from_bytes(header[LENGTH_AT], "little")
    if length > layout.capacity:
        return None

    start = base + layout.header_len
    block = blob[start : start + layout.capacity]
    intact = len(block) == layout.capacity and zlib.crc32(
        header[CRC_COVERS] + block
    ) == int.from_bytes(header[CRC_AT], "little")

    return Record(
        index, int.from_bytes(header[SEQUENCE_AT], "little"), block[:length], intact
    )


def current(records: list[Record | None]) -> Record | None:
    """The slot the firmware would load: intact, and newest by sequence."""
    best: Record | None = None
    for record in records:
        if record is None or not record.intact:
            continue
        # Wrapping comparison, as in `supersedes`.
        ahead = (record.sequence - best.sequence) % 2**32 if best else 1
        if ahead and ahead < 0x8000_0000:
            best = record
    return best


def describe(payload: bytes) -> str:
    """A one-line rendering of a payload, for a human checking their work."""
    shown = payload[:16].hex(" ")
    if len(payload) > 16:
        shown += " ..."
    text = "".join(chr(b) if 0x20 <= b < 0x7F else "." for b in payload[:32])
    return f"{shown}  |{text}|"


def report(blob: bytes, layout: Layout) -> Record | None:
    """Print what an image holds, slot by slot."""
    records = [decode(blob, i, layout) for i in range(layout.slots)]
    for index, record in enumerate(records):
        if record is None:
            print(f"  slot {index}: empty")
            continue
        state = "intact" if record.intact else "CRC MISMATCH"
        print(
            f"  slot {index}: seq {record.sequence}, {len(record.payload)} bytes, {state}"
        )
        print(f"           {describe(record.payload)}")

    live = current(records)
    if live is None:
        print("  the firmware would find no record here and fall back to its defaults")
    else:
        print(f"  the firmware would load slot {live.slot}")
        if len(live.payload) >= 4:
            counter = int.from_bytes(live.payload[:4], "little")
            print(f"  first four bytes as a boot count: {counter}")
    return live


def payload_from(args: argparse.Namespace) -> bytes:
    """Whichever payload source was asked for."""
    if args.boot_count is not None:
        return args.boot_count.to_bytes(4, "little")
    if args.text is not None:
        return args.text.encode()
    if args.hex is not None:
        try:
            return bytes.fromhex(args.hex.replace(" ", ""))
        except ValueError as error:
            sys.exit(f"--hex: {error}")
    return args.file.read_bytes()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])

    source = parser.add_mutually_exclusive_group()
    source.add_argument(
        "--boot-count",
        type=int,
        metavar="N",
        help="a u32 counter, what the firmware reads today",
    )
    source.add_argument("--text", metavar="STRING", help="UTF-8 bytes")
    source.add_argument("--hex", metavar="HEX", help="raw bytes, e.g. '00 01 ff'")
    source.add_argument(
        "--file", type=Path, metavar="PATH", help="raw bytes from a file"
    )
    source.add_argument(
        "--inspect",
        type=Path,
        metavar="IMAGE",
        help="decode an image and exit, writing nothing",
    )

    parser.add_argument(
        "--out", type=Path, default=Path("settings.bin"), help="image to write"
    )
    parser.add_argument(
        "--sequence",
        type=int,
        default=0,
        help="sequence number of the provisioned record (default 0, a fresh device)",
    )
    parser.add_argument(
        "--board", default=DEFAULT_BOARD, help="board whose capacity to use"
    )
    parser.add_argument(
        "--capacity", type=int, help="override the board's SETTINGS_CAPACITY"
    )
    parser.add_argument("--erase-size", type=int, default=DEFAULT_ERASE_SIZE)
    parser.add_argument(
        "--storage", type=Path, default=STORAGE_RS, help="where the format lives"
    )
    parser.add_argument(
        "--offset",
        type=lambda value: int(value, 0),
        default=DEFAULT_OFFSET,
        help="flash offset of the partition (default 0x9000, espflash's nvs)",
    )
    parser.add_argument(
        "--flash", action="store_true", help="run espflash write-bin afterwards"
    )
    parser.add_argument("--port", help="serial port to pass to espflash")
    args = parser.parse_args()

    layout = read_layout(args.storage, args.board, args.capacity, args.erase_size)

    if args.inspect is not None:
        blob = args.inspect.read_bytes()
        print(f"{args.inspect}: {len(blob):,} bytes, {layout.capacity}-byte capacity")
        report(blob, layout)
        return

    if (
        args.boot_count is None
        and args.text is None
        and args.hex is None
        and args.file is None
    ):
        parser.error("give a payload: --boot-count, --text, --hex or --file")

    payload = payload_from(args)
    blob = image(payload, layout, args.sequence)

    # Decode what was just built, as --inspect would.
    live = current([decode(blob, i, layout) for i in range(layout.slots)])
    if live is None or live.payload != payload:
        sys.exit("built an image this script cannot read back; refusing to write it")

    args.out.write_bytes(blob)
    print(f"{args.out}: {len(blob):,} bytes covering {layout.slots} slots")
    print(
        f"  payload: {len(payload)} of {layout.capacity} bytes, sequence {args.sequence}"
    )
    print(f"           {describe(payload)}")

    command = ["espflash", "write-bin", f"0x{args.offset:x}", str(args.out)]
    if args.port:
        command += ["--port", args.port]
    print("\n" + " ".join(command))

    if not args.flash:
        return
    if shutil.which(command[0]) is None:
        sys.exit(f"{command[0]}: not installed")
    sys.exit(subprocess.run(command, check=False).returncode)


if __name__ == "__main__":
    main()
