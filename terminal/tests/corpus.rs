//! Byte streams replayed into a [`Screen`] and painted on a CPU raster: the headless half of the
//! "daily corpus" check (spec, risks: "his everyday programs misbehave, because this is not foot").
//! The foot comparison of the same streams is the GUI pass's half.
//!
//! Two parts:
//! - **synthetic streams**, written here, which run in every `cargo test`: each is a behaviour a
//!   daily program depends on, with an exact assertion;
//! - **the owner's recorded streams** (`examples/record.rs`), `#[ignore]`d because they live outside
//!   git: `EITRI_TERMINAL_CORPUS=<dir> cargo test -p eitri-terminal --test corpus -- --ignored
//!   --nocapture`. Each replay writes `<name>.png` and `<name>.txt` beside its `.bytes` and reports
//!   what vte could not handle -- its own `[unhandled ...]` debug records, counted here.

mod common;

use std::sync::Mutex;

use common::{metrics, screen_text, Raster};
use eitri_terminal::{PtySize, Screen, TerminalColors};
use terminal_render::{PaintList, PaintOp, RgbColor};

/// Every `log` record vte or alacritty_terminal emits about a sequence they did not handle.
struct Unhandled(Mutex<Vec<String>>);

impl log::Log for Unhandled {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let message = record.args().to_string();
        if message.to_ascii_lowercase().contains("unhandled") {
            self.0.lock().unwrap().push(message);
        }
    }

    fn flush(&self) {}
}

static UNHANDLED: Unhandled = Unhandled(Mutex::new(Vec::new()));

fn listen_for_unhandled() {
    let _ = log::set_logger(&UNHANDLED);
    log::set_max_level(log::LevelFilter::Debug);
}

fn take_unhandled() -> Vec<String> {
    std::mem::take(&mut *UNHANDLED.0.lock().unwrap())
}

fn replay(bytes: &[u8], cols: u16, rows: u16) -> (Screen, PaintList) {
    let mut screen = Screen::new(
        PtySize {
            cols,
            rows,
            cell_width_px: 9,
            cell_height_px: 18,
        },
        TerminalColors::default(),
    );
    // In read-sized chunks, as the session feeds it.
    for chunk in bytes.chunks(4096) {
        screen.feed(chunk);
    }
    let list = screen.render(true);
    (screen, list)
}

fn text_color(list: &PaintList, row: u16, col: u16) -> Option<RgbColor> {
    list.ops.iter().find_map(|op| match op {
        PaintOp::DrawText {
            row: r, col: c, color, ..
        } if (*r, *c) == (row, col) => Some(*color),
        _ => None,
    })
}

