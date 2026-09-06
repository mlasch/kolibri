#!/usr/bin/env python3
"""Report what the firmware costs in flash and in RAM.

Both numbers matter on a microcontroller and neither is visible in a build log:
`cargo build` prints nothing about size, and the ELF on disk is mostly debug
info -- 2.6 MB of it here -- so its file size says nothing about what reaches
the chip.

The report is derived from two files the build already produced, so it cannot
drift away from the firmware: section addresses and sizes come out of the ELF,
and the memory regions they are measured against come out of the `memory.x`
that esp-hal's build script emitted for this chip and that the linker actually
used. Flash figures come from espflash, which assembles the image that gets
written to the part.

    python3 tools/fw-size.py target/riscv32imc-unknown-none-elf/release/kolibri \
        --chip esp32c3

Add `--format markdown` for a report to paste into a pull request; CI does that
and posts it as a comment.
"""

from __future__ import annotations

import argparse
import ast
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

# ELF constants. Only the 32-bit little-endian flavour is handled: every target
# this project can plausibly grow to (RISC-V, Xtensa, Cortex-M) is ELF32.
SHF_ALLOC = 0x2
SHT_NOBITS = 8

# Sections the linker uses to pad a region out to an alignment boundary. They
# occupy address space without holding anything.
PADDING_SUFFIX = "_dummy"

# The section esp-hal points the stack at. It is whatever DRAM is left over, so
# counting it as "used" would report every firmware as filling the chip.
STACK_SECTION = ".stack"


@dataclass
class Region:
    """One entry of the linker script's MEMORY block."""

    name: str
    origin: int
    length: int
    sections: list[Section] = field(default_factory=list)

    @property
    def end(self) -> int:
        return self.origin + self.length

    @property
    def is_flash(self) -> bool:
        """Memory-mapped flash rather than on-chip SRAM.

        Espressif calls the two flash windows IROM and DROM; a Cortex-M
        `memory.x` conventionally calls its one window FLASH.
        """
        upper = self.name.upper()
        return "ROM" in upper or "FLASH" in upper

    @property
    def used(self) -> int:
        """Bytes the firmware costs: everything but the leftover stack.

        Alignment padding counts. It holds nothing, but it reserves address
        space that nothing else can be placed in either.
        """
        return sum(s.size for s in self.sections if s.name != STACK_SECTION)

    @property
    def padding(self) -> int:
        return sum(s.size for s in self.sections if s.name.endswith(PADDING_SUFFIX))

    @property
    def stack(self) -> int:
        return sum(s.size for s in self.sections if s.name == STACK_SECTION)


@dataclass
class Section:
    name: str
    addr: int
    size: int
    nobits: bool

    @property
    def in_flash(self) -> bool:
        """Whether the section's contents are stored in the flash image.

        NOBITS sections (.bss, .stack) have no contents to store; everything
        else does, including the RAM-resident code and data that the startup
        code copies out of flash.
        """
        return not self.nobits


# ---------------------------------------------------------------------------
# ELF
# ---------------------------------------------------------------------------


def read_sections(elf: Path) -> list[Section]:
    """Return the allocated, non-empty sections of an ELF32 file."""
    blob = elf.read_bytes()
    if blob[:4] != b"\x7fELF":
        sys.exit(f"{elf}: not an ELF file")
    if blob[4] != 1 or blob[5] != 1:
        sys.exit(f"{elf}: expected a 32-bit little-endian ELF")

    # e_shoff at 0x20, then e_flags and the two header sizes before the
    # section header table's own count and string-table index.
    sh_off, sh_entsize, sh_num, sh_strndx = struct.unpack_from("<I10xHHH", blob, 0x20)

    def header(i: int) -> tuple[int, ...]:
        return struct.unpack_from("<10I", blob, sh_off + i * sh_entsize)

    # Section names live in their own string table, itself a section.
    str_off, str_size = header(sh_strndx)[4:6]
    strtab = blob[str_off : str_off + str_size]

    sections = []
    for i in range(sh_num):
        name_off, sh_type, flags, addr, _, size = header(i)[:6]
        if not flags & SHF_ALLOC or size == 0:
            continue
        name = strtab[name_off : strtab.index(b"\0", name_off)].decode()
        sections.append(Section(name, addr, size, sh_type == SHT_NOBITS))
    return sections


