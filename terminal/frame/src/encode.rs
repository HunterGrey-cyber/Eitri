//! WIRE ENCODINGS -- the measuring instruments for the transport question.
//!
//! Two encodings of the same [`TerminalFrame`], sharing one header so the
//! difference between them is purely the cell body:
//!
//! * [`encode_naive`] -- a fixed 15-byte record per cell. The obvious thing.
//! * [`encode_rle`] -- runs keyed on the SGR attribute tuple
//!   `(fg, bg, flags)`, with a repeated-character collapse inside each run.
//!   Terminal screens are overwhelmingly repetitive; this is what exploits it.
//!
//! BOTH ARE EXACTLY REVERSIBLE, and `tests/encode.rs` round-trips every frame
//! every other test produces. A byte count for a lossy encoding is not a
//! measurement, it is a wish.
//!
//! No `serde`, no protobuf. A hand-rolled encoder is ~200 lines, has no
//! dependency, and -- the point -- is small enough that `cargo mutants` can
//! prove the round-trip tests actually constrain it. The numbers it produces
//! are a fair proxy for what protobuf would cost: protobuf would add a field
//! tag per field and a length prefix per message, i.e. it is strictly larger
//! than this, so using these numbers to argue about transport is conservative
//! in the direction that matters.

use crate::frame::{
    CellExtras, CellFlags, ColorOverride, FrameCell, FrameColor, FrameCursor, FrameCursorShape, FrameKind, Rgb,
    RowUpdate, TerminalFrame, TerminalModes,
};

const VERSION: u8 = 1;

/// Bytes per cell in [`encode_naive`], excluding the `extra` payload.
pub const NAIVE_CELL_BYTES: usize = 15;

/// Which encoding produced a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Naive,
    Rle,
}

/// Anything that made a buffer undecodable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadVersion(u8),
    BadTag(&'static str, u8),
    BadChar(u32),
    Trailing(usize),
}

// ---------------------------------------------------------------------------
// varint / zigzag
// ---------------------------------------------------------------------------

fn put_uvarint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_ivarint(out: &mut Vec<u8>, value: i64) {
    put_uvarint(out, ((value << 1) ^ (value >> 63)) as u64);
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        let byte = *self.bytes.get(self.at).ok_or(DecodeError::Truncated)?;
        self.at += 1;
        Ok(byte)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.at.checked_add(n).ok_or(DecodeError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(DecodeError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn uvarint(&mut self) -> Result<u64, DecodeError> {
        let mut value: u64 = 0;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= 64 {
                return Err(DecodeError::Truncated);
            }
        }
    }

    fn ivarint(&mut self) -> Result<i64, DecodeError> {
        let raw = self.uvarint()?;
        Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
    }

    fn char(&mut self) -> Result<char, DecodeError> {
        let raw = self.uvarint()? as u32;
        char::from_u32(raw).ok_or(DecodeError::BadChar(raw))
    }

    fn done(&self) -> Result<(), DecodeError> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(DecodeError::Trailing(self.bytes.len() - self.at))
        }
    }
}

// ---------------------------------------------------------------------------
// colours
// ---------------------------------------------------------------------------

/// Variable-width colour, used by the RLE body and the header.
fn put_color(out: &mut Vec<u8>, color: FrameColor) {
    match color {
        FrameColor::Palette(index) => {
            out.push(0);
            put_uvarint(out, index as u64);
        }
        FrameColor::Rgb(rgb) => {
            out.push(1);
            out.extend_from_slice(&[rgb.r, rgb.g, rgb.b]);
        }
    }
}

fn get_color(reader: &mut Reader<'_>) -> Result<FrameColor, DecodeError> {
    match reader.u8()? {
        0 => Ok(FrameColor::Palette(reader.uvarint()? as u16)),
        1 => {
            let rgb = reader.take(3)?;
            Ok(FrameColor::Rgb(Rgb::new(rgb[0], rgb[1], rgb[2])))
        }
        other => Err(DecodeError::BadTag("color", other)),
    }
}

/// Fixed 4-byte colour, used by the naive body so its record really is fixed.
fn put_color_fixed(out: &mut Vec<u8>, color: FrameColor) {
    match color {
        FrameColor::Palette(index) => {
            out.push(0);
            out.extend_from_slice(&index.to_le_bytes());
            out.push(0);
        }
        FrameColor::Rgb(rgb) => {
            out.push(1);
            out.extend_from_slice(&[rgb.r, rgb.g, rgb.b]);
        }
    }
}

