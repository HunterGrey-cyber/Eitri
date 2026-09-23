//! A real `Term`, a real parser, and the projection the renderer consumes.
//! No Skia, no window: the whole cell contract is testable as data.
#![allow(dead_code)]

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use terminal_frame::project_window;
use terminal_render::{
    build_paint_list, palette_for, CursorColoring, PaintList, PaintOp, RenderInput, RgbColor, SelectionSpan, ViewMode,
};

#[derive(Clone, Copy, Debug)]
pub struct Size {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Screen {
    pub term: Term<VoidListener>,
    pub parser: Processor,
    pub size: Size,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_history(cols, rows, 1000)
    }

    pub fn with_history(cols: usize, rows: usize, history: usize) -> Self {
        let size = Size { cols, rows };
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, VoidListener),
            parser: Processor::new(),
            size,
        }
    }

    pub fn feed(&mut self, text: &str) {
        self.parser.advance(&mut self.term, text.as_bytes());
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.size = Size { cols, rows };
        self.term.resize(self.size);
    }

    /// The live screen, focused. `top_line` is 0, so window row N is absolute
    /// line N and the tests read naturally.
    pub fn paint(&self) -> PaintList {
        self.paint_window(0, self.term.screen_lines(), ViewMode::FollowBottom, &[])
    }

    pub fn paint_selected(&self, selection: &[SelectionSpan]) -> PaintList {
        self.paint_window(0, self.term.screen_lines(), ViewMode::FollowBottom, selection)
    }

    pub fn paint_window(&self, top_line: i32, rows: usize, mode: ViewMode, selection: &[SelectionSpan]) -> PaintList {
        self.paint_full(top_line, rows, mode, selection, true)
    }

    pub fn paint_full(
        &self,
        top_line: i32,
        rows: usize,
        mode: ViewMode,
        selection: &[SelectionSpan],
        focused: bool,
    ) -> PaintList {
        self.paint_cursor(top_line, rows, mode, selection, focused, CursorColoring::Palette)
    }

    /// The live screen, focused, with the cursor coloured by `cursor_color`.
    pub fn paint_colored(&self, cursor_color: CursorColoring) -> PaintList {
        self.paint_cursor(
            0,
            self.term.screen_lines(),
            ViewMode::FollowBottom,
            &[],
            true,
            cursor_color,
        )
    }

    pub fn paint_cursor(
        &self,
        top_line: i32,
        rows: usize,
        mode: ViewMode,
        selection: &[SelectionSpan],
        focused: bool,
        cursor_color: CursorColoring,
    ) -> PaintList {
        let frame = project_window(&self.term, top_line, rows);
        let palette = palette_for(&frame);
        build_paint_list(&RenderInput {
            frame: &frame,
            window_top_line: top_line,
            mode,
            selection,
            focused,
            palette: &palette,
            cursor_color,
        })
    }
}

/// Glyphs on one WINDOW ROW as `(col, cols, text)`, column order.
pub fn glyphs_on(list: &PaintList, row: u16) -> Vec<(u16, u16, String)> {
    let mut v: Vec<_> = list
        .ops
        .iter()
        .filter_map(|op| match op {
            PaintOp::DrawText {
                row: r,
                col,
                cols,
                text,
                ..
            } if *r == row => Some((*col, *cols, text.clone())),
            _ => None,
        })
        .collect();
    v.sort_by_key(|(c, _, _)| *c);
    v
}

pub fn glyph_fg(list: &PaintList, row: u16, col: u16) -> Option<RgbColor> {
    list.ops.iter().find_map(|op| match op {
        PaintOp::DrawText {
            row: r, col: c, color, ..
        } if *r == row && *c == col => Some(*color),
        _ => None,
    })
}

pub fn bg_at(list: &PaintList, row: u16, col: u16) -> Option<RgbColor> {
    list.ops.iter().find_map(|op| match op {
        PaintOp::FillCells {
            row: r, col: c, color, ..
        } if *r == row && *c == col => Some(*color),
        _ => None,
    })
}