/// One test, not one per stream: the logger is process-global, and parallel tests would mix their
/// records.
#[test]
fn synthetic_streams_render_as_daily_programs_expect() {
    listen_for_unhandled();
    take_unhandled();

    // SGR: 16-colour, 256-colour and truecolour foregrounds (ls --color, git log --color, bat).
    let (_, list) = replay(
        b"\x1b[31mred\x1b[0m \x1b[38;5;208m256\x1b[0m \x1b[38;2;10;200;30mtrue\x1b[0m",
        40,
        3,
    );
    assert_eq!(screen_text(&list)[0], "red 256 true");
    assert_eq!(text_color(&list, 0, 0), Some(RgbColor::new(0xcd, 0x00, 0x00)));
    assert_eq!(text_color(&list, 0, 4), Some(RgbColor::new(0xff, 0x87, 0x00)));
    assert_eq!(text_color(&list, 0, 8), Some(RgbColor::new(10, 200, 30)));

    // Wide characters take two cells (Chinese commit messages in git log).
    let (_, list) = replay("漢字ok".as_bytes(), 20, 2);
    let wide: Vec<(u16, u16)> = list
        .ops
        .iter()
        .filter_map(|op| match op {
            PaintOp::DrawText { col, cols, text, .. } if text == "漢" || text == "字" => Some((*col, *cols)),
            _ => None,
        })
        .collect();
    assert_eq!(wide, vec![(0, 2), (2, 2)]);
    assert!(
        text_color(&list, 0, 4).is_some(),
        "'o' lands in column 4, after two wide cells"
    );

    // The alternate screen: htop, less and nvim draw there and leave the shell's screen as it was.
    let (_, list) = replay(b"before\x1b[?1049h\x1b[Hinside\x1b[?1049l", 20, 3);
    assert_eq!(screen_text(&list)[0], "before");

    // DEC line drawing (tmux borders, some TUIs).
    let (_, list) = replay(b"\x1b(0lqk\x1b(B", 10, 2);
    assert_eq!(screen_text(&list)[0], "\u{250c}\u{2500}\u{2510}");

    // A synchronized update (DECSET 2026) publishes nothing until it closes.
    let mut screen = Screen::new(
        PtySize {
            cols: 20,
            rows: 3,
            cell_width_px: 9,
            cell_height_px: 18,
        },
        TerminalColors::default(),
    );
    screen.take_dirty();
    screen.feed(b"\x1b[?2026hheld");
    assert!(!screen.take_dirty(), "nothing is published inside an open update");
    screen.feed(b"\x1b[?2026l");
    assert!(screen.take_dirty());
    assert_eq!(screen_text(&screen.render(true))[0], "held");

    // Every glyph above painted real ink.
    let m = metrics();
    let (_, list) = replay(b"\x1b[1mbold\x1b[0m \x1b[4munder\x1b[0m", 20, 2);
    let r = Raster::of(&list, &m);
    let bg = (
        list.surface_background.r,
        list.surface_background.g,
        list.surface_background.b,
    );
    assert!(r.ink(&m, 0, 0, 4, bg) > 0 && r.ink(&m, 0, 5, 5, bg) > 0);

    let unhandled = take_unhandled();
    assert!(unhandled.is_empty(), "vte could not handle: {unhandled:#?}");
}

#[test]
#[ignore = "needs EITRI_TERMINAL_CORPUS=<dir> of recordings; see the module doc"]
fn the_owners_recorded_streams_replay() {
    let dir = std::path::PathBuf::from(std::env::var_os("EITRI_TERMINAL_CORPUS").expect("EITRI_TERMINAL_CORPUS"));
    listen_for_unhandled();
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "bytes"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no *.bytes in {}", dir.display());
    let m = metrics();
    let mut report = Vec::new();
    for path in names {
        // `<name>.<cols>x<rows>.bytes`, as examples/record.rs names them.
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let (name, geometry) = stem.rsplit_once('.').expect("<name>.<cols>x<rows>.bytes");
        let (cols, rows) = geometry.split_once('x').expect("<cols>x<rows>");
        let (cols, rows): (u16, u16) = (cols.parse().unwrap(), rows.parse().unwrap());
        let bytes = std::fs::read(&path).unwrap();
        take_unhandled();
        let started = std::time::Instant::now();
        let (_, list) = replay(&bytes, cols, rows);
        let elapsed = started.elapsed();
        let unhandled = take_unhandled();

        let raster = Raster::of(&list, &m);
        let bg = (
            list.surface_background.r,
            list.surface_background.g,
            list.surface_background.b,
        );
        let ink: usize = (0..rows).map(|row| raster.ink(&m, row, 0, cols, bg)).sum();
        std::fs::write(dir.join(format!("{name}.txt")), screen_text(&list).join("\n") + "\n").unwrap();
        std::fs::write(dir.join(format!("{name}.png")), common::png(&list, &m)).unwrap();
        std::fs::write(dir.join(format!("{name}.unhandled.txt")), unhandled.join("\n") + "\n").unwrap();
        report.push((unhandled.len(), name.to_string(), bytes.len(), elapsed, ink));
    }
    report.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    // A newline first: libtest has already printed "test <name> ... " without one.
    eprintln!(
        "\n{:>9}  {:<24} {:>10} {:>10}",
        "unhandled", "stream", "bytes", "replay"
    );
    for (unhandled, name, bytes, elapsed, _) in &report {
        eprintln!("{unhandled:>9}  {name:<24} {bytes:>10} {elapsed:>10.1?}");
    }
    for (_, name, _, _, ink) in &report {
        assert!(*ink > 0, "{name} replayed to a blank screen");
    }
}