fn get_color_fixed(reader: &mut Reader<'_>) -> Result<FrameColor, DecodeError> {
    let bytes = reader.take(4)?;
    match bytes[0] {
        0 => Ok(FrameColor::Palette(u16::from_le_bytes([bytes[1], bytes[2]]))),
        1 => Ok(FrameColor::Rgb(Rgb::new(bytes[1], bytes[2], bytes[3]))),
        other => Err(DecodeError::BadTag("color", other)),
    }
}

// ---------------------------------------------------------------------------
// extras
// ---------------------------------------------------------------------------

fn put_extras(out: &mut Vec<u8>, extras: &CellExtras) {
    put_uvarint(out, extras.zerowidth.len() as u64);
    for mark in &extras.zerowidth {
        put_uvarint(out, *mark as u64);
    }
    match extras.underline_color {
        Some(color) => {
            out.push(1);
            put_color(out, color);
        }
        None => out.push(0),
    }
}

fn get_extras(reader: &mut Reader<'_>) -> Result<CellExtras, DecodeError> {
    let count = reader.uvarint()? as usize;
    let mut zerowidth = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        zerowidth.push(reader.char()?);
    }
    let underline_color = match reader.u8()? {
        0 => None,
        1 => Some(get_color(reader)?),
        other => return Err(DecodeError::BadTag("underline", other)),
    };
    Ok(CellExtras {
        zerowidth,
        underline_color,
    })
}

// ---------------------------------------------------------------------------
// header
// ---------------------------------------------------------------------------

fn shape_tag(shape: FrameCursorShape) -> u8 {
    match shape {
        FrameCursorShape::Block => 0,
        FrameCursorShape::Underline => 1,
        FrameCursorShape::Beam => 2,
        FrameCursorShape::HollowBlock => 3,
    }
}

fn shape_of(tag: u8) -> Result<FrameCursorShape, DecodeError> {
    match tag {
        0 => Ok(FrameCursorShape::Block),
        1 => Ok(FrameCursorShape::Underline),
        2 => Ok(FrameCursorShape::Beam),
        3 => Ok(FrameCursorShape::HollowBlock),
        other => Err(DecodeError::BadTag("shape", other)),
    }
}

fn modes_bits(modes: TerminalModes) -> u8 {
    (modes.alt_screen as u8)
        | (modes.line_wrap as u8) << 1
        | (modes.insert as u8) << 2
        | (modes.origin as u8) << 3
        | (modes.mouse_reporting as u8) << 4
}

fn modes_of(bits: u8) -> TerminalModes {
    TerminalModes {
        alt_screen: bits & 1 != 0,
        line_wrap: bits & 2 != 0,
        insert: bits & 4 != 0,
        origin: bits & 8 != 0,
        mouse_reporting: bits & 16 != 0,
    }
}

/// Everything but the cell bodies. Shared by both encodings, so a naive-vs-RLE
/// comparison is a comparison of cell bodies and nothing else.
fn put_header(out: &mut Vec<u8>, frame: &TerminalFrame) {
    out.push(VERSION);
    out.push(match frame.kind {
        FrameKind::Full => 0,
        FrameKind::Delta => 1,
    });
    put_uvarint(out, frame.generation);
    put_uvarint(out, frame.cols as u64);
    put_uvarint(out, frame.rows as u64);
    put_ivarint(out, frame.cursor.line as i64);
    put_uvarint(out, frame.cursor.col as u64);
    out.push(
        shape_tag(frame.cursor.shape)
            | (frame.cursor.visible as u8) << 2
            | (frame.cursor.blinking as u8) << 3
            | (frame.focused as u8) << 4,
    );
    out.push(modes_bits(frame.modes));
    put_uvarint(out, frame.color_overrides.len() as u64);
    for over in &frame.color_overrides {
        put_uvarint(out, over.index as u64);
        match over.color {
            Some(rgb) => {
                out.push(1);
                out.extend_from_slice(&[rgb.r, rgb.g, rgb.b]);
            }
            None => out.push(0),
        }
    }
    put_uvarint(out, frame.rows_changed.len() as u64);
}

struct Header {
    kind: FrameKind,
    generation: u64,
    cols: u16,
    rows: u16,
    cursor: FrameCursor,
    focused: bool,
    modes: TerminalModes,
    color_overrides: Vec<ColorOverride>,
    row_count: usize,
}