# ---------------------------------------------------------------------------
# Linker script
# ---------------------------------------------------------------------------

MEMORY_ENTRY = re.compile(
    r"^\s*(?P<name>\w+)\s*(?:\([^)]*\))?\s*:\s*"
    r"ORIGIN\s*=\s*(?P<origin>[^,]+),\s*"
    r"(?:LENGTH|len|l)\s*=\s*(?P<length>.+?)\s*$",
    re.IGNORECASE,
)
SIZE_SUFFIX = re.compile(r"\b(0[xX][0-9a-fA-F]+|\d+)\s*([KMkm])\b")
REFERENCE = re.compile(r"\b(ORIGIN|LENGTH)\s*\(\s*(\w+)\s*\)", re.IGNORECASE)


def choose_memory_x(
    elf: Path, sections: list[Section]
) -> tuple[Path, dict[str, Region]]:
    """Find the linker script that describes the chip this ELF was linked for.

    Several crates in one dependency tree ship a memory.x -- oled_async carries
    one for an STM32 example board -- and their build scripts drop them side by
    side under the target directory. Neither name nor timestamp tells them
    apart: whichever crate happened to build last is newest, which is how CI
    came to measure an ESP32-C3 firmware against an STM32F103's 20 KB of RAM.

    So the candidates are tried rather than guessed. The regions of the script
    the linker actually used are the ones the ELF's sections fall inside.
    """
    candidates = sorted(elf.parent.glob("build/*/out/memory.x"))
    if not candidates:
        sys.exit(f"no memory.x found near {elf}; pass --memory-x explicitly")

    def explains(candidate: Path) -> tuple[int, dict[str, Region]]:
        regions = parse_regions(candidate)
        return sum(s.size for s in sections if find_region(s, regions)), regions

    scored = [(*explains(candidate), candidate) for candidate in candidates]
    placed, regions, memory_x = max(scored, key=lambda entry: entry[0])
    if placed == 0:
        sys.exit(
            f"no linker script near {elf} describes it; tried "
            + ", ".join(str(c) for c in candidates)
        )
    return memory_x, regions


def parse_regions(memory_x: Path) -> dict[str, Region]:
    """Parse the MEMORY block of a linker script into regions."""
    text = re.sub(r"/\*.*?\*/", "", memory_x.read_text(), flags=re.DOTALL)
    match = re.search(r"\bMEMORY\b\s*\{(.*?)\}", text, re.DOTALL)
    if not match:
        sys.exit(f"{memory_x}: no MEMORY block")

    exprs: dict[str, tuple[str, str]] = {}
    for line in match.group(1).splitlines():
        entry = MEMORY_ENTRY.match(line.rstrip().rstrip(","))
        if entry:
            exprs[entry["name"]] = (entry["origin"], entry["length"])

    resolving: set[str] = set()

    def evaluate(expr: str) -> int:
        """Evaluate a linker expression: literals, K/M suffixes, ORIGIN/LENGTH."""

        def substitute(ref: re.Match[str]) -> str:
            name = ref.group(2)
            if name not in exprs:
                sys.exit(f"{memory_x}: unknown region {name}")
            if name in resolving:
                sys.exit(f"{memory_x}: circular reference to {name}")
            resolving.add(name)
            origin, length = exprs[name]
            value = evaluate(origin if ref.group(1).upper() == "ORIGIN" else length)
            resolving.discard(name)
            return str(value)

        expr = REFERENCE.sub(substitute, expr)
        expr = SIZE_SUFFIX.sub(
            lambda m: str(int(m[1], 0) * (1024 if m[2] in "Kk" else 1024 * 1024)), expr
        )
        return arithmetic(expr, memory_x)

    return {
        name: Region(name, evaluate(origin), evaluate(length))
        for name, (origin, length) in exprs.items()
    }


