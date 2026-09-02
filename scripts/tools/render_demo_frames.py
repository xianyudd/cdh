#!/usr/bin/env python3
"""Rasterize tmux pane captures (ANSI text) into PNG frames on a fixed grid.

Every cell is drawn at an exact integer step, so a missing or mis-advancing
glyph can only make one cell ugly -- it can never shift the columns.

Braille cells are synthesized on the 2x4 sub-grid rather than taken from the
font: no monospace font tiles Braille on both axes at once (Cascadia Code's dot
pitch is 0.208em vertically and 0.234em horizontally, and a terminal cell is
0.586em wide), and the ambient cube needs the dots to touch across cell
boundaries or its edges break into scattered specks.

Everything else comes from the font chain: each codepoint is drawn with the first
font whose cmap maps it, and a codepoint no font in the chain covers fails the
run instead of shipping a .notdef box.
"""

from __future__ import annotations

import argparse
import re
import struct
import sys
import unicodedata
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

BRAILLE_BASE = 0x2800
BRAILLE_LAST = 0x28FF

# Dot bit -> (sub-column, sub-row) on the 2x4 grid. Bits 0..5 run down the two
# columns three at a time; bits 6 and 7 are the bottom row added by 8-dot
# Braille, which is why they are not simply "next in the column".
BRAILLE_DOTS = (
    (0, 0),
    (0, 1),
    (0, 2),
    (1, 0),
    (1, 1),
    (1, 2),
    (0, 3),
    (1, 3),
)

# One pass over every escape sequence, SGR first. Stripping "other" escapes in a
# separate earlier pass is what silently rendered a whole run in monochrome: the
# generic CSI alternative ends in [A-Za-z], which also matches the `m` of an SGR.
ESCAPE_RE = re.compile(
    r"\x1b\[([0-9;:]*)m"
    r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"
    r"|\x1b\[[0-9;?]*[A-Za-z]"
    r"|\x1b[()][B0]"
    r"|\x1b."
)

BASIC_COLORS = (
    (0x00, 0x00, 0x00),
    (0xCD, 0x00, 0x00),
    (0x00, 0xCD, 0x00),
    (0xCD, 0xCD, 0x00),
    (0x00, 0x00, 0xEE),
    (0xCD, 0x00, 0xCD),
    (0x00, 0xCD, 0xCD),
    (0xE5, 0xE5, 0xE5),
    (0x7F, 0x7F, 0x7F),
    (0xFF, 0x00, 0x00),
    (0x00, 0xFF, 0x00),
    (0xFF, 0xFF, 0x00),
    (0x5C, 0x5C, 0xFF),
    (0xFF, 0x00, 0xFF),
    (0x00, 0xFF, 0xFF),
    (0xFF, 0xFF, 0xFF),
)


