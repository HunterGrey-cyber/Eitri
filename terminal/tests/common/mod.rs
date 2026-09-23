//! Shared by the engine's integration tests: a session driven from a test thread, and a CPU raster.
//! No GTK and no window anywhere: `surfaces::raster_n32_premul` is a real Skia canvas in memory.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use neovibe_terminal::{
    paint, ExitInfo, HostEvents, PtySize, SessionCommand, SessionConfig, SpawnSpec, TerminalColors, TerminalMetrics,
    TerminalSession,
};
use skia_safe::{surfaces, EncodedImageFormat, ISize, Surface};
use terminal_input::keys::{Key, KeyEvent, ModifiersState, NamedKey};
use terminal_input::NormalizedInput;
use terminal_render::{PaintList, PaintOp};

pub const WAIT: Duration = Duration::from_secs(5);

pub fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols,
        rows,
        cell_width_px: 9,
        cell_height_px: 18,
    }
}

pub fn spec(program: &str, args: &[&str]) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from(program),
        args: args.iter().map(OsString::from).collect(),
        cwd: std::env::temp_dir(),
    }
}

/// `sh -i` with a fixed prompt and a minimal environment, for determinism. The product never
/// clears the environment; tests that are about the environment use `spec` directly.
pub fn plain_sh() -> SpawnSpec {
    spec(
        "/usr/bin/env",
        &[
            "-i",
            "PS1=$ ",
            "TERM=xterm-256color",
            "HOME=/tmp",
            "PATH=/usr/bin:/bin",
            "/bin/sh",
            "-i",
        ],
    )
}

/// A session plus everything it has reported so far.
pub struct Harness {
    pub session: TerminalSession,
    woken: mpsc::Receiver<()>,
    pub frame: Option<PaintList>,
    pub events: HostEvents,
    pub exited: Option<ExitInfo>,
    pub wakes: usize,
}

impl Harness {
    pub fn start(spec: SpawnSpec, size: PtySize) -> Self {
        Self::start_with(SessionConfig {
            spawn: spec,
            size,
            colors: TerminalColors::default(),
            focused: true,
            tap: None,
        })
    }

    pub fn start_with(config: SessionConfig) -> Self {
        let (tx, woken) = mpsc::channel();
        let session = TerminalSession::spawn(config, move || {
            let _ = tx.send(());
        })
        .expect("spawn");
        Harness {
            session,
            woken,
            frame: None,
            events: HostEvents::default(),
            exited: None,
            wakes: 0,
        }
    }

    /// Takes whatever the session published.
    fn absorb(&mut self) {
        let update = self.session.take_update();
        if let Some(frame) = update.frame {
            self.frame = Some(frame);
        }
        if update.events.title.is_some() {
            self.events.title = update.events.title;
        }
        self.events.bell |= update.events.bell;
        if update.events.clipboard.is_some() {
            self.events.clipboard = update.events.clipboard;
        }
        if update.exited.is_some() {
            self.exited = update.exited;
        }
    }

    /// Waits for wake-ups until `pred` holds, or panics with the screen after `timeout`.
    pub fn wait_for(&mut self, timeout: Duration, pred: impl Fn(&Harness) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            self.absorb();
            if pred(self) {
                return;
            }
            let now = Instant::now();
            assert!(now < deadline, "timed out; screen:\n{}", self.text().join("\n"));
            if self.woken.recv_timeout(deadline - now).is_ok() {
                self.wakes += 1;
            }
        }
    }

    /// Wake-ups the session has rung since the last time this (or [`Harness::wait_for`]) drained
    /// the channel, counted without blocking. `wakes` only grows inside `wait_for`'s own
    /// `recv_timeout`, so a caller that wants a real host-wake measurement over a span with no
    /// `wait_for` call in it (fix round 1, review finding 2) needs this instead.
    pub fn pending_wakes(&self) -> usize {
        self.woken.try_iter().count()
    }

    pub fn text(&self) -> Vec<String> {
        self.frame.as_ref().map(screen_text).unwrap_or_default()
    }

    pub fn has_line(&self, line: &str) -> bool {
        self.text().iter().any(|l| l == line)
    }

    pub fn send(&self, input: NormalizedInput) {
        self.session.send(SessionCommand::Input(input));
    }

    /// Types `s` one key at a time, `\n` as Enter.
    pub fn type_str(&self, s: &str) {
        for ch in s.chars() {
            let key = if ch == '\n' {
                Key::Named(NamedKey::Enter)
            } else {
                Key::Character(ch.to_string())
            };
            self.send(NormalizedInput::Key {
                event: KeyEvent::press(key),
                mods: ModifiersState::empty(),
            });
        }
    }
}