fn get_header(reader: &mut Reader<'_>) -> Result<Header, DecodeError> {
    let version = reader.u8()?;
    if version != VERSION {
        return Err(DecodeError::BadVersion(version));
    }
    let kind = match reader.u8()? {
        0 => FrameKind::Full,
        1 => FrameKind::Delta,
        other => return Err(DecodeError::BadTag("kind", other)),
    };
    let generation = reader.uvarint()?;
    let cols = reader.uvarint()? as u16;
    let rows = reader.uvarint()? as u16;
    let line = reader.ivarint()? as i32;
    let col = reader.uvarint()? as u16;
    let bits = reader.u8()?;
    let cursor = FrameCursor {
        line,
        col,
        shape: shape_of(bits & 0b11)?,
        visible: bits & 0b100 != 0,
        blinking: bits & 0b1000 != 0,
    };
    let focused = bits & 0b1_0000 != 0;
    let modes = modes_of(reader.u8()?);
    let override_count = reader.uvarint()? as usize;
    let mut color_overrides = Vec::with_capacity(override_count.min(crate::frame::PALETTE_LEN));
    for _ in 0..override_count {
        let index = reader.uvarint()? as u16;
        let color = match reader.u8()? {
            0 => None,
            1 => {
                let rgb = reader.take(3)?;
                Some(Rgb::new(rgb[0], rgb[1], rgb[2]))
            }
            other => return Err(DecodeError::BadTag("override", other)),
        };
        color_overrides.push(ColorOverride { index, color });
    }
    let row_count = reader.uvarint()? as usize;
    Ok(Header {
        kind,
        generation,
        cols,
        rows,
        cursor,
        focused,
        modes,
        color_overrides,
        row_count,
    })
}

fn header_to_frame(header: Header, rows_changed: Vec<RowUpdate>) -> TerminalFrame {
    TerminalFrame {
        generation: header.generation,
        kind: header.kind,
        cols: header.cols,
        rows: header.rows,
        cursor: header.cursor,
        focused: header.focused,
        modes: header.modes,
        color_overrides: header.color_overrides,
        rows_changed,
    }
}

fn put_row_head(out: &mut Vec<u8>, row: &RowUpdate) {
    put_ivarint(out, row.line as i64);
    put_uvarint(out, row.left as u64);
    put_uvarint(out, row.right as u64);
}

fn get_row_head(reader: &mut Reader<'_>) -> Result<(i32, u16, u16, usize), DecodeError> {
    let line = reader.ivarint()? as i32;
    let left = reader.uvarint()? as u16;
    let right = reader.uvarint()? as u16;
    if right < left {
        return Err(DecodeError::BadTag("span", 0));
    }
    Ok((line, left, right, right as usize - left as usize + 1))
}

// ---------------------------------------------------------------------------
// (i) naive: one fixed record per cell
// ---------------------------------------------------------------------------

/// Fixed 15-byte record per cell: `char`(4) `fg`(4) `bg`(4) `flags`(2)
/// `has_extra`(1), plus the extras payload when present.
pub fn encode_naive(frame: &TerminalFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + frame.cell_count() * NAIVE_CELL_BYTES);
    put_header(&mut out, frame);
    for row in &frame.rows_changed {
        put_row_head(&mut out, row);
        for cell in &row.cells {
            out.extend_from_slice(&(cell.c as u32).to_le_bytes());
            put_color_fixed(&mut out, cell.fg);
            put_color_fixed(&mut out, cell.bg);
            out.extend_from_slice(&cell.flags.bits().to_le_bytes());
            match &cell.extra {
                Some(extras) => {
                    out.push(1);
                    put_extras(&mut out, extras);
                }
                None => out.push(0),
            }
        }
    }
    out
}

pub fn decode_naive(bytes: &[u8]) -> Result<TerminalFrame, DecodeError> {
    let mut reader = Reader::new(bytes);
    let header = get_header(&mut reader)?;
    let mut rows_changed = Vec::with_capacity(header.row_count.min(4096));
    for _ in 0..header.row_count {
        let (line, left, right, width) = get_row_head(&mut reader)?;
        let mut cells = Vec::with_capacity(width.min(4096));
        for _ in 0..width {
            let raw = u32::from_le_bytes(reader.take(4)?.try_into().unwrap());
            let c = char::from_u32(raw).ok_or(DecodeError::BadChar(raw))?;
            let fg = get_color_fixed(&mut reader)?;
            let bg = get_color_fixed(&mut reader)?;
            let flags = CellFlags::from_bits_truncate(u16::from_le_bytes(reader.take(2)?.try_into().unwrap()));
            let extra = match reader.u8()? {
                0 => None,
                1 => Some(Box::new(get_extras(&mut reader)?)),
                other => return Err(DecodeError::BadTag("extra", other)),
            };
            cells.push(FrameCell {
                c,
                fg,
                bg,
                flags,
                extra,
            });
        }
        rows_changed.push(RowUpdate {
            line,
            left,
            right,
            cells,
        });
    }
    reader.done()?;
    Ok(header_to_frame(header, rows_changed))
}