def arithmetic(expr: str, source: Path) -> int:
    """Evaluate an integer arithmetic expression without executing code."""
    operators = {
        ast.Add: lambda a, b: a + b,
        ast.Sub: lambda a, b: a - b,
        ast.Mult: lambda a, b: a * b,
        ast.FloorDiv: lambda a, b: a // b,
        ast.Div: lambda a, b: a // b,
    }

    def visit(node: ast.AST) -> int:
        if isinstance(node, ast.Expression):
            return visit(node.body)
        if isinstance(node, ast.Constant) and isinstance(node.value, int):
            return node.value
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, (ast.UAdd, ast.USub)):
            value = visit(node.operand)
            return value if isinstance(node.op, ast.UAdd) else -value
        if isinstance(node, ast.BinOp) and type(node.op) in operators:
            return operators[type(node.op)](visit(node.left), visit(node.right))
        sys.exit(f"{source}: cannot evaluate {ast.dump(node)}")

    try:
        return visit(ast.parse(expr.strip(), mode="eval"))
    except SyntaxError:
        sys.exit(f"{source}: cannot parse expression {expr!r}")


def find_region(section: Section, regions: dict[str, Region]) -> Region | None:
    """The region a section lives in.

    Smallest containing region wins: on Espressif the cache window sits inside
    the IRAM window, and the tighter one is the real home.
    """
    ordered = sorted(regions.values(), key=lambda r: r.length)
    return next((r for r in ordered if r.origin <= section.addr < r.end), None)


def assign(sections: list[Section], regions: dict[str, Region]) -> None:
    """File each section under its region, and refuse to report on a stray.

    A section outside every region means the regions are not this ELF's, and a
    report measured against the wrong memory map is worse than no report: it is
    wrong in a way that looks entirely plausible.
    """
    strays = []
    for section in sections:
        home = find_region(section, regions)
        if home is None:
            strays.append(section)
        else:
            home.sections.append(section)
    if strays:
        sys.exit(
            "outside every memory region, so these regions are not this ELF's: "
            + ", ".join(s.name for s in strays)
        )


# ---------------------------------------------------------------------------
# Flash image
# ---------------------------------------------------------------------------


@dataclass
class Image:
    """What espflash would write to the part."""

    app: int
    partition: int
    flashed: int | None = None


APP_SIZE = re.compile(r"App/part\. size:\s*([\d,]+)\s*/\s*([\d,]+)")

# Cells that hold a plain number, a percentage or an address get right-aligned.
NUMBER = re.compile(r"[\d,]+|\d+\.\d%|0x[0-9a-f]+")


def measure_image(elf: Path, chip: str, flashed: Path | None) -> Image | None:
    """Ask espflash how large the app image is, and how much partition it has.

    The app image is the part that is rebuilt on every flash; the partition it
    has to fit in is the budget that actually constrains the firmware.
    """
    if shutil.which("espflash") is None:
        print("note: espflash not on PATH, skipping flash figures", file=sys.stderr)
        return None

    with tempfile.TemporaryDirectory() as tmp:
        result = subprocess.run(
            ["espflash", "save-image", str(elf), "--chip", chip, f"{tmp}/app.bin"],
            capture_output=True,
            text=True,
            check=False,
        )
    output = result.stdout + result.stderr
    if result.returncode != 0:
        sys.exit(f"espflash save-image failed:\n{output}")

    match = APP_SIZE.search(output)
    if not match:
        print("note: could not read the size out of espflash", file=sys.stderr)
        return None

    return Image(
        app=int(match[1].replace(",", "")),
        partition=int(match[2].replace(",", "")),
        flashed=flashed.stat().st_size if flashed else None,
    )


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------


def percent(part: int, whole: int) -> str:
    return f"{100 * part / whole:.1f}%" if whole else "-"