def xterm256(index: int) -> tuple[int, int, int]:
    if index < 16:
        return BASIC_COLORS[index]
    if index < 232:
        index -= 16
        levels = (0, 95, 135, 175, 215, 255)
        return (levels[index // 36], levels[(index // 6) % 6], levels[index % 6])
    gray = 8 + 10 * (index - 232)
    return (gray, gray, gray)


def parse_color(spec: str) -> tuple[int, int, int]:
    text = spec.strip().lstrip("#")
    return (int(text[0:2], 16), int(text[2:4], 16), int(text[4:6], 16))


def char_width(ch: str) -> int:
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


def sfnt_tables(data: bytes, base: int) -> dict[bytes, int]:
    count = struct.unpack_from(">H", data, base + 4)[0]
    tables = {}
    for index in range(count):
        record = base + 12 + index * 16
        tables[data[record : record + 4]] = struct.unpack_from(">I", data, record + 8)[0]
    return tables


def cmap_subtable_coverage(data: bytes, offset: int) -> set[int]:
    """Codepoints one cmap subtable maps to a glyph other than .notdef."""
    kind = struct.unpack_from(">H", data, offset)[0]
    covered: set[int] = set()
    if kind == 4:
        segments_x2 = struct.unpack_from(">H", data, offset + 6)[0]
        segments = segments_x2 // 2
        ends = struct.unpack_from(f">{segments}H", data, offset + 14)
        starts = struct.unpack_from(f">{segments}H", data, offset + 16 + segments_x2)
        deltas = struct.unpack_from(f">{segments}h", data, offset + 16 + 2 * segments_x2)
        range_base = offset + 16 + 3 * segments_x2
        range_offsets = struct.unpack_from(f">{segments}H", data, range_base)
        for index in range(segments):
            start, end = starts[index], min(ends[index], 0xFFFE)
            for code in range(start, end + 1):
                if range_offsets[index] == 0:
                    glyph = (code + deltas[index]) & 0xFFFF
                else:
                    at = range_base + index * 2 + range_offsets[index] + (code - start) * 2
                    if at + 2 > len(data):
                        continue
                    glyph = struct.unpack_from(">H", data, at)[0]
                    if glyph:
                        glyph = (glyph + deltas[index]) & 0xFFFF
                if glyph:
                    covered.add(code)
    elif kind == 12:
        groups = struct.unpack_from(">I", data, offset + 12)[0]
        for index in range(groups):
            start, end, glyph = struct.unpack_from(">III", data, offset + 16 + index * 12)
            if glyph:
                covered.update(range(start, min(end, 0x10FFFF) + 1))
    elif kind == 6:
        first, count = struct.unpack_from(">HH", data, offset + 6)
        glyphs = struct.unpack_from(f">{count}H", data, offset + 10)
        covered.update(first + index for index, glyph in enumerate(glyphs) if glyph)
    elif kind == 0:
        glyphs = data[offset + 6 : offset + 6 + 256]
        covered.update(index for index, glyph in enumerate(glyphs) if glyph)
    return covered


def font_coverage(path: str, face_index: int = 0) -> frozenset[int]:
    """Read the codepoints a font actually maps, straight out of its cmap.

    The fallback chain has to be decided per codepoint, because a font missing a
    glyph draws .notdef -- an on-screen tofu box and no error anywhere. That is
    how a full-width question mark from the help overlay shipped as a box. Only
    the Unicode subtables are consulted, and a segment mapping to glyph 0 does
    not count as coverage, or the guard in main() would trust a box.
    """
    data = Path(path).read_bytes()
    base = 0
    if data[:4] == b"ttcf":
        faces = struct.unpack_from(">I", data, 8)[0]
        if face_index >= faces:
            raise SystemExit(f"font {path} has {faces} faces, no index {face_index}")
        base = struct.unpack_from(">I", data, 12 + 4 * face_index)[0]
    cmap = sfnt_tables(data, base).get(b"cmap")
    if cmap is None:
        raise SystemExit(f"font {path} has no cmap table")
    subtables = struct.unpack_from(">H", data, cmap + 2)[0]
    covered: set[int] = set()
    for index in range(subtables):
        platform, encoding, offset = struct.unpack_from(">HHI", data, cmap + 4 + index * 8)
        if platform == 0 or (platform == 3 and encoding in (1, 10)):
            covered |= cmap_subtable_coverage(data, cmap + offset)
    return frozenset(covered)


class Pen:
    __slots__ = ("fg", "bg", "bold", "reverse")

    def __init__(self, fg, bg):
        self.fg = fg
        self.bg = bg
        self.bold = False
        self.reverse = False


def apply_sgr(pen: Pen, params: str, default_fg, default_bg) -> None:
    # An empty parameter list means SGR 0. Sub-parameters are colon-separated in
    # the ITU form (38:2:...), so flatten both separators into one code stream.
    codes = [int(part) if part else 0 for part in re.split(r"[;:]", params or "0")]
    index = 0
    while index < len(codes):
        code = codes[index]
        if code == 0:
            pen.fg, pen.bg = default_fg, default_bg
            pen.bold = False
            pen.reverse = False
        elif code == 1:
            pen.bold = True
        elif code in (21, 22):
            pen.bold = False
        elif code == 7:
            pen.reverse = True
        elif code == 27:
            pen.reverse = False
        elif 30 <= code <= 37:
            pen.fg = BASIC_COLORS[code - 30]
        elif 90 <= code <= 97:
            pen.fg = BASIC_COLORS[code - 90 + 8]
        elif code == 39:
            pen.fg = default_fg
        elif 40 <= code <= 47:
            pen.bg = BASIC_COLORS[code - 40]
        elif 100 <= code <= 107:
            pen.bg = BASIC_COLORS[code - 100 + 8]
        elif code == 49:
            pen.bg = default_bg
        elif code in (38, 48):
            target_fg = code == 38
            kind = codes[index + 1] if index + 1 < len(codes) else 5
            if kind == 5:
                color = xterm256(codes[index + 2] if index + 2 < len(codes) else 0)
                index += 2
            elif kind == 2:
                channels = codes[index + 2 : index + 5]
                while len(channels) < 3:
                    channels.append(0)
                color = tuple(min(255, max(0, value)) for value in channels)
                index += 4
            else:
                index += 1
                color = default_fg if target_fg else default_bg
            if target_fg:
                pen.fg = color
            else:
                pen.bg = color
        index += 1


def parse_frame(text: str, cols: int, rows: int, default_fg, default_bg):
    """Turn one `tmux capture-pane -e -p` dump into a rows x cols cell grid."""
    grid = [[(" ", default_fg, default_bg, False) for _ in range(cols)] for _ in range(rows)]
    for row_index, raw_line in enumerate(text.split("\n")[:rows]):
        line = raw_line.replace("\r", "")
        pen = Pen(default_fg, default_bg)
        column = 0
        position = 0
        for match in ESCAPE_RE.finditer(line):
            column = emit(grid[row_index], line[position : match.start()], pen, column, cols)
            params = match.group(1)
            if params is not None:
                apply_sgr(pen, params, default_fg, default_bg)
            position = match.end()
        emit(grid[row_index], line[position:], pen, column, cols)
    return grid


def emit(row, text: str, pen: Pen, column: int, cols: int) -> int:
    fg, bg = (pen.bg, pen.fg) if pen.reverse else (pen.fg, pen.bg)
    for ch in text:
        if column >= cols:
            break
        width = char_width(ch)
        row[column] = (ch, fg, bg, pen.bold)
        for filler in range(column + 1, min(column + width, cols)):
            row[filler] = ("", fg, bg, False)
        column += width
    return column


class FontFace:
    __slots__ = ("regular", "bold", "coverage")

    def __init__(self, path: str, size: float):
        self.regular = ImageFont.truetype(path, size)
        self.bold = ImageFont.truetype(path, size)
        try:
            self.bold.set_variation_by_name("Bold")
        except OSError:
            self.bold = self.regular
        self.coverage = font_coverage(path)


class Geometry:
    def __init__(self, cols: int, rows: int, cell_w: int, cell_h: int, fonts, font_size: float):
        self.cols = cols
        self.rows = rows
        self.cell_w = cell_w
        self.cell_h = cell_h
        self.width = cols * cell_w
        self.height = rows * cell_h
        self.faces = [FontFace(path, font_size) for path in fonts]
        # One baseline for the whole chain, taken from the primary font: a
        # fallback glyph has to sit on the same line as its neighbours, which its
        # own metrics would not guarantee.
        ascent, descent = self.faces[0].regular.getmetrics()
        self.baseline = (cell_h - (ascent + descent)) // 2 + ascent

    def face_for(self, code: int):
        for face in self.faces:
            if code in face.coverage:
                return face
        return None

    def sub_x(self, index: int) -> int:
        return round(index * self.cell_w / 2)

    def sub_y(self, index: int) -> int:
        return round(index * self.cell_h / 4)


def braille_bits(ch: str):
    if len(ch) != 1:
        return None
    code = ord(ch)
    if not BRAILLE_BASE <= code <= BRAILLE_LAST:
        return None
    return code - BRAILLE_BASE


def row_text(row) -> str:
    """One character per cell, so a string index is also a column index."""
    return "".join(ch or "\x00" for ch, _fg, _bg, _bold in row)


def mask_path(grid, needle: str, replacement: str, default_fg, default_bg) -> int:
    """Swap a throwaway path prefix for a stable one without moving a column.

    Done in cell space rather than on the captured text because the replacement
    is shorter: the rest of the path slides left to stay contiguous, and the
    slack is paid back as blanks at the end of the row. A row carrying anything
    besides blanks behind the path is refused instead of silently shifted.
    """

    def is_blank(cell) -> bool:
        return cell[0] in ("", " ") and cell[2] == default_bg

    blank = (" ", default_fg, default_bg, False)
    hits = 0
    for row_index, row in enumerate(grid):
        cols = len(row)
        start = row_text(row).find(needle)
        while start >= 0:
            end = start + len(needle)
            stop = end
            while stop < cols and not is_blank(row[stop]):
                stop += 1
            if any(not is_blank(cell) for cell in row[stop:]):
                raise SystemExit(
                    f"mask: row {row_index} carries content past column {stop}; "
                    "shortening the path would shift it out of its captured column"
                )
            style = row[start][1:]
            row[start:] = (
                [(ch, *style) for ch in replacement]
                + row[end:stop]
                + [blank] * (cols - stop + len(needle) - len(replacement))
            )
            hits += 1
            start = row_text(row).find(needle, start + len(replacement))
    return hits


def forbidden_hits(grid, needles) -> str | None:
    for row in grid:
        text = row_text(row)
        for needle in needles:
            if needle in text:
                return needle
    return None


def render_frame(grid, geom: Geometry, default_bg, missing: set[int]) -> Image.Image:
    image = Image.new("RGB", (geom.width, geom.height), default_bg)
    draw = ImageDraw.Draw(image)

    for row_index, row in enumerate(grid):
        top = row_index * geom.cell_h
        run_start = 0
        while run_start < geom.cols:
            bg = row[run_start][2]
            run_end = run_start + 1
            while run_end < geom.cols and row[run_end][2] == bg:
                run_end += 1
            if bg != default_bg:
                draw.rectangle(
                    (run_start * geom.cell_w, top, run_end * geom.cell_w - 1, top + geom.cell_h - 1),
                    fill=bg,
                )
            run_start = run_end

    # Dot radius keeps the Braille cells reading as dots while still touching
    # across a cell boundary: the contact band is cell_w/2 - 2*radius wide, so
    # no pixel row between two stacked dots is left blank.
    radius = max(1, round(min(geom.cell_w / 2, geom.cell_h / 4) * 0.28))
    for row_index, row in enumerate(grid):
        top = row_index * geom.cell_h
        for col_index, (ch, fg, _bg, bold) in enumerate(row):
            if ch in ("", " "):
                continue
            left = col_index * geom.cell_w
            bits = braille_bits(ch)
            if bits is not None:
                for bit, (sub_col, sub_row) in enumerate(BRAILLE_DOTS):
                    if not bits & (1 << bit):
                        continue
                    x0 = left + geom.sub_x(sub_col)
                    x1 = left + geom.sub_x(sub_col + 1) - 1
                    y0 = top + geom.sub_y(sub_row)
                    y1 = top + geom.sub_y(sub_row + 1) - 1
                    draw.rounded_rectangle((x0, y0, x1, y1), radius=radius, fill=fg)
                continue
            face = geom.face_for(ord(ch))
            if face is None:
                missing.add(ord(ch))
                face = geom.faces[0]
            draw.text(
                (left, top + geom.baseline),
                ch,
                font=face.bold if bold else face.regular,
                fill=fg,
                anchor="ls",
            )
    return image


def check_braille_contact(image: Image.Image, grid, geom: Geometry) -> int:
    """Assert stacked Braille dots leave no blank pixel row at the cell seam.

    Guards the synthesis above against a regression that reintroduces a gap
    (an inset, a smaller dot, a cell height that stops dividing by four): a
    seam gap is exactly what shattered the cube into specks in the HTML player.
    """
    pixels = image.load()
    checked = 0
    for row_index in range(geom.rows - 1):
        for col_index in range(geom.cols):
            upper = braille_bits(grid[row_index][col_index][0])
            lower = braille_bits(grid[row_index + 1][col_index][0])
            if not upper or not lower:
                continue
            seam = (row_index + 1) * geom.cell_h
            left = col_index * geom.cell_w
            for sub_col in range(2):
                bottom_bit = 1 << (6 + sub_col)
                top_bit = 1 << (0 if sub_col == 0 else 3)
                if not (upper & bottom_bit and lower & top_bit):
                    continue
                x0 = left + geom.sub_x(sub_col)
                x1 = left + geom.sub_x(sub_col + 1)
                for y in range(seam - 2, seam + 2):
                    background = grid[row_index + (0 if y < seam else 1)][col_index][2]
                    if all(pixels[x, y] == background for x in range(x0, x1)):
                        raise SystemExit(
                            f"braille seam gap: blank pixel row y={y} under cell "
                            f"(row={row_index}, col={col_index}, sub_col={sub_col})"
                        )
                checked += 1
    return checked


def count_styled_cells(grid, default_fg, default_bg) -> tuple[int, int]:
    """Count cells whose colors came from SGR rather than the defaults.

    A regression in escape handling drops every colour without changing a single
    glyph, so the frames still look plausible in a text diff. The selected list
    row is the one guaranteed non-default background on screen, which is why the
    background count is tracked separately.
    """
    tinted = 0
    filled = 0
    for row in grid:
        for _ch, fg, bg, _bold in row:
            if fg != default_fg:
                tinted += 1
            if bg != default_bg:
                filled += 1
    return tinted, filled


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--frames", required=True, type=Path, help="directory of capture-pane dumps")
    parser.add_argument("--out", required=True, type=Path, help="directory to write PNG frames into")
    parser.add_argument("--font", required=True, help="path to a monospace TTF covering U+2800..28FF")
    parser.add_argument(
        "--fallback-font",
        action="append",
        default=[],
        metavar="TTF",
        help="font to draw codepoints the primary font does not map; repeatable, tried in order",
    )
    parser.add_argument(
        "--mask-path",
        metavar="OLD=NEW",
        help="rewrite a path prefix in every frame; NEW must not be longer than OLD",
    )
    parser.add_argument(
        "--forbid",
        action="append",
        default=[],
        metavar="TEXT",
        help="fail if this text survives into a rendered frame; repeatable",
    )
    parser.add_argument("--cols", required=True, type=int)
    parser.add_argument("--rows", required=True, type=int)
    parser.add_argument("--cell-width", required=True, type=int, help="device pixels per column")
    parser.add_argument("--cell-height", required=True, type=int, help="device pixels per row")
    parser.add_argument("--font-size", required=True, type=float, help="device pixels")
    parser.add_argument("--default-fg", default="#d8e1f5")
    parser.add_argument("--default-bg", default="#141822")
    args = parser.parse_args()

    if args.cell_height % 4 != 0:
        parser.error("--cell-height must be divisible by 4 so the Braille sub-grid tiles exactly")

    needle = replacement = ""
    if args.mask_path:
        needle, _, replacement = args.mask_path.partition("=")
        if not needle or len(replacement) > len(needle):
            parser.error("--mask-path wants OLD=NEW with NEW no longer than OLD")

    default_fg = parse_color(args.default_fg)
    default_bg = parse_color(args.default_bg)
    geom = Geometry(
        args.cols,
        args.rows,
        args.cell_width,
        args.cell_height,
        [args.font, *args.fallback_font],
        args.font_size,
    )

    sources = sorted(args.frames.glob("*.txt"))
    if not sources:
        parser.error(f"no frames found in {args.frames}")
    args.out.mkdir(parents=True, exist_ok=True)

    seams = 0
    tinted = 0
    filled = 0
    masked = 0
    missing: set[int] = set()
    for index, source in enumerate(sources):
        grid = parse_frame(
            source.read_text(encoding="utf-8", errors="replace"),
            args.cols,
            args.rows,
            default_fg,
            default_bg,
        )
        if needle:
            masked += mask_path(grid, needle, replacement, default_fg, default_bg)
        leaked = forbidden_hits(grid, args.forbid)
        if leaked is not None:
            print(f"render: FAIL {source.name} still shows '{leaked}'", file=sys.stderr)
            return 1
        frame_tinted, frame_filled = count_styled_cells(grid, default_fg, default_bg)
        tinted += frame_tinted
        filled += frame_filled
        image = render_frame(grid, geom, default_bg, missing)
        seams += check_braille_contact(image, grid, geom)
        image.save(args.out / f"frame-{index:04d}.png")

    if seams == 0:
        print("render: FAIL no stacked Braille cells found -- the cube never rendered", file=sys.stderr)
        return 1
    if tinted == 0 or filled == 0:
        print(
            f"render: FAIL colour was dropped ({tinted} tinted cells, {filled} filled cells)",
            file=sys.stderr,
        )
        return 1
    if needle and masked == 0:
        print(f"render: FAIL '{needle}' never appeared, so nothing was masked", file=sys.stderr)
        return 1
    if missing:
        codes = " ".join(f"U+{code:04X}" for code in sorted(missing))
        print(
            f"render: FAIL no font in the chain maps {codes} -- they drew as tofu boxes",
            file=sys.stderr,
        )
        return 1
    print(
        f"render: {len(sources)} frames at {geom.width}x{geom.height}, "
        f"{seams} Braille seams verified, {tinted} tinted and {filled} filled cells, "
        f"{masked} paths masked, {len(geom.faces)} fonts in the chain"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