/// Ctrl+`letter`, as `shell`'s GDK normalisation produces it: the control code as the text.
pub fn ctrl(letter: char) -> NormalizedInput {
    let code = char::from_u32(letter as u32 & 0x1f).expect("a control code");
    NormalizedInput::Key {
        event: KeyEvent::press(Key::Character(letter.to_string())).with_text(Some(&code.to_string())),
        mods: ModifiersState::CONTROL,
    }
}

/// Each row as it appears, with `DrawText` runs placed at their columns. `PaintList::row_text`
/// concatenates runs and drops the blanks between them (`$ echo hi` reads `$echohi`).
pub fn screen_text(list: &PaintList) -> Vec<String> {
    let mut rows = vec![vec![' '; list.cols as usize]; list.rows as usize];
    for op in &list.ops {
        if let PaintOp::DrawText { row, col, text, .. } = op {
            for (i, ch) in text.chars().enumerate() {
                if let Some(cell) = rows.get_mut(*row as usize).and_then(|r| r.get_mut(*col as usize + i)) {
                    *cell = ch;
                }
            }
        }
    }
    rows.into_iter()
        .map(|r| r.into_iter().collect::<String>().trim_end().to_string())
        .collect()
}

pub fn metrics() -> TerminalMetrics {
    TerminalMetrics::with_font("monospace", 15.0, 4000.0, 4000.0, 1.0)
}

/// A frame painted on a CPU raster by the product's own `paint`.
pub struct Raster {
    pub pixels: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

fn painted(list: &PaintList, metrics: &TerminalMetrics) -> Surface {
    let width = ((list.cols as f32 * metrics.cell_width()).ceil() as i32).max(1);
    let height = ((list.rows as f32 * metrics.cell_height()).ceil() as i32).max(1);
    let mut surface = surfaces::raster_n32_premul(ISize::new(width, height)).expect("raster surface");
    paint(surface.canvas(), list, metrics);
    surface
}

/// The frame as a PNG, for a person to look at.
pub fn png(list: &PaintList, metrics: &TerminalMetrics) -> Vec<u8> {
    #[allow(deprecated)]
    let data = painted(list, metrics)
        .image_snapshot()
        .encode_to_data(EncodedImageFormat::PNG)
        .expect("png");
    data.as_bytes().to_vec()
}

impl Raster {
    pub fn of(list: &PaintList, metrics: &TerminalMetrics) -> Self {
        let mut surface = painted(list, metrics);
        let (width, height) = (surface.width(), surface.height());
        let info = surface.image_info();
        let row_bytes = info.min_row_bytes();
        let mut pixels = vec![0u8; row_bytes * height as usize];
        assert!(surface.read_pixels(&info, &mut pixels, row_bytes, (0, 0)));
        Raster { pixels, width, height }
    }

    /// `(r, g, b)` at a pixel. n32 premul is BGRA here, and every pixel is opaque.
    pub fn at(&self, x: i32, y: i32) -> (u8, u8, u8) {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        (self.pixels[i + 2], self.pixels[i + 1], self.pixels[i])
    }

    /// Pixels in cells `col..col+cols` of `row` that are not `bg`.
    pub fn ink(&self, metrics: &TerminalMetrics, row: u16, col: u16, cols: u16, bg: (u8, u8, u8)) -> usize {
        let rect = metrics.cell_rect(row, col, cols);
        let mut n = 0;
        for y in (rect.top as i32).max(0)..(rect.bottom as i32).min(self.height) {
            for x in (rect.left as i32).max(0)..(rect.right as i32).min(self.width) {
                if self.at(x, y) != bg {
                    n += 1;
                }
            }
        }
        n
    }
}