def render(
    regions: dict[str, Region],
    image: Image | None,
    label: str,
    markdown: bool,
    marker: str | None,
) -> str:
    """Build the report. The markdown flavour is what CI posts to a PR."""
    placed = [r for r in regions.values() if r.sections]
    ram = [r for r in placed if not r.is_flash]
    resident = sum(s.size for r in placed for s in r.sections if s.in_flash)

    out: list[str] = []
    if marker:
        out.append(marker)
    out += heading(f"Firmware size -- {label}", 2, markdown)

    if image:
        out += heading("Flash", 3, markdown)
        rows = [
            (
                "app image",
                f"{image.app:,}",
                f"{percent(image.app, image.partition)} of the"
                f" {image.partition:,}-byte app partition",
            )
        ]
        if image.flashed is not None:
            rows.append(
                (
                    "flashed image",
                    f"{image.flashed:,}",
                    "bootloader + partition table + app",
                )
            )
        rows.append(
            (
                "section contents",
                f"{resident:,}",
                "the rest of the app image is segment headers and"
                " 64 KB cache-alignment padding",
            )
        )
        out += table(["", "bytes", ""], rows, markdown)

    out += heading("SRAM at boot", 3, markdown)
    rows = []
    for region in sorted(ram, key=lambda r: r.origin):
        notes = []
        if region.padding:
            notes.append(f"{region.padding:,} of it alignment padding")
        if region.stack:
            notes.append(f"{region.stack:,} bytes left over for the stack")
        rows.append(
            (
                region.name,
                f"{region.used:,}",
                f"{region.length:,}",
                percent(region.used, region.length),
                "; ".join(notes),
            )
        )
    out += table(["region", "used", "size", "", ""], rows, markdown)
    out += [
        "",
        "Static allocation only -- the picture before `main` runs, which says"
        " nothing about how deep the stack goes later. Regions can be separate"
        " windows onto the same physical SRAM, so free space does not"
        " necessarily add up across them.",
    ]

    out += ["", "<details><summary>Sections</summary>"] if markdown else [""]
    for region in sorted(placed, key=lambda r: r.origin):
        kind = "flash" if region.is_flash else "SRAM"
        out += heading(f"{region.name} ({kind})", 4, markdown)
        out += table(
            ["section", "bytes", "address"],
            [
                (s.name, f"{s.size:,}", f"0x{s.addr:08x}")
                for s in sorted(region.sections, key=lambda s: s.addr)
            ],
            markdown,
        )
    if markdown:
        out += ["", "</details>"]

    return "\n".join(out).strip() + "\n"


def heading(text: str, level: int, markdown: bool) -> list[str]:
    return ["", f"{'#' * level} {text}" if markdown else f"{text}:"]


def table(headers: list[str], rows: list[tuple[str, ...]], markdown: bool) -> list[str]:
    """Lay out a table, right-aligning the columns that hold only numbers."""
    if not rows:
        return []
    numeric = [
        all(NUMBER.fullmatch(row[i]) for row in rows) for i in range(len(headers))
    ]
    if markdown:
        return [
            "",
            "| " + " | ".join(headers) + " |",
            "|" + "|".join("--:" if n else "---" for n in numeric) + "|",
            *("| " + " | ".join(row) + " |" for row in rows),
        ]
    widths = [max(len(row[i]) for row in rows) for i in range(len(headers))]
    return [
        (
            "  "
            + "  ".join(
                cell.rjust(widths[i]) if numeric[i] else cell.ljust(widths[i])
                for i, cell in enumerate(row)
            )
        ).rstrip()
        for row in rows
    ]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("elf", type=Path, help="the linked firmware ELF")
    parser.add_argument("--chip", help="chip to ask espflash about, e.g. esp32c3")
    parser.add_argument(
        "--image", type=Path, help="the flashable image built alongside the ELF"
    )
    parser.add_argument(
        "--memory-x",
        type=Path,
        help="linker script to read regions from (auto-detected)",
    )
    parser.add_argument("--label", help="what to call this firmware in the report")
    parser.add_argument("--format", choices=["text", "markdown"], default="text")
    parser.add_argument(
        "--marker", help="HTML comment identifying an updatable PR comment"
    )
    parser.add_argument(
        "--out", type=Path, help="write the report here as well as to stdout"
    )
    args = parser.parse_args()

    if not args.elf.is_file():
        sys.exit(f"{args.elf}: no such file -- build the firmware first")

    sections = read_sections(args.elf)
    if args.memory_x:
        memory_x, regions = args.memory_x, parse_regions(args.memory_x)
    else:
        memory_x, regions = choose_memory_x(args.elf, sections)
    print(f"regions from {memory_x}", file=sys.stderr)
    assign(sections, regions)

    image = measure_image(args.elf, args.chip, args.image) if args.chip else None

    report = render(
        regions,
        image,
        args.label or args.elf.name,
        args.format == "markdown",
        args.marker,
    )
    if args.out:
        args.out.write_text(report)
    print(report, end="")


if __name__ == "__main__":
    main()