// ---------------------------------------------------------------------------
// (ii) RLE keyed on SGR attributes
// ---------------------------------------------------------------------------

/// The run key: everything a single SGR state sets.
fn same_style(a: &FrameCell, b: &FrameCell) -> bool {
    a.fg == b.fg && a.bg == b.bg && a.flags == b.flags
}

/// Runs keyed on `(fg, bg, flags)`, with two collapses:
///
/// * **style runs** -- consecutive cells sharing `(fg, bg, flags)` are one
///   style run and pay for the style once; segments inside it set a "style
///   repeats" bit and pay nothing at all;
/// * **uniform segments** -- a stretch of [`UNIFORM_MIN_RUN`] or more identical
///   characters inside a style run is encoded as one character plus a length.
///   This is what collapses the blank tail of every line, which is most of a
///   terminal screen.
///
/// Cells carrying `extra` (combining marks, underline colour) break neither:
/// extras are appended once per row as `(offset, payload)` pairs. Paying a
/// per-cell presence bit for something this rare -- which is what the naive
/// encoding does -- is a large part of why the naive encoding is big.
pub fn encode_rle(frame: &TerminalFrame) -> Vec<u8> {
    encode_rle_with_threshold(frame, UNIFORM_MIN_RUN)
}

/// [`encode_rle`] with the uniform-segment threshold as a parameter.
///
/// THE MEASURING INSTRUMENT, not a second format: at `threshold ==
/// UNIFORM_MIN_RUN` it is byte-identical to [`encode_rle`], and
/// [`decode_rle`] decodes every threshold, because the threshold only decides
/// where the encoder splits -- the wire format records the split.
///
/// It exists because `UNIFORM_MIN_RUN` has to be chosen, and the honest way to
/// choose it is to encode a real corpus at every candidate and take the
/// smallest. `tests/encode.rs::the_uniform_threshold_is_the_measured_minimum`
/// does exactly that and asserts the constant is the argmin.
pub fn encode_rle_with_threshold(frame: &TerminalFrame, threshold: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + frame.cell_count());
    put_header(&mut out, frame);
    for row in &frame.rows_changed {
        put_row_head(&mut out, row);

        let segments = segment(&row.cells, threshold.max(1));
        put_uvarint(&mut out, segments.len() as u64);
        let mut previous_style: Option<&FrameCell> = None;
        for Segment { from, to, uniform } in segments {
            let head = &row.cells[from];
            put_uvarint(&mut out, (to - from) as u64);
            let repeats = previous_style.map(|p| same_style(p, head)).unwrap_or(false);
            out.push((repeats as u8) | (uniform as u8) << 1);
            if !repeats {
                put_color(&mut out, head.fg);
                put_color(&mut out, head.bg);
                put_uvarint(&mut out, head.flags.bits() as u64);
            }
            if uniform {
                put_uvarint(&mut out, head.c as u64);
            } else {
                for cell in &row.cells[from..to] {
                    put_uvarint(&mut out, cell.c as u64);
                }
            }
            previous_style = Some(head);
        }

        let extras: Vec<(usize, &CellExtras)> = row
            .cells
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| cell.extra.as_ref().map(|e| (index, e.as_ref())))
            .collect();
        put_uvarint(&mut out, extras.len() as u64);
        for (index, payload) in extras {
            put_uvarint(&mut out, index as u64);
            put_extras(&mut out, payload);
        }
    }
    out
}

/// Shortest stretch of identical characters worth collapsing into one
/// `(length, char)` pair.
///
/// CHOSEN BY MEASUREMENT, not by arithmetic. The break-even point is not a
/// constant: for a run at the END of a row (the common case -- every line's
/// blank tail) collapsing wins from about 3, because nothing follows that has
/// to open a new segment; for a run SANDWICHED between two literals it costs an
/// extra segment header and does not win until about 6.
/// `tests/encode.rs::the_uniform_threshold_is_the_measured_minimum` encodes a
/// 2 180-frame corpus at every threshold from 1 to 12 and asserts this constant
/// is the one that produces the fewest bytes. Measured:
///
/// ```text
/// threshold  1       2       3       4       5       6       8      12
/// bytes    433241  430919  429272  430684  431722  433402  435919  444732
/// ```
///
/// The curve is shallow -- 3.6% between best and worst -- so this is a
/// tie-break, not a load-bearing choice. It is measured anyway because the
/// obvious hand-derivation ("a uniform segment costs 3 bytes, so collapse from
/// 4") picks the wrong value.
pub const UNIFORM_MIN_RUN: usize = 3;

