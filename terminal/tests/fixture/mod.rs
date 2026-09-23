//! Parses Verdandi's golden op dump back into `PaintList`s.
//!
//! The dump is a readable one-line-per-op rendering of the exact ops Verdandi's builder produced for
//! each scenario. Parsing it here rather than depending on the builder keeps this crate's tests free
//! of `terminal-frame`, `alacritty_terminal` and any `Term` -- the same ignorance property the
//! backend itself has, applied to its tests.
//!
//! A parse failure panics loudly rather than skipping a line. A fixture parser that quietly ignored
//! what it did not understand would shrink the corpus as the corpus grew.

#![allow(dead_code)]

use terminal_render::{CursorShape, CursorText, GlyphStyle, PaintList, PaintOp, RgbColor, UnderlineKind};

pub struct Scenario {
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub list: PaintList,
}

impl Scenario {
    /// Swaps the cursor's shape, for differential tests (does a hollow cursor cover less than a
    /// block one?).
    pub fn set_cursor_shape(&mut self, new_shape: CursorShape) {
        for op in &mut self.list.ops {
            if let PaintOp::DrawCursor { shape, .. } = op {
                *shape = new_shape;
                return;
            }
        }
        panic!("no DrawCursor to reshape");
    }

    /// Swaps one cell's text, for differential tests (does 'e' render differently from 'é'?).
    pub fn replace_text(&mut self, row: u16, col: u16, new_text: &str) {
        for op in &mut self.list.ops {
            if let PaintOp::DrawText {
                row: r, col: c, text, ..
            } = op
            {
                if *r == row && *c == col {
                    *text = new_text.to_string();
                    return;
                }
            }
        }
        panic!("no DrawText at r{row}c{col} to replace");
    }
}

pub fn load_scenarios() -> Vec<Scenario> {
    parse(include_str!("../../../terminal-render/tests/golden/paintops.txt"))
}

fn hex(s: &str) -> RgbColor {
    let v = u32::from_str_radix(s.trim(), 16).unwrap_or_else(|_| panic!("bad colour {s:?}"));
    RgbColor::new((v >> 16) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8)
}

/// Unescapes the dump's `\u{...}` form. Combining marks appear this way precisely because they are
/// invisible in a text file, which is also why a test that dropped them would be hard to notice.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => {
                assert_eq!(chars.next(), Some('{'), "malformed \\u escape");
                let mut digits = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    digits.push(c);
                }
                let code = u32::from_str_radix(&digits, 16).expect("bad \\u escape");
                out.push(char::from_u32(code).expect("bad scalar"));
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Pulls the first `"..."` out of a line, returning it unescaped plus the remainder after it.
fn quoted(line: &str) -> (String, &str) {
    let start = line.find('"').expect("expected a quoted string");
    let rest = &line[start + 1..];
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'"' && (end == 0 || bytes[end - 1] != b'\\') {
            break;
        }
        end += 1;
    }
    (unescape(&rest[..end]), &rest[end + 1..])
}

/// `r3` -> 3, `c12` -> 12, `c0..8` -> (0, 8), `c0+2` -> (0, 2)
fn number(token: &str) -> u16 {
    token
        .trim_start_matches(['r', 'c'])
        .parse()
        .unwrap_or_else(|_| panic!("bad number in {token:?}"))
}

fn parse(text: &str) -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    let mut current: Option<Scenario> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix("=== ").and_then(|l| l.strip_suffix(" ===")) {
            if let Some(scenario) = current.take() {
                scenarios.push(scenario);
            }
            current = Some(Scenario {
                name: name.to_string(),
                cols: 0,
                rows: 0,
                list: PaintList {
                    ops: Vec::new(),
                    cols: 0,
                    rows: 0,
                    surface_background: RgbColor::default(),
                    top_line: 0,
                },
            });
            continue;
        }
        let scenario = current.as_mut().expect("an op line before any scenario header");
        let mut tokens = line.split_whitespace();
        let kind = tokens.next().expect("a non-empty line");

        match kind {
            "surface" => {
                // surface 12x2 bg=181818 top_line=0
                let dims = tokens.next().expect("dimensions");
                let (cols, rows) = dims.split_once('x').expect("WxH");
                scenario.cols = cols.parse().expect("cols");
                scenario.rows = rows.parse().expect("rows");
                scenario.list.cols = scenario.cols;
                scenario.list.rows = scenario.rows;
                for token in tokens {
                    if let Some(v) = token.strip_prefix("bg=") {
                        scenario.list.surface_background = hex(v);
                    } else if let Some(v) = token.strip_prefix("top_line=") {
                        scenario.list.top_line = v.parse().expect("top_line");
                    }
                }
            }
            "fill" => {
                // fill   r0 c0..12 181818
                let row = number(tokens.next().expect("row"));
                let span = tokens.next().expect("span");
                let (from, to) = span.trim_start_matches('c').split_once("..").expect("a..b");
                let col: u16 = from.parse().expect("from");
                let end: u16 = to.parse().expect("to");
                let color = hex(tokens.next().expect("colour"));
                scenario.list.ops.push(PaintOp::FillCells {
                    row,
                    col,
                    cols: end - col,
                    color,
                });
            }
            "text" => {
                // text   r0 c0+2 "漢" fg=d8d8d8 bold strike ulc=d8d8d8 ul=u
                let row = number(tokens.next().expect("row"));
                let span = tokens.next().expect("span");
                let (col, cols) = span.trim_start_matches('c').split_once('+').expect("c+n");
                let col: u16 = col.parse().expect("col");
                let cols: u16 = cols.parse().expect("cols");
                let (text, rest) = quoted(line);
                let (color, style) = parse_style(rest);
                scenario.list.ops.push(PaintOp::DrawText {
                    row,
                    col,
                    cols,
                    text,
                    color,
                    style,
                });
            }
            "cursor" => {
                // cursor r0 c0+1 Block d8d8d8 blink under="X"@181818
                let row = number(tokens.next().expect("row"));
                let span = tokens.next().expect("span");
                let (col, cols) = span.trim_start_matches('c').split_once('+').expect("c+n");
                let shape = match tokens.next().expect("shape") {
                    "Block" => CursorShape::Block,
                    "Underline" => CursorShape::Underline,
                    "Beam" => CursorShape::Beam,
                    "HollowBlock" => CursorShape::HollowBlock,
                    other => panic!("unknown cursor shape {other:?}"),
                };
                let color = hex(tokens.next().expect("colour"));
                let blinking = line.contains(" blink ");
                let text_under = if line.contains("under=-") {
                    None
                } else {
                    let after = line.split("under=").nth(1).expect("under=");
                    let (text, rest) = quoted(after);
                    let color = hex(rest.trim_start_matches('@').trim());
                    Some(CursorText {
                        text,
                        color,
                        style: GlyphStyle::default(),
                    })
                };
                scenario.list.ops.push(PaintOp::DrawCursor {
                    row,
                    col: col.parse().expect("col"),
                    cols: cols.parse().expect("cols"),
                    shape,
                    color,
                    text_under,
                    blinking,
                });
            }
            "notice" => {
                // notice r0 c0+8 " ... " fg=181818 bg=d8d8d8
                let row = number(tokens.next().expect("row"));
                let span = tokens.next().expect("span");
                let (col, cols) = span.trim_start_matches('c').split_once('+').expect("c+n");
                let (text, rest) = quoted(line);
                let mut color = RgbColor::default();
                let mut background = RgbColor::default();
                for token in rest.split_whitespace() {
                    if let Some(v) = token.strip_prefix("fg=") {
                        color = hex(v);
                    } else if let Some(v) = token.strip_prefix("bg=") {
                        background = hex(v);
                    }
                }
                scenario.list.ops.push(PaintOp::DrawNotice {
                    row,
                    col: col.parse().expect("col"),
                    cols: cols.parse().expect("cols"),
                    text,
                    color,
                    background,
                });
            }
            other => panic!("unknown op line kind {other:?} in: {line}"),
        }
    }
    if let Some(scenario) = current.take() {
        scenarios.push(scenario);
    }
    scenarios
}

fn parse_style(rest: &str) -> (RgbColor, GlyphStyle) {
    let mut color = RgbColor::default();
    let mut style = GlyphStyle::default();
    let mut underline_color: Option<RgbColor> = None;
    for token in rest.split_whitespace() {
        match token {
            "bold" => style.bold = true,
            "italic" => style.italic = true,
            "strike" => style.strikeout = true,
            // `dim` is a resolved COLOUR upstream, not a flag the backend acts on. It appears in the
            // dump as a readability aid; treating it as a style here would re-derive something
            // Verdandi already decided.
            "dim" => {}
            _ => {
                if let Some(v) = token.strip_prefix("fg=") {
                    color = hex(v);
                } else if let Some(v) = token.strip_prefix("ulc=") {
                    underline_color = Some(hex(v));
                } else if let Some(v) = token.strip_prefix("ul=") {
                    style.underline = match v {
                        "-" => UnderlineKind::None,
                        "u" => UnderlineKind::Single,
                        "d" => UnderlineKind::Double,
                        "c" => UnderlineKind::Curl,
                        "." => UnderlineKind::Dotted,
                        "-." => UnderlineKind::Dashed,
                        other => panic!("unknown underline kind {other:?}"),
                    };
                }
            }
        }
    }
    // Always concrete: the glyph's own colour when the terminal specified none. The dump omits `ulc`
    // in that case, so reconstructing it here matches what the builder actually emits.
    style.underline_color = underline_color.unwrap_or(color);
    (color, style)
}