struct Segment {
    from: usize,
    to: usize,
    uniform: bool,
}

/// Split a row into style runs, then into uniform / literal segments.
fn segment(cells: &[FrameCell], threshold: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < cells.len() {
        let mut style_end = at + 1;
        while style_end < cells.len() && same_style(&cells[style_end], &cells[at]) {
            style_end += 1;
        }
        let mut cursor = at;
        let mut literal: Option<usize> = None;
        while cursor < style_end {
            let mut end = cursor + 1;
            while end < style_end && cells[end].c == cells[cursor].c {
                end += 1;
            }
            if end - cursor >= threshold {
                if let Some(from) = literal.take() {
                    out.push(Segment {
                        from,
                        to: cursor,
                        uniform: false,
                    });
                }
                out.push(Segment {
                    from: cursor,
                    to: end,
                    uniform: true,
                });
            } else if literal.is_none() {
                literal = Some(cursor);
            }
            cursor = end;
        }
        if let Some(from) = literal.take() {
            out.push(Segment {
                from,
                to: style_end,
                uniform: false,
            });
        }
        at = style_end;
    }
    out
}

pub fn decode_rle(bytes: &[u8]) -> Result<TerminalFrame, DecodeError> {
    let mut reader = Reader::new(bytes);
    let header = get_header(&mut reader)?;
    let mut rows_changed = Vec::with_capacity(header.row_count.min(4096));
    for _ in 0..header.row_count {
        let (line, left, right, width) = get_row_head(&mut reader)?;
        let mut cells: Vec<FrameCell> = Vec::with_capacity(width.min(4096));
        let run_count = reader.uvarint()? as usize;
        let mut style: Option<(FrameColor, FrameColor, CellFlags)> = None;
        for _ in 0..run_count {
            let len = reader.uvarint()? as usize;
            let bits = reader.u8()?;
            let repeats = bits & 1 != 0;
            let uniform = bits & 2 != 0;
            if !repeats {
                let fg = get_color(&mut reader)?;
                let bg = get_color(&mut reader)?;
                let flags = CellFlags::from_bits_truncate(reader.uvarint()? as u16);
                style = Some((fg, bg, flags));
            }
            let (fg, bg, flags) = style.ok_or(DecodeError::BadTag("style", bits))?;
            if uniform {
                let c = reader.char()?;
                for _ in 0..len {
                    cells.push(FrameCell {
                        c,
                        fg,
                        bg,
                        flags,
                        extra: None,
                    });
                }
            } else {
                for _ in 0..len {
                    let c = reader.char()?;
                    cells.push(FrameCell {
                        c,
                        fg,
                        bg,
                        flags,
                        extra: None,
                    });
                }
            }
        }
        let extra_count = reader.uvarint()? as usize;
        for _ in 0..extra_count {
            let index = reader.uvarint()? as usize;
            let payload = get_extras(&mut reader)?;
            let cell = cells.get_mut(index).ok_or(DecodeError::Truncated)?;
            cell.extra = Some(Box::new(payload));
        }
        if cells.len() != width {
            return Err(DecodeError::Truncated);
        }
        rows_changed.push(RowUpdate {
            line,
            left,
            right,
            cells,
        });
    }
    reader.done()?;
    Ok(header_to_frame(header, rows_changed))
}

/// Encode with `encoding`.
pub fn encode(frame: &TerminalFrame, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Naive => encode_naive(frame),
        Encoding::Rle => encode_rle(frame),
    }
}

/// Decode with `encoding`.
pub fn decode(bytes: &[u8], encoding: Encoding) -> Result<TerminalFrame, DecodeError> {
    match encoding {
        Encoding::Naive => decode_naive(bytes),
        Encoding::Rle => decode_rle(bytes),
    }
}

/// The header cost alone, so naive-vs-RLE can be reported as a body comparison.
pub fn header_len(frame: &TerminalFrame) -> usize {
    let mut out = Vec::new();
    put_header(&mut out, frame);
    out.len()
}
