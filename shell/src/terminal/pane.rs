//! The bottom terminal's widget: a `GtkGLArea` that paints its session's latest frame, and the
//! keyboard, click, focus and resize wiring around it.
//!
//! From the frozen `terminal-pane/src/pane.rs` which had no
//! session at all; what changed is what the frozen pane's callbacks are now connected to. The pane
//! owns no terminal semantics: bytes, parsing, encoding and rendering are `eitri-terminal`'s, on
//! the session thread. What it keeps is the latest `PaintList` and the GL surface to paint it on.
//!
//! **The shell starts lazily**: `start` only says a shell is wanted, and the session is
//! spawned at the first allocation with area, so it never starts at 1x1 and redraws its prompt.
//! That allocation is **kept** ([`State::allocation`]) and the lazily built metrics are built AT it
//! (GUI pass 2026-09-23, defect 1): the first `resize` arrives before any metrics exist, so it has
//! nothing to resize, and GTK sends no second one for the same allocation. Before this, the metrics
//! were built at a 1x1 placeholder and the first `Ctrl+a t` spawned the shell on a 1x1 PTY, blank
//! but for one cursor cell, until something happened to resize the pane.
//!
//! **Woken, never polled.** The session calls a waker from its own thread; the waker sends on a
//! channel whose receiver a `glib::spawn_future_local` future awaits on the main loop. No timer:
//! a 33 ms poll like the agent panel's steady tick would add up to 33ms to every echo, and an idle terminal costs no
//! wake-ups at all (tested in `eitri-terminal`'s `idle_renders_nothing`). Hidden (unmapped), the
//! pane tells the session so, and a program printing away in a hidden terminal renders nothing.
//!
//! **A typed key's echo shows one output frame after the key** (about 18 ms at 60 Hz, 8 ms at 165 Hz,
//! where an emulator that commits the echo directly takes about 1 ms). The pane is a `GtkGLArea`
//! painted in GTK's frame clock. A printable key reaches the shell as an input-method commit, whose
//! handler paints at once, in the key's own frame-clock cycle, before the echo is back. The compositor
//! composites that commit, and the echo's frame, committed 2-3 ms later, waits for its next output
//! frame. Without that paint, GTK 4.24 holds the echo's render until one period after the key's cycle
//! instead (`min_next_frame_time`, a 60 Hz period where the output reports no refresh), which is no
//! sooner and often later. Only presenting outside GTK's frame clock, as the editor's subsurface does,
//! takes the frame away.
//!
//! **A shell that will not start** (a stale `$SHELL`) falls back to the passwd entry's shell and
//! then `/bin/sh`; if none starts, the pane says so in its own frame and waits for Enter, rather
//! than trying again on every resize.
//!
//! **Chinese input (bottom-terminal phase 2)** is `ime.rs`'s: a `GtkIMMulticontext` on this widget's
//! key controller, its commits through [`submit`], its unfinished composition drawn over the frame
//! at the cursor the session reports (`Update::cursor`), and the input method pointed at its caret
//! on every `preedit-*`, when the terminal gains the keys, and when a frame moves the cursor while
//! it has them. **GTK's key controller owns the input method's focus** (`set_im_context`) and
//! focuses it out from inside a focus change, before the focus report says anything -- which is when
//! fcitx5 commits whatever was being composed. So the commit handler asks GTK whether the terminal
//! still has the keys ([`connect_ime`]), and a composition cut off by the keys leaving is dropped,
//! never typed (`ime.rs`'s module doc has the sources). Its client widget is set by
//! [`TerminalPane::start`], so a window whose terminal is never shown creates no input-method client.
//! **No `RefCell` borrow is held across a call into the input method** (`reset`,
//! `set_cursor_location`, `set_client_widget`): `reset` can emit `commit` or `preedit-changed` before
//! it returns, and those handlers borrow the state.
//!
//! **Everything bound for the shell goes through one door, [`submit`]** (bottom-terminal phase 2):
//! typed keys, the prefix's literals, `Ctrl+Shift+V`'s text and input-method commits. A paste reads
//! the clipboard asynchronously, and what is typed while it is read waits behind it (`input_queue`),
//! so an Enter can never overtake the text it was meant to run. A middle click pastes the primary
//! selection through the same door (`paste_primary`).
//!
//! **Selection: drag, double/triple click, and copy.** `connect_pointer` turns the primary button's click count and motion into
//! `eitri_terminal::SelectCommand`s (`pointer::cell_at`/`kind_for`); a release copies the
//! finished text to both the clipboard and the primary selection at once (`pump`'s own
//! `events.selection` handling), and `Ctrl+Shift+C` copies it again from `State::last_selection`
//! (`copy_selection`) -- it is no longer held back the way it was through phase 2.
//!
//! **A program's copy lands on the desktop, and its bell flashes the pane** (bottom-terminal phase
//! 2). An OSC 52 copy goes to GTK's clipboard (`c`) or primary selection (`p`/`s`), hidden or not; a
//! read is still refused, by `Term` itself. Copies show a byte-only notice with this pane's number
//! and target, coalesced for one second. Hidden copies show when mapped. A bell tints the pane for
//! a moment (`bell.rs`), and a bell rung while the terminal was hidden is not flashed when it comes back. The title a program
//! sets has nowhere to go: the terminal's title rule sends it to the pane's header "when one exists", and none does.
//!
//! **The font metrics are lazy too, and never panic.** A
//! window whose terminal is never shown never touches a font manager: `TerminalMetrics` is built
//! by [`ensure_session`], the same gate that lazily spawns the shell, not at [`TerminalPane::new`].
//! A machine with no usable monospace font shows the pane's own failure frame -- the same one a
//! shell that would not start shows -- rather than panicking the whole process.
//!
//! **The face and the text size are the window's** (`super::TerminalFace`, [`TerminalPane::set_face`],
//! [`TerminalPane::set_zoom`]): the editor's `guifont` unless `terminal.font` says otherwise, at the
//! window's terminal zoom, which the controller moves with the editor's `g:neovide_scale_factor` or alone.
//! A change rebuilds the metrics at the pane's size, and the grid that comes back reaches the PTY the
//! way a pane resize does, so a larger font is a smaller grid the shell is told about. Before the
//! metrics exist the face is only kept.

use std::cell::RefCell;
use std::ffi::OsString;
use std::panic;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use gtk4::gdk::{Clipboard, ModifierType, Rectangle};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use gtk4::{
    EventControllerFocus, EventControllerKey, EventControllerScroll, EventControllerScrollFlags, GLArea, GestureClick,
    GestureDrag, IMMulticontext, InputPurpose,
};

use eitri_terminal::mouse::{Button as MouseButton, MouseKind, MouseModes, MouseMods, WheelDir};
use eitri_terminal::pty::{fallback_shell, passwd_shell};
use eitri_terminal::{
    layout_preedit, CursorCell, DetachRefused, Detached, ExitInfo, HostEvents, PreeditLayout, PtySize, Screen,
    ScrollRequest, SelectCommand, SessionCommand, SessionConfig, SpawnSpec, TerminalColors, TerminalMetrics,
    TerminalSession,
};
use terminal_input::NormalizedInput;
use terminal_render::PaintList;

use super::bell::{self, BellFlash};
use super::clipboard_notice::ClipboardNotice;
use super::gl::SkiaState;
use super::ime::{im_cursor_rect, Commit, FocusChange, GtkFocus, ImeGate};
use super::input_queue::{InputQueue, PaneInput, PasteTicket};
use super::keys::{self, ClipboardChord, CopyModeKey, DeliveredKeys, RawKey, RepeatTracker};
use super::pointer;
use super::remote::{self, Again, TermSource};
use super::TerminalFace;
use crate::wheel_zoom::WheelZoom;
use eitri_core::remote_project::terminal_link::OpenAnswer;
use eitri_core::remote_project::terminals::TerminalEntry;
use eitri_core::wm::ShellIdentity;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// GDK's own numbering (X11/Wayland convention, not named constants in `gtk4-rs`): button 1 is the
/// primary (left) button, button 2 the middle.
const BUTTON_PRIMARY: u32 = 1;
const BUTTON_MIDDLE: u32 = 2;

/// How long `Ctrl+Shift+V` waits for the clipboard before typing goes on without the paste
/// (`input_queue`'s module doc). A read from this process takes microseconds and one through a
/// Wayland pipe milliseconds; two seconds is a clipboard owner that is not answering.
pub(crate) const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

/// A fixed environment remains usable alongside a callback evaluated for each new shell.
pub(crate) trait ShellEnvSource {
    fn read(&self) -> Vec<(OsString, OsString)>;
}

impl ShellEnvSource for Vec<(OsString, OsString)> {
    fn read(&self) -> Vec<(OsString, OsString)> {
        self.clone()
    }
}

impl<F: Fn() -> Vec<(OsString, OsString)> + ?Sized> ShellEnvSource for Rc<F> {
    fn read(&self) -> Vec<(OsString, OsString)> {
        self()
    }
}

struct State {
    skia: Option<SkiaState>,
    /// `None` until [`ensure_session`] builds it, the first time a shell is actually about to
    /// spawn. Never built at construction: a window whose terminal is never
    /// shown must not pay for a font manager and a fontconfig match at every launch.
    metrics: Option<TerminalMetrics>,
    /// The last allocation with area the GLArea was given; `None` until the first. What the
    /// metrics are built at when a shell is first wanted (GUI pass 2026-09-23, defect 1).
    allocation: Option<Allocation>,
    /// A shell is wanted (`start` was called). Spawned once there is an `allocation`.
    wanted: bool,
    session: Option<TerminalSession>,
    generation: u64,
    remote: Option<remote::Binding>,
    overlay_wake: Option<futures_channel::mpsc::UnboundedSender<()>>,
    /// Counts input accepted for the shell, including keys and mouse reports parked behind a paste and the start
    /// of a paste; focus reports do not count. A restart decided on an idle read compares it, since a command typed
    /// after the read keeps the same shell and pid. Releasing parked input may count it again; only equality matters.
    typed: u64,
    /// The shell exited, OR the metrics/spawn failed before one ever started: the last frame (or
    /// the failure message) stays, and Enter restarts it.
    exited: bool,
    frame: Option<PaintList>,
    /// Where the cursor is on `frame`, as the session reported it with that frame: where a
    /// composition is drawn (phase 2). `None` is "unknown" -- before the first frame, with a frame
    /// that carried none, and after a restart -- and nothing is drawn at a stale cell then.
    cursor: Option<CursorCell>,
    /// The input method's composition and what of it may reach the shell (phase 2).
    ime: ImeGate,
    repeats: RepeatTracker,
    /// Keyvals whose press this pane forwarded, so it knows which releases are its to forward too.
    delivered: DeliveredKeys,
    /// What is bound for the shell and waits behind a paste still being read (phase 2).
    queue: InputQueue,
    /// Clipboard transfers still owned by this shell, cancelled when their tickets end.
    paste_reads: Vec<(PasteTicket, gio::Cancellable)>,
    /// The bell's flash (phase 2).
    bell: BellFlash,
    clipboard_notice: ClipboardNotice,
    number: u8,
    /// The last selection a drag finished: kept host-side so `Ctrl+Shift+C`
    /// can copy it again without a round trip through the session (`copy_selection`'s own doc).
    /// Cleared on a restart, the same as `cursor`.
    last_selection: Option<String>,
    /// The live mouse-mode summary, as the session last reported it (bottom-terminal phase 3c):
    /// what [`pointer::route`]/[`pointer::wheel_route`] need to decide, *before* sending, whether a
    /// gesture is the program's or this pane's own. Reset to [`MouseModes::default`] (report off) on
    /// a restart, the same as `cursor`.
    mouse: MouseModes,
    /// Which button (if any) a drag or motion is holding, for the live mouse mode's own report.
    /// Reset on a restart: a button held across a restarted shell reports nothing new.
    reports: pointer::ReportTracker,
    /// `prefix [`/`prefix PageUp` opened copy mode: `connect_keyboard` reads
    /// this ahead of `scroll_chord`, and the input method's commit and preedit handlers read it too
    /// ([`Self::route_commit`], [`Self::take_preedit`]) -- that is where every printable key
    /// arrives -- so a copy-mode key never reaches the shell. Reset on exit or
    /// restart (`forget_session`), the same as `cursor`/`last_selection` -- a shell that is gone or
    /// about to be replaced holds nothing left to scroll back through.
    copy_mode: bool,
    /// The last widget-relative pointer position: a wheel event carries no position of its
    /// own, so a report needs the last one the motion controller (or drag gesture) saw.
    last_pointer: (f64, f64),
    colors: TerminalColors,
    focused: bool,
    cwd: PathBuf,
    /// What the window sets on this pane's shells ([`TerminalPane::set_shell_env`]).
    shell_env: Option<Rc<dyn Fn() -> Vec<(OsString, OsString)>>>,
    /// Told when the shell ends in a way that closes the module (`super::closes_on_exit`); set by
    /// `main.rs` ([`TerminalPane::on_shell_exit`]). Called with no borrow of this state held.
    on_shell_exit: Option<Rc<dyn Fn(u64) -> bool>>,
    /// Told the text of an OSC 52 notice the pane could not show because its shell ended and closed the module
    /// first; set by `main.rs` ([`TerminalPane::on_clipboard_notice_lost`]). Called with no borrow of this state held.
    on_notice_lost: Option<Rc<dyn Fn(String)>>,
    /// Told each time a shell is spawned, with its local or server identity; set by `main.rs` ([`TerminalPane::on_shell_start`]).
    /// Called with no borrow of this state held.
    on_shell_start: Option<Rc<dyn Fn(ShellIdentity)>>,
    /// The session is frozen for a move to the other window: what is typed or pointed here goes nowhere until the
    /// move ends, since after a commit the other window answers for the program.
    frozen: bool,
    /// The size the other window had this terminal at, for a pane prepared to take it over before it has an
    /// allocation of its own: what the adopted screen is sized to until GTK lays the pane out.
    prepared: Option<Allocation>,
    /// What the metrics are built in: the face before the zoom, and the window's terminal zoom over it.
    /// Kept while there are no metrics, so the first build is already in the window's face and size.
    face: TerminalFace,
    zoom: f32,
}

/// How far apart two zoom levels may be and still be one size: far below the 0.1 of a step.
const ZOOM_EPSILON: f32 = 1e-4;

/// What a new face or zoom did to the pane ([`State::refont`]).
#[derive(Debug, PartialEq)]
enum Refont {
    /// Neither the face nor the size moved: nothing to draw again.
    Same,
    /// Kept for the metrics' first build, or the metrics were rebuilt in it: drawn again, and the PTY told
    /// `resize` when its size changed.
    Changed { resize: Option<PtySize> },
}

/// What becomes of an input-method commit ([`State::route_commit`]).
#[derive(Debug, PartialEq)]
enum CommitRoute {
    /// To the shell, through the pane's one door ([`submit`]).
    Shell(NormalizedInput),
    /// Copy mode is open: the commit is a copy-mode key or nothing, and never the shell's.
    CopyMode(CopyModeKey),
    /// Nothing at all; the gate's reason, for the log.
    Dropped(&'static str),
}

impl State {
    fn bind_remote(&mut self, registry: Rc<remote::Registry>, module: String, restored: bool, wake: Rc<dyn Fn()>) {
        let mut binding = remote::Binding::new(registry, module);
        // A window that binds no server terminals (a chat or page window) has no saved entry to read.
        binding.restore = restored && binding.owner.is_some();
        binding.ownership_known = !binding.restore;
        binding.wants_restore = restored;
        binding.listen(wake);
        self.remote = Some(binding);
    }

    fn set_start_dir(&mut self, dir: PathBuf) {
        // A reused idle surface is identified by the controller's directory operation before placement or start.
        if !self.wanted {
            if let Some(binding) = self.remote.as_mut() {
                binding.adopt_pending();
            }
        }
        self.cwd = dir;
    }

    fn source(&self) -> TermSource {
        match &self.remote {
            Some(remote) => remote::source_for(&remote.registry.origin()),
            None => remote::unbound_source(),
        }
    }

    fn start_source(state: &RefCell<Self>) -> (TermSource, Vec<(OsString, OsString)>) {
        let (source, shell_env) = {
            let state = state.borrow();
            (state.source(), state.shell_env.clone())
        };
        let extra = if matches!(source, TermSource::Local) {
            shell_env.map(|env| env()).unwrap_or_default()
        } else {
            Vec::new()
        };
        (source, extra)
    }

    fn adopt(
        &mut self,
        record: &serde_json::Value,
        fds: Vec<std::os::fd::OwnedFd>,
        wake: impl Fn() + Send + 'static,
    ) -> Result<(TerminalSession, bool), String> {
        self.allow_adopt()?;
        if self.session.is_some() && !self.exited {
            return Err("the pane already runs a shell".into());
        }
        let size = self.adopt_size(build_metrics)?;
        let config = SessionConfig {
            // An adoption uses the existing PTY and never spawns this shell.
            spawn: SpawnSpec::user_shell(self.cwd.clone()),
            size,
            colors: self.colors,
            focused: self.focused,
            tap: None,
        };
        super::moving::adopt(record, fds, config, wake)
    }

    fn remote_report(&mut self, report: eitri_terminal::mirror::MirrorReport) {
        if let Some(remote) = self.remote.as_mut() {
            remote.report(report, Instant::now());
            if remote.model.ending.is_some() {
                self.exited = true;
            }
        }
    }

    fn remote_exit(&mut self, exit: ExitInfo) {
        if let Some(remote) = self.remote.as_mut() {
            if !exit.detached {
                remote.model.ending = Some(remote::Ending::Exited(exit));
            }
        }
        self.exited = !exit.detached || self.remote.is_none();
    }

    fn reset_for_restart(&mut self) {
        if let Some(remote) = self.remote.as_mut() {
            remote.reset();
        }
        self.forget_session();
        self.generation += 1;
    }

    fn detach(&mut self) -> Result<Receiver<Result<Detached, DetachRefused>>, String> {
        self.allow_adopt()?;
        if self.frozen {
            return Err("the terminal is already frozen for a move".into());
        }
        let session = match (&self.session, self.exited) {
            (Some(session), false) => session,
            _ => return Err("no shell runs in the terminal".into()),
        };
        if let Some(why) = self.freeze_refusal() {
            return Err(why);
        }
        let (answer, answered) = std::sync::mpsc::channel();
        session.send(SessionCommand::Detach(answer));
        self.frozen = true;
        Ok(answered)
    }

    fn allow_adopt(&self) -> Result<(), String> {
        if self.remote.is_some() {
            return Err(super::moving::refusal_text(DetachRefused::ServerTerminal));
        }
        Ok(())
    }

    /// The construction seam `TerminalPane::new` uses. Deliberately GTK-free (no field here holds
    /// a `gtk4` type instantiated by anything but `None`), so a test can assert what a pane's state
    /// looks like the instant it exists -- in particular, that it holds no metrics -- with no
    /// display and no `gtk4::init`.
    fn new(cwd: PathBuf) -> Self {
        State {
            skia: None,
            metrics: None,
            allocation: None,
            wanted: false,
            session: None,
            generation: 0,
            remote: None,
            overlay_wake: None,
            typed: 0,
            exited: false,
            frame: None,
            cursor: None,
            ime: ImeGate::new(),
            repeats: RepeatTracker::new(),
            delivered: DeliveredKeys::new(),
            queue: InputQueue::new(),
            paste_reads: Vec::new(),
            bell: BellFlash::new(),
            clipboard_notice: ClipboardNotice::default(),
            number: 1,
            last_selection: None,
            mouse: MouseModes::default(),
            reports: pointer::ReportTracker::default(),
            copy_mode: false,
            last_pointer: (0.0, 0.0),
            colors: TerminalColors::default(),
            focused: false,
            cwd,
            shell_env: None,
            on_shell_exit: None,
            on_notice_lost: None,
            on_shell_start: None,
            frozen: false,
            prepared: None,
            face: TerminalFace::default(),
            zoom: 1.0,
        }
    }

    /// A new face or zoom for the pane. With metrics, they are rebuilt at the pane's size in the new face,
    /// and the PTY size that comes back is sent when it differs: a larger font is fewer cells, and the
    /// shell must hear of it the way it hears of a smaller pane. Without metrics (no shell has been
    /// wanted yet) nothing is built: the face is kept for the first build, and no font manager is touched
    /// for a terminal never shown. A font that fails to build keeps the old metrics and the old face.
    fn refont(&mut self, face: TerminalFace, zoom: f32) -> Refont {
        // An editor-only step keeps the terminal in place by dividing its factor by the editor's new scale,
        // which can leave the zoom a float's width off the old one: that is the same size, not a rebuild.
        let same_zoom = (zoom - self.zoom).abs() < ZOOM_EPSILON;
        if !zoom.is_finite() || zoom <= 0.0 || (face == self.face && same_zoom) {
            return Refont::Same;
        }
        let Some(metrics) = self.metrics.as_ref() else {
            self.face = face;
            self.zoom = zoom;
            return Refont::Changed { resize: None };
        };
        let before = PtySize::from_metrics(metrics);
        let allocation = self.kept_allocation();
        let rebuilt = match build_metrics(allocation, &face, zoom) {
            Ok(rebuilt) => rebuilt,
            Err(reason) => {
                eprintln!(
                    "[terminal] could not build font metrics for {:?}: {reason}",
                    face.families
                );
                return Refont::Same;
            }
        };
        let after = PtySize::from_metrics(&rebuilt);
        println!(
            "[terminal] font {:?} at {:.1}px: {}x{} cells",
            rebuilt.families(),
            rebuilt.size() * rebuilt.scale(),
            after.cols,
            after.rows
        );
        self.metrics = Some(rebuilt);
        self.face = face;
        self.zoom = zoom;
        Refont::Changed {
            resize: (after != before).then_some(after),
        }
    }

    /// The pane's own allocation once it has one, else the one the other window had it at, else a nominal
    /// one the pane's first allocation resizes.
    fn kept_allocation(&self) -> Allocation {
        self.allocation.or(self.prepared).unwrap_or(NOMINAL_ALLOCATION)
    }

    /// `connect_resize`'s bookkeeping, GTK-free so it is tested without a display: keep the
    /// allocation, and size the metrics to it if there are any yet. Returns the grid the PTY must
    /// be told about, when it changed.
    fn allocate(&mut self, allocation: Allocation) -> Option<PtySize> {
        self.allocation = Some(allocation);
        // `None` until a shell has actually been wanted (`spawn_size` builds it): there is nothing
        // to resize yet, and no PTY to tell. The allocation kept above is what it will be built at.
        let metrics = self.metrics.as_mut()?;
        let changed = if (metrics.scale() - allocation.scale).abs() > f32::EPSILON {
            *metrics = metrics.rescale(allocation.width, allocation.height, allocation.scale);
            true
        } else {
            metrics.resize(allocation.width, allocation.height)
        };
        changed.then(|| PtySize::from_metrics(metrics))
    }

    /// The composition laid out over the latest frame, at the cursor the session reported with it.
    /// `None` with no composition, or while the cursor is unknown.
    fn preedit_layout(&self) -> Option<PreeditLayout> {
        let preedit = self.ime.preedit()?;
        let frame = self.frame.as_ref()?;
        layout_preedit(
            &preedit.text,
            preedit.caret,
            self.cursor?,
            frame.cols,
            frame.rows,
            &self.colors,
        )
    }

    /// The cell the input method's candidate window belongs at: the composition's caret while there
    /// is one, else the cursor. `None` while the cursor is unknown.
    fn ime_caret(&self) -> Option<(u16, u16)> {
        if let Some(layout) = self.preedit_layout() {
            return Some((layout.caret_row, layout.caret_col));
        }
        let cursor = self.cursor?;
        Some((cursor.row, cursor.col))
    }

    /// [`Self::ime_caret`] as the rectangle `set_cursor_location` wants. `None` also before the
    /// metrics exist.
    fn ime_rect(&self, scale_factor: i32) -> Option<Rectangle> {
        let (row, col) = self.ime_caret()?;
        let metrics = self.metrics.as_ref()?;
        Some(im_cursor_rect(metrics.cell_rect(row, col, 1), scale_factor))
    }

    /// Stores the cursor reported with a frame. A frame without a cursor, such as one from a
    /// contained panic, clears the old cell. Returns `true` when the cursor moved while focused
    /// and the input method must be pointed again. fcitx5 places its first candidate window
    /// before `preedit-*`, so the location must be set on gaining focus and on cursor movement.
    fn take_cursor(&mut self, cursor: Option<CursorCell>) -> bool {
        let cell = |cursor: Option<CursorCell>| cursor.map(|c| (c.row, c.col));
        let moved = cell(cursor) != cell(self.cursor);
        self.cursor = cursor;
        moved && cursor.is_some() && self.focused
    }

    /// A bell the session reported. It flashes only a pane that is on screen: a bell rung while the
    /// terminal was hidden is still pending when it is shown again, and flashing it then would be a
    /// flash for something long over ([`connect_visibility`] takes it with `on_screen: false`).
    fn bell(&mut self, on_screen: bool, now: Instant) -> Option<Instant> {
        if on_screen {
            self.bell.ring(now)
        } else {
            None
        }
    }

    /// What an update's OSC 52 writes and exit owe the clipboard notice: the expiry of a notice that starts showing
    /// now, and the notice's text when the exit closes the module (`super::closes_on_exit`), since the close forgets
    /// the pane's notice. The writes are recorded first, so a copy made by the shell's last command in the same
    /// update as its exit is still said. A detached or unknown exit keeps the module, and the notice stays here.
    fn notice_on_update(
        &mut self,
        events: &HostEvents,
        exit: Option<&ExitInfo>,
        on_screen: bool,
        now: Instant,
    ) -> (Option<Instant>, Option<String>) {
        let until = self.clipboard_notice.record(events, on_screen, now);
        let lost = if exit.is_some_and(super::closes_on_exit) {
            self.clipboard_notice.lost_on_close(now, self.number)
        } else {
            None
        };
        (until, lost)
    }

    /// Accepts input now, even when a paste holds it back from the session.
    fn submit(&mut self, input: PaneInput) {
        if !self.remote_input(matches!(input, PaneInput::Key(_))) {
            return;
        }
        let counts_as_typing = !matches!(input, PaneInput::Focus(_));
        match self.queue.submit(input) {
            Some(input) => self.send_input(input),
            None if counts_as_typing => self.typed += 1,
            None => {}
        }
    }

    fn remote_input(&mut self, typed: bool) -> bool {
        let Some(remote) = self.remote.as_mut() else {
            return true;
        };
        let allowed = if typed {
            remote.model.input(Instant::now())
        } else {
            remote.model.recorded && !remote.starting
        };
        if typed {
            if let Some(wake) = &self.overlay_wake {
                let _ = wake.unbounded_send(());
            }
        }
        allowed
    }

    fn connection_changed(&mut self, report: &mut eitri_terminal::mirror::MirrorReport) {
        if report.synced.is_some() && self.copy_mode {
            self.apply_copy_mode_key(CopyModeKey::Leave);
        }
        if report.synced.is_some()
            || report
                .link
                .is_some_and(|link| link != eitri_terminal::mirror::LinkState::Live)
        {
            if !self.queue.is_empty() {
                report.refused_input_bytes = report.refused_input_bytes.saturating_add(1);
            }
            self.queue.clear();
            for (_, read) in self.paste_reads.drain(..) {
                read.cancel();
            }
        }
    }

    /// Accepts a paste before its clipboard read answers, so a restart cannot discard it meanwhile.
    fn begin_paste(&mut self) -> (PasteTicket, gio::Cancellable) {
        self.typed += 1;
        let ticket = self.queue.begin_paste();
        let cancellable = gio::Cancellable::new();
        self.paste_reads.push((ticket, cancellable.clone()));
        (ticket, cancellable)
    }

    /// The queue belongs to this pane and cannot follow a session to the other window.
    fn freeze_refusal(&self) -> Option<String> {
        (!self.queue.is_empty()).then(|| "input is still waiting behind a paste".into())
    }

    /// Straight to the running shell; dropped when there is none (not started, or exited). The one
    /// place that turns a [`PaneInput`] into the matching [`SessionCommand`] (bottom-terminal phase
    /// 3c): a key is `Input`, a mouse gesture `Mouse`, a focus change `FocusReport` -- never the
    /// direct `SessionCommand::Focus` a report must not ride (`set_focused`'s own doc).
    fn send_input(&mut self, input: PaneInput) {
        if !self.remote_input(matches!(input, PaneInput::Key(_))) {
            return;
        }
        if !matches!(input, PaneInput::Focus(_)) {
            self.typed += 1;
        }
        if self.frozen {
            return;
        }
        if let (Some(session), false) = (&self.session, self.exited) {
            let command = match input {
                PaneInput::Key(input) => SessionCommand::Input(input),
                PaneInput::Mouse(m) => SessionCommand::Mouse(m),
                PaneInput::Focus(focused) => SessionCommand::FocusReport(focused),
            };
            session.send(command);
        }
    }

    /// One command to the running shell's session, dropped when there is none. Callers write
    /// `state.borrow().send_command(..)`, so no `if let` scrutinee has to hold the pane's `RefCell`
    /// borrow across the send.
    fn send_command(&self, command: SessionCommand) {
        if let Some(session) = &self.session {
            session.send(command);
        }
    }

    /// Ends one paste and sends what it was holding back. `false`: the paste had already been given
    /// up on (the timeout, or a restart), and its text is not sent.
    fn finish_paste(&mut self, ticket: PasteTicket, input: Option<NormalizedInput>) -> bool {
        if let Some(at) = self.paste_reads.iter().position(|(pending, _)| *pending == ticket) {
            self.paste_reads.swap_remove(at).1.cancel();
        }
        match self.queue.finish(ticket, input) {
            Some(ready) => {
                for input in ready {
                    self.send_input(input);
                }
                true
            }
            None => false,
        }
    }

    /// Enter after the shell exited: the old session goes, and so does anything still waiting to
    /// reach it behind a paste, and where its cursor was (the new shell's first frame says).
    fn forget_session(&mut self) {
        self.session = None;
        self.exited = false;
        self.queue.clear();
        for (_, read) in self.paste_reads.drain(..) {
            read.cancel();
        }
        self.cursor = None;
        self.last_selection = None;
        self.clipboard_notice = ClipboardNotice::default();
        self.mouse = MouseModes::default();
        self.reports = pointer::ReportTracker::default();
        // Leaving copy mode on exit/restart too -- a gone or about-to-be-replaced
        // shell holds nothing left to scroll back through.
        self.copy_mode = false;
    }

    /// A shell taken over from the other window replaces whatever this pane held: the pane starts afresh as for a new
    /// shell, then takes the copy mode the arriving view is in, so its keys keep going to copy mode, not the shell.
    fn took_over(&mut self, copy_mode: bool) {
        self.forget_session();
        self.generation += 1;
        self.wanted = true;
        self.frozen = false;
        self.frame = None;
        self.copy_mode = copy_mode;
    }

    /// `prefix [`/`prefix PageUp`, once the controller has already established this pane holds the
    /// keys (`eitri_core::wm::keys::copy_mode_entry`): does nothing without a running shell -- a
    /// prompt or a message frame with none holds no copy to scroll back through. `up` also asks for
    /// one page up at once, the one difference between `prefix [` and `prefix PageUp`.
    fn enter_copy_mode(&mut self, up: bool) {
        if self.exited {
            return;
        }
        let Some(session) = self.session.as_ref() else { return };
        session.send(SessionCommand::CopyMode(true));
        if up {
            session.send(SessionCommand::Scroll(ScrollRequest::Pages(1)));
        }
        self.copy_mode = true;
    }

    /// One copy-mode key, from either door it can arrive by: `key-pressed` (the arrows, the page keys,
    /// `Ctrl+u`/`Ctrl+d`, `Esc`) or an input-method commit (every printable key, [`Self::route_commit`]).
    /// Nothing here reaches the shell: a scroll goes to the view, a leave snaps it back to the bottom.
    fn apply_copy_mode_key(&mut self, key: CopyModeKey) {
        match key {
            CopyModeKey::Scroll(req) => {
                if let Some(session) = &self.session {
                    session.send(SessionCommand::Scroll(req));
                }
            }
            CopyModeKey::Leave => {
                if let Some(session) = &self.session {
                    session.send(SessionCommand::CopyMode(false));
                }
                self.copy_mode = false;
            }
            CopyModeKey::Swallow => {}
        }
    }

    /// Where an input-method commit goes. The gate decides first, as ever (`ImeGate::commit`: the
    /// keys, a reset of the pane's own, an empty commit). What it would send goes to the shell --
    /// **except in copy mode**, where it is a copy-mode key or nothing (`keys::copy_mode_commit`):
    /// with an input method attached a printable key arrives only as a commit (`ime.rs`), so without
    /// this `k`, `j`, `g`, `G`, `q` and every other letter would be typed into the shell underneath
    /// (copy mode never leaks a key to the shell).
    fn route_commit(&mut self, text: &str, gtk: GtkFocus) -> CommitRoute {
        match self.ime.commit(text, gtk) {
            Commit::Dropped(why) => CommitRoute::Dropped(why),
            Commit::Send(_) if self.copy_mode => CommitRoute::CopyMode(keys::copy_mode_commit(text)),
            Commit::Send(input) => CommitRoute::Shell(input),
        }
    }

    /// A composition started, changed or ended ([`preedit_changed`]'s half that needs no display).
    /// `true`: it must be thrown away -- copy mode has no composition to draw and nothing to commit it
    /// to, so a composition that starts there (rime on, and a letter pressed) is never kept, and the
    /// caller resets the input method. The letter that started it does nothing: with rime composing,
    /// the key was the input method's, as it would be in any terminal under tmux's copy mode.
    fn take_preedit(&mut self, text: &str, caret: i32) -> bool {
        if self.copy_mode {
            self.ime.preedit_changed("", 0);
            return !text.is_empty();
        }
        self.ime.preedit_changed(text, caret);
        false
    }

    /// What [`pointer::route`]/[`pointer::wheel_route`] see: the live mode while a shell is running,
    /// or [`MouseModes::default`] (report off) once it has exited or never started -- a program that
    /// died in mouse mode (`kill -9 htop`) must not leave its final screen unselectable.
    fn routing_modes(&self) -> MouseModes {
        if self.exited || self.session.is_none() {
            MouseModes::default()
        } else {
            self.mouse
        }
    }

    /// [`TerminalPane::close`]'s half that needs no display: no shell wanted, none kept, nothing of
    /// the last one left on screen. The metrics and the allocation stay: they are the pane's, not
    /// the shell's, and the next start spawns at them.
    fn close(&mut self) {
        self.wanted = false;
        self.frozen = false;
        self.forget_session();
        self.frame = None;
    }

    /// The size an adopted session starts at: the pane's own allocation once it has one, else the one the other
    /// window had it at, else a nominal one the pane's first allocation resizes (the other window could not tell, and
    /// the shell is better taken over at a guess than left with no window). The metrics are built at it the first
    /// time, as a spawn builds them.
    fn adopt_size(
        &mut self,
        build: impl FnOnce(Allocation, &TerminalFace, f32) -> Result<TerminalMetrics, String>,
    ) -> Result<PtySize, String> {
        let allocation = self.kept_allocation();
        let metrics = match self.metrics.take() {
            Some(metrics) => metrics,
            None => build(allocation, &self.face, self.zoom)?,
        };
        let size = PtySize::from_metrics(&metrics);
        self.metrics = Some(metrics);
        Ok(size)
    }

    /// [`ensure_session`]'s gate, and the size it spawns at. `None`: nothing to start (no shell
    /// wanted, no allocation yet, one already running, or an exited one waiting for Enter).
    /// `Some(Err)`: the font failed. Otherwise the metrics exist -- built by `build` at the kept
    /// allocation the first time -- and the size is theirs.
    fn spawn_size(
        &mut self,
        build: impl FnOnce(Allocation, &TerminalFace, f32) -> Result<TerminalMetrics, String>,
    ) -> Option<Result<PtySize, String>> {
        if !self.wanted || self.session.is_some() || self.exited {
            return None;
        }
        let allocation = self.allocation?;
        let metrics = match self.metrics.take() {
            Some(metrics) => metrics,
            None => match build(allocation, &self.face, self.zoom) {
                Ok(metrics) => metrics,
                Err(reason) => return Some(Err(reason)),
            },
        };
        let size = PtySize::from_metrics(&metrics);
        self.metrics = Some(metrics);
        Some(Ok(size))
    }
}

/// One allocation of the GLArea, as its `resize` signal gives it: DEVICE pixels, and the scale
/// they are at -- what [`TerminalMetrics`] is built and resized with.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Allocation {
    width: f32,
    height: f32,
    scale: f32,
}

/// What metrics are built at with no allocation of the pane's own and none from the other window: a guess
/// the pane's first allocation resizes (an adopted shell is better taken over at one than left with no
/// window).
const NOMINAL_ALLOCATION: Allocation = Allocation {
    width: 800.0,
    height: 480.0,
    scale: 1.0,
};

/// Cheap to clone: a handle on one widget and its state.
#[derive(Clone)]
pub(crate) struct TerminalPane {
    area: GLArea,
    state: Rc<RefCell<State>>,
    /// The input method (phase 2): `area`'s key controller runs it, and its client widget is `area`
    /// from the first [`Self::start`].
    im: IMMulticontext,
}

impl TerminalPane {
    /// The pane for a terminal whose shell will run in `cwd` (the resolved project root).
    pub(crate) fn new(cwd: PathBuf) -> Self {
        let area = GLArea::builder()
            .hexpand(true)
            .vexpand(true)
            // Both must match what `SkiaState::ensure_surface` tells Skia: stencil 8, no MSAA.
            .has_stencil_buffer(true)
            .auto_render(false)
            .focusable(true)
            .can_focus(true)
            .build();
        area.add_css_class("terminal");
        let state = Rc::new(RefCell::new(State::new(cwd)));
        // Its client widget is set at the first `start`, not here: that is what creates the input
        // method's own client (for fcitx5-gtk, a D-Bus client and an input window), and a window
        // whose terminal is never shown must not pay for one. Neither this nor the purpose does.
        let im = IMMulticontext::new();
        // What VTE tells the input method too: a terminal, where any character and the control
        // codes are input. (fcitx5-gtk has no terminal case and treats it as free form.)
        im.set_input_purpose(InputPurpose::Terminal);
        connect_resize(&area, &state, &im);
        connect_scale_change(&area);
        connect_render(&area, &state);
        connect_keyboard(&area, &state, &im);
        connect_ime(&area, &state, &im);
        connect_pointer(&area, &state, &im);
        connect_scroll(&area, &state);
        connect_visibility(&area, &state, &im);
        let (wake, mut woken) = futures_channel::mpsc::unbounded();
        state.borrow_mut().overlay_wake = Some(wake);
        let weak_state = Rc::downgrade(&state);
        let weak_area = area.downgrade();
        glib::spawn_future_local(async move {
            while woken.recv().await.is_ok() {
                let (Some(state), Some(area)) = (weak_state.upgrade(), weak_area.upgrade()) else {
                    break;
                };
                let expiry = state.borrow().remote.as_ref().and_then(|r| r.model.expiry());
                area.queue_render();
                if let Some(until) = expiry {
                    repaint_at(&area, until);
                }
            }
        });
        TerminalPane { area, state, im }
    }

    /// The module host that `main.rs` adds to the grid. Keys reach it through `focus_module`,
    /// which refuses hidden modules, rather than through the pane's own focus call.
    pub(crate) fn widget(&self) -> &GLArea {
        &self.area
    }

    pub(crate) fn set_remote(&self, registry: Rc<remote::Registry>, module: String, restored: bool) {
        if registry.origin().remote().is_none() || (self.state.borrow().remote.is_some() && !registry.adopting()) {
            return;
        }
        let state = Rc::downgrade(&self.state);
        let area = self.area.downgrade();
        let im = self.im.downgrade();
        self.state.borrow_mut().bind_remote(
            registry,
            module,
            restored,
            Rc::new(move || {
                let (Some(state), Some(area), Some(im)) = (state.upgrade(), area.upgrade(), im.upgrade()) else {
                    return;
                };
                restore_remote_owner(&state);
                ensure_session(&state, &area, &im);
            }),
        );
        restore_remote_owner(&self.state);
        // A pane wanted before it was bound waited; with its binding it may start now, as a wake would start it.
        ensure_session(&self.state, &self.area, &self.im);
    }

    pub(crate) fn claims_remote_terminal(&self) -> bool {
        self.state
            .borrow()
            .remote
            .as_ref()
            .is_some_and(remote::Binding::claims_module)
    }

    pub(crate) fn may_have_job(&self) -> bool {
        let state = self.state.borrow();
        !state.exited && (state.session.is_some() || state.remote.as_ref().is_some_and(remote::Binding::may_have_job))
    }

    pub(crate) fn shell_identity(&self) -> Option<ShellIdentity> {
        let state = self.state.borrow();
        if state.exited {
            return None;
        }
        if let Some(remote) = &state.remote {
            return remote
                .entry
                .as_ref()?
                .terminal_id
                .as_deref()
                .and_then(|id| remote::identity(state.generation, id));
        }
        state
            .session
            .as_ref()
            .and_then(TerminalSession::local_pid)
            .map(|pid| ShellIdentity::Local { pid })
    }

    pub(crate) fn job_query(&self) -> Option<Box<dyn super::jobs::JobQuery>> {
        let state = self.state.borrow();
        if state.exited {
            return None;
        }
        if let Some(remote) = &state.remote {
            let id = remote.entry.as_ref()?.terminal_id.clone()?;
            return Some(Box::new(super::jobs::Server::new(remote.registry.facts.clone(), id)));
        }
        state
            .session
            .as_ref()
            .and_then(TerminalSession::local_pid)
            .map(|pid| Box::new(super::jobs::Local { pid }) as Box<dyn super::jobs::JobQuery>)
    }

    pub(crate) fn respawn_server(&self, dir: PathBuf) -> Result<(), String> {
        if self.state.borrow().remote.is_none() {
            return Err("this terminal has no server".into());
        }
        self.close();
        self.set_start_dir(dir);
        self.start();
        Ok(())
    }

    pub(crate) fn set_number(&self, number: u8) {
        self.state.borrow_mut().number = number;
    }

    /// A shell is wanted: spawn it now if the pane has been laid out, else at its first allocation.
    /// Also where the input method gets its client widget, once (GTK ignores the same widget again;
    /// set late, `GtkIMMulticontext` rebuilds its delegate with it, focus included).
    pub(crate) fn start(&self) {
        self.im.set_client_widget(Some(&self.area));
        {
            let mut state = self.state.borrow_mut();
            state.generation += 1;
            state.wanted = true;
        }
        ensure_session(&self.state, &self.area, &self.im);
    }

    /// Whether a live local child or mirror session occupies this pane.
    pub(crate) fn is_running(&self) -> bool {
        let state = self.state.borrow();
        !state.exited && (state.session.is_some() || state.remote.as_ref().is_some_and(|r| r.starting))
    }

    /// The running shell's pid, absent before spawn and after exit.
    pub(crate) fn shell_pid(&self) -> Option<u32> {
        let state = self.state.borrow();
        if state.exited {
            None
        } else {
            state.session.as_ref().and_then(TerminalSession::local_pid)
        }
    }

    /// The directory used by the next shell; a running shell keeps its own directory.
    pub(crate) fn set_start_dir(&self, dir: PathBuf) {
        self.state.borrow_mut().set_start_dir(dir);
    }

    /// How many keys and mouse reports the pane has been given for its shell; a focus report is not one.
    pub(crate) fn typed(&self) -> u64 {
        self.state.borrow().typed
    }

    /// The identity of the current start, retained across a close until the next start.
    pub(crate) fn generation(&self) -> u64 {
        self.state.borrow().generation
    }

    /// Whether the pane holds the keys, as the focus report says: a solid cursor or a hollow one.
    ///
    /// **This is not what focuses the input method in or out: GTK's key controller did, before
    /// the focus report could say so** (`ime.rs`'s module doc), and a composition fcitx5 committed on the
    /// way out was dropped by the commit handler asking GTK, not by this. What is left here: gaining
    /// the keys points the input method at the cursor before the first key; losing them resets it,
    /// which discards what survives a focus-out in the client (a dead-key sequence in GTK's simple
    /// context or fcitx5-gtk's xkb compose state, `fcitximcontext.cpp:1113-1130`; the gate is
    /// already closed, so nothing that reset commits passes), and takes the composition off the
    /// screen.
    ///
    /// `SessionCommand::Focus` goes straight to the session, beside [`submit`], because it changes
    /// only the cursor's shape: no byte reaches the program. **Phase 3's focus reports (DECSET 1004)
    /// do not ride it that way:** a report is input, and one sent here would overtake keys still
    /// waiting behind a paste -- so this method also
    /// submits a [`PaneInput::Focus`] through the door, once the borrow above is released.
    pub(crate) fn set_focused(&self, focused: bool) {
        let (was, change) = {
            let mut state = self.state.borrow_mut();
            let was = state.focused;
            state.focused = focused;
            if let Some(session) = &state.session {
                session.send(SessionCommand::Focus(focused));
            }
            (was, state.ime.set_focused(focused))
        };
        // Submit the focus report after dropping the borrow above: `submit` mutably borrows `state`,
        // and a nested borrow would panic the GTK thread. The focus report already sends only changes,
        // so `was != focused` is an additional guard.
        if was != focused {
            submit(&self.state, PaneInput::Focus(focused));
        }
        match change {
            FocusChange::In => point_input_method(&self.im, &self.state, &self.area),
            FocusChange::Out => {
                self.im.reset();
                self.area.queue_render();
            }
            FocusChange::Unchanged => {}
        }
    }

    pub(crate) fn set_colors(&self, colors: TerminalColors) {
        let mut state = self.state.borrow_mut();
        state.colors = colors;
        if let Some(session) = &state.session {
            session.send(SessionCommand::SetColors(colors));
        }
        drop(state);
        self.area.queue_render();
    }

    /// The face the pane draws in before the zoom: the editor's `guifont`, or `terminal.font`.
    pub(crate) fn set_face(&self, face: TerminalFace) {
        let zoom = self.state.borrow().zoom;
        self.refont(face, zoom);
    }

    /// The window's terminal text zoom, relative to the face's own size.
    pub(crate) fn set_zoom(&self, zoom: f32) {
        let face = self.state.borrow().face.clone();
        self.refont(face, zoom);
    }

    fn refont(&self, face: TerminalFace, zoom: f32) {
        let changed = self.state.borrow_mut().refont(face, zoom);
        let Refont::Changed { resize } = changed else { return };
        self.area.queue_render();
        if let Some(size) = resize {
            self.state.borrow().send_command(SessionCommand::Resize(size));
        }
    }

    /// Hands the running shell input that did not come from this widget's own keyboard -- the
    /// prefix's literal `Ctrl+a`/`Ctrl+l`. Dropped when no shell is running; behind a paste still
    /// being read, it waits for it ([`submit`]).
    pub(crate) fn send(&self, input: NormalizedInput) {
        discard_composition(&self.state, &self.im, &self.area);
        submit(&self.state, PaneInput::Key(input));
    }

    /// `prefix [`/`prefix PageUp`: the controller's `TermOp::CopyMode` reaches this only once
    /// `eitri_core::wm::keys::copy_mode_entry` has already said this pane holds the keys.
    ///
    /// A composition in progress is thrown away first, as a paste or a `Ctrl+a` literal throws it
    /// away: copy mode has nowhere to commit it, and it must not reach the shell once copy mode ends.
    pub(crate) fn enter_copy_mode(&self, up: bool) {
        discard_composition(&self.state, &self.im, &self.area);
        self.state.borrow_mut().enter_copy_mode(up);
    }

    /// `f` runs when the shell ends by itself in a way that closes the module (`super::closes_on_exit`),
    /// with that session's generation, after the notice has been published and with this pane's state
    /// released. It takes the module
    /// off screen and says whether it did; if it did, the pane is [`Self::close`]d. If it could not
    /// -- the terminal is the last module on screen (`super::OnExit::CloseWindow`), so `f` closes
    /// the window instead of the module -- this pane is left alone: the notice stays and Enter
    /// restarts, until the window really goes.
    pub(crate) fn on_shell_exit(&self, f: impl Fn(u64) -> bool + 'static) {
        self.state.borrow_mut().on_shell_exit = Some(Rc::new(f));
    }

    /// `f` runs with the host-only text of an OSC 52 notice that the pane's closing would otherwise drop: the shell
    /// ended by itself and closes the module while its writes were never shown (the pane was hidden) or are still on
    /// screen. It runs just before the end is reported (see [`Self::on_shell_exit`]), with this pane's state released.
    pub(crate) fn on_clipboard_notice_lost(&self, f: impl Fn(String) + 'static) {
        self.state.borrow_mut().on_notice_lost = Some(Rc::new(f));
    }

    /// `f` runs each time a shell is spawned in this pane -- at its first allocation after a start, or
    /// at once when the pane is laid out, and on the Enter that restarts an exited one -- with the
    /// shell's identity, with this pane's state released.
    pub(crate) fn on_shell_start(&self, f: impl Fn(ShellIdentity) + 'static) {
        self.state.borrow_mut().on_shell_start = Some(Rc::new(f));
    }

    /// What every shell this pane starts gets on top of the host's environment. A shell that is already
    /// running keeps what it has. A callback runs once before trying the initial shell and its fallbacks.
    pub(crate) fn set_shell_env(&self, env: impl ShellEnvSource + 'static) {
        self.state.borrow_mut().shell_env = Some(Rc::new(move || env.read()));
    }

    /// `prefix x` (or the shell's own end): the shell is hung up if it still runs, and the pane
    /// forgets it -- its screen, its cursor, anything queued behind a paste (and the input method is reset) -- so the
    /// next [`Self::start`] is a fresh shell on a blank pane, as the first one was. Does not wait:
    /// `TerminalSession`'s own `Drop` hangs up and reaps off the thread.
    pub(crate) fn close(&self) {
        close_remote(&self.state, !crate::end_signals::signal_close_started());
        self.state.borrow_mut().close();
        // No borrow held: `reset` can emit `commit`/`preedit-changed`, whose handlers borrow the state.
        self.im.reset();
        self.area.queue_render();
    }

    /// Freezes the running shell's session for a move to the other window: it reads, writes and draws nothing until
    /// the move ends, and what is typed here meanwhile is dropped. The receiver gives the session's answer (what the
    /// other window needs, or why the terminal stays); it is waited on off the GTK thread. Refused with no running
    /// shell, one already frozen, or while input waits behind a paste.
    pub(crate) fn detach(&self) -> Result<Receiver<Result<Detached, DetachRefused>>, String> {
        discard_composition(&self.state, &self.im, &self.area);
        self.state.borrow_mut().detach()
    }

    /// The move did not happen: the frozen session rebuilds its screen, writes what it owed and reads again, and
    /// this pane takes keys for it again. Nothing for a pane that is not frozen.
    pub(crate) fn resume(&self) {
        let mut state = self.state.borrow_mut();
        if !state.frozen {
            return;
        }
        state.frozen = false;
        state.send_command(SessionCommand::Resume);
        drop(state);
        self.area.queue_render();
    }

    /// The other window holds the terminal now: the session lets its shell go without a hangup and ends, and the
    /// pane forgets it as a kill's close does, so the number's next start is a fresh shell. The shell's exit is still
    /// this process's to reap, which the window's spawner watch does (`linked_window::spawn::watch_moved_child`).
    pub(crate) fn forget(&self) {
        {
            let mut state = self.state.borrow_mut();
            // `Committed` is read before the `Shutdown` the dropped handle sends, and the session has ended by then.
            state.send_command(SessionCommand::Committed);
            state.close();
        }
        self.im.reset();
        self.area.queue_render();
    }

    /// Made ready to take a terminal over from the other window, before anything there is frozen: the size it has
    /// there, in logical pixels, so the screen it brings is laid out at it until this pane is allocated.
    pub(crate) fn prepare(&self, width: u32, height: u32) {
        let scale = self.area.scale_factor().max(1) as f32;
        let mut state = self.state.borrow_mut();
        state.prepared = (width > 0 && height > 0).then_some(Allocation {
            width: width as f32 * scale,
            height: height as f32 * scale,
            scale,
        });
    }

    /// Takes over the terminal a committed move carried: its shell, its PTY and its screen, which the session
    /// rebuilds and draws, writing what the other window still owed the shell. Refused, with nothing taken, when the
    /// record or its descriptors do not hold a terminal this process can adopt, or the pane already runs a shell.
    pub(crate) fn adopt(&self, record: &serde_json::Value, fds: Vec<std::os::fd::OwnedFd>) -> Result<(), String> {
        self.state.borrow().allow_adopt()?;
        self.im.set_client_widget(Some(&self.area));
        let (wake, woken) = futures_channel::mpsc::unbounded::<()>();
        let (session, copy_mode) = self.state.borrow_mut().adopt(record, fds, move || {
            let _ = wake.unbounded_send(());
        })?;
        let pid = session.pid();
        println!("[terminal] took over the shell {pid} from the other window");
        let started = {
            let mut state = self.state.borrow_mut();
            state.took_over(copy_mode);
            state.session = Some(session);
            state.on_shell_start.clone()
        };
        if let Some(started) = started {
            started(ShellIdentity::Local { pid });
        }
        watch_session(&self.state, &self.area, &self.im, woken);
        Ok(())
    }

    /// How a shell this pane took over from the other window ended, as the process that started it reports it: only
    /// that process can wait for it. Whether this pane runs that shell, named by its pid and its start time, so an
    /// end reported for an earlier process that had the same pid is not taken for this one's.
    pub(crate) fn child_exited(&self, pid: u32, start: u64, status: std::process::ExitStatus) -> bool {
        let state = self.state.borrow();
        match &state.session {
            Some(session) if session.local_pid() == Some(pid) && session.adopted_start() == Some(start) => {
                session.send(SessionCommand::ChildExited(status));
                true
            }
            _ => false,
        }
    }

    /// Window close: hang the shell up. Does not wait -- `TerminalSession`'s own `Drop`.
    pub(crate) fn shutdown(&self) {
        close_remote(&self.state, !crate::end_signals::signal_close_started());
        let mut state = self.state.borrow_mut();
        state.wanted = false;
        state.session = None;
    }
}

impl super::registry::Pane for TerminalPane {
    fn close(&self) {
        TerminalPane::close(self);
    }
}

/// What the pane shows a failure message at when it has no metrics to size a screen from -- a
/// font failure, before any metrics have ever been built.
const NO_METRICS_SIZE: PtySize = PtySize {
    cols: 80,
    rows: 24,
    cell_width_px: 0,
    cell_height_px: 0,
};

/// Builds the pane's font metrics at `allocation`, containing a font-manager panic rather than
/// letting it take the whole process down: a machine with no
/// usable monospace font must fail with a message in the pane, like a shell that would not start,
/// not crash the window.
///
/// **At the allocation, never at a placeholder** (GUI pass 2026-09-23, defect 1). The first version
/// built these at 1x1, "exactly as `TerminalPane::new` used to build it eagerly", trusting the next
/// resize to correct cols/rows. Eagerly, that was true: the first resize found metrics and sized
/// them before the spawn. Lazily it is not: the first resize is the one that finds none, the spawn
/// follows it at 1x1, and there is no next resize until the pane's size changes.
///
/// In `face` at `zoom`: the face's size times the window's terminal zoom, in logical pixels (the metrics
/// scale it to device pixels).
fn build_metrics(allocation: Allocation, face: &TerminalFace, zoom: f32) -> Result<TerminalMetrics, String> {
    let Allocation { width, height, scale } = allocation;
    let size = face.base_px * zoom;
    panic::catch_unwind(|| TerminalMetrics::with_families(&face.families, size, width, height, scale)).map_err(
        |payload| {
            payload
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "the font manager panicked".to_string())
        },
    )
}

fn shell_spec(program: Option<PathBuf>, cwd: PathBuf, extra: &[(OsString, OsString)]) -> SpawnSpec {
    let mut spec = match program {
        None => SpawnSpec::user_shell(cwd),
        Some(program) => SpawnSpec::shell(program, cwd),
    };
    spec.extra_env = extra.to_vec();
    spec
}

/// Spawns the session if one is wanted, the pane has real geometry, and none is running. Also
/// where the terminal's font metrics are built, the first time they are needed -- never at
/// construction, so a window whose terminal is never shown never touches a font manager.
fn ensure_session(state: &Rc<RefCell<State>>, area: &GLArea, im: &IMMulticontext) {
    if state.borrow().remote.as_ref().is_some_and(|remote| remote.starting) {
        return;
    }
    let (cwd, size, colors, focused) = {
        let mut s = state.borrow_mut();
        let size = match s.spawn_size(build_metrics) {
            None => return,
            Some(Ok(size)) => size,
            Some(Err(reason)) => {
                eprintln!("[terminal] could not build font metrics: {reason}");
                s.exited = true;
                s.frame = Some(message_frame(
                    NO_METRICS_SIZE,
                    s.colors,
                    &format!("[no terminal font available: {reason} \u{2014} Enter tries again]"),
                ));
                drop(s);
                area.queue_render();
                return;
            }
        };
        (s.cwd.clone(), size, s.colors, s.focused)
    };
    if retry_slot_before_start(state) {
        return;
    }
    let (source, extra) = State::start_source(state);
    match source {
        TermSource::Waiting => {
            let mut s = state.borrow_mut();
            if s.frame.is_none() {
                s.frame = Some(message_frame(size, colors, ""));
            }
            drop(s);
            area.queue_render();
            return;
        }
        TermSource::Server(connection) => {
            start_remote(state, area, im, connection, size, colors, focused);
            return;
        }
        TermSource::Local => {}
    }
    let (wake, woken) = futures_channel::mpsc::unbounded::<()>();
    let mut spec = shell_spec(None, cwd.clone(), &extra);
    let mut tried = Vec::new();
    let started = loop {
        let program = spec.program.clone();
        let wake = wake.clone();
        let config = SessionConfig {
            spawn: spec,
            size,
            colors,
            focused,
            tap: None,
        };
        match TerminalSession::spawn(config, move || {
            let _ = wake.unbounded_send(());
        }) {
            Ok(session) => {
                println!("[terminal] started {} (pid {})", program.display(), session.pid());
                break Ok(session);
            }
            Err(err) => {
                eprintln!("[terminal] could not start {}: {err}", program.display());
                tried.push(program);
                // Looked up only now, on this rare path: a passwd lookup can go through NSS, and
                // this is the GTK thread.
                match fallback_shell(&tried, passwd_shell()) {
                    Some(next) => spec = shell_spec(Some(next), cwd.clone(), &extra),
                    None => break Err(err),
                }
            }
        }
    };
    // Only the running session's waker may keep the channel open: its end is the future's end.
    drop(wake);
    let session = match started {
        Ok(session) => session,
        Err(err) => {
            let mut s = state.borrow_mut();
            s.exited = true;
            s.frame = Some(message_frame(
                size,
                colors,
                &format!("[no shell would start: {err} \u{2014} Enter tries again]"),
            ));
            drop(s);
            area.queue_render();
            return;
        }
    };
    let pid = session.pid();
    let started = {
        let mut s = state.borrow_mut();
        s.session = Some(session);
        s.on_shell_start.clone()
    };
    if let Some(started) = started {
        started(ShellIdentity::Local { pid });
    }
    watch_session(state, area, im, woken);
}

/// A window that could not take the first slot at its start (its previous owner was still exiting) tries again as a
/// pane is about to start, on Enter included. Taking it wakes every pane, this one too: each reads its saved entry
/// first, and the woken start goes on, so this one stops (`true`).
fn retry_slot_before_start(state: &RefCell<State>) -> bool {
    let retry = state.borrow().slot_retry();
    match retry {
        Some(registry) if registry.retry_slot() => {
            registry.wake();
            true
        }
        _ => false,
    }
}

fn restore_remote_owner(state: &Rc<RefCell<State>>) {
    {
        let mut s = state.borrow_mut();
        if let Some(remote) = s.remote.as_mut() {
            remote.ensure_owner();
        }
    }
    let (owner, work, cancelled, generation) = {
        let s = state.borrow();
        let Some(remote) = s.remote.as_ref() else { return };
        if !remote.restore || remote.ownership_known || remote.starting || s.session.is_some() {
            return;
        }
        let Some(owner) = remote.current_owner() else {
            return;
        };
        (owner, remote.work.clone(), remote.cancelled.clone(), s.generation)
    };
    let (tell, mut told) = futures_channel::mpsc::unbounded();
    work.run(move || {
        let _ = tell.unbounded_send(owner.restore());
    });
    let state = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let Ok(result) = told.recv().await else { return };
        let Some(state) = state.upgrade() else { return };
        let mut s = state.borrow_mut();
        if s.generation != generation || s.session.is_some() || cancelled.load(Ordering::SeqCst) {
            return;
        }
        let Some(remote) = s.remote.as_mut() else { return };
        if remote.starting {
            return;
        }
        match result {
            Ok(entry) => {
                remote.entry = entry;
                remote.ownership_known = true;
            }
            Err(error) => {
                let registry = remote.registry.clone();
                drop(s);
                registry.notice(format!(
                    "terminal records: {}",
                    eitri_core::onboarding::display::display_text(&error.to_string())
                ));
            }
        }
    });
}

enum RemoteReady {
    Started(TerminalSession, TerminalEntry, OpenAnswer),
    Recorded((String, String, String)),
    Failed(String),
}

/// The size, colours and focus a server session is started with. The start runs on the record worker and over the
/// network, and the pane keeps changing meanwhile; what differs when the session arrives is sent to it then.
#[derive(Clone, Copy, Debug, PartialEq)]
struct View {
    size: PtySize,
    colors: TerminalColors,
    focused: bool,
}

/// What a session that has just arrived is told about the pane's changes since its start was captured.
#[derive(Debug, Default, PartialEq)]
struct CatchUp {
    size: Option<PtySize>,
    colors: Option<TerminalColors>,
    focused: Option<bool>,
}

/// A server start decided on the GTK thread, for its record worker.
struct RemoteStart {
    owner: remote::Owner,
    work: remote::Work,
    cancelled: Arc<AtomicBool>,
    again: Option<Again>,
    retry: Option<TerminalEntry>,
    cwd: PathBuf,
    generation: u64,
    view: View,
}

type ShellStarted = Option<(Rc<dyn Fn(ShellIdentity)>, ShellIdentity)>;

impl State {
    /// The window registry of a bound pane that has no owner yet, which may try for the first slot again.
    fn slot_retry(&self) -> Option<Rc<remote::Registry>> {
        self.remote
            .as_ref()
            .filter(|remote| remote.owner.is_none() || remote.registry.may_retry_slot())
            .map(|remote| remote.registry.clone())
    }

    /// `None` when there is nothing to start: a start is already under way, or this window binds no server terminal
    /// (the pane then ends with that reason).
    fn begin_remote_start(&mut self, view: View) -> Option<RemoteStart> {
        let cwd = self.cwd.clone();
        let generation = self.generation;
        let remote = self.remote.as_mut()?;
        if remote.starting {
            return None;
        }
        remote.ensure_owner();
        let Some(owner) = remote.current_owner() else {
            remote.model.ending = Some(remote::Ending::Failed(remote.registry.no_slot_reason().into()));
            self.exited = true;
            return None;
        };
        remote.starting = true;
        remote.model.opening = true;
        remote.cancelled = Arc::new(AtomicBool::new(false));
        Some(RemoteStart {
            owner,
            work: remote.work.clone(),
            cancelled: remote.cancelled.clone(),
            again: remote.again.take(),
            retry: remote.retry_entry.take(),
            cwd,
            generation,
            view,
        })
    }

    /// The pane's size, colours and focus now; the size is the metrics' grid, `fallback`'s before any metrics.
    fn view(&self, fallback: PtySize) -> View {
        View {
            size: self.metrics.as_ref().map(PtySize::from_metrics).unwrap_or(fallback),
            colors: self.colors,
            focused: self.focused,
        }
    }

    /// Takes the started session. Sizes, colours and focus set while the start ran went nowhere (there was no session
    /// to send them to), so whatever differs from what the session was started with is sent to it now.
    fn remote_started(
        &mut self,
        session: TerminalSession,
        entry: TerminalEntry,
        answer: OpenAnswer,
        started: View,
        mapped: bool,
    ) -> (ShellStarted, CatchUp) {
        let now = self.view(started.size);
        let catch_up = CatchUp {
            size: (now.size != started.size).then_some(now.size),
            colors: (now.colors != started.colors).then_some(now.colors),
            focused: (now.focused != started.focused).then_some(now.focused),
        };
        let generation = self.generation;
        let callback = self.on_shell_start.clone();
        let Some(remote) = self.remote.as_mut() else {
            return (None, CatchUp::default());
        };
        remote.starting = false;
        remote.ownership_known = true;
        // Attach identities were durable before this start. Open identities wait for Recorded.
        remote.model.recorded = entry.terminal_id.is_some();
        let identity = entry
            .terminal_id
            .as_deref()
            .and_then(|id| remote::identity(generation, id));
        remote.entry = Some(entry);
        remote.answer = Some(answer);
        session.send(SessionCommand::Visible(mapped));
        if let Some(size) = catch_up.size {
            session.send(SessionCommand::Resize(size));
        }
        if let Some(colors) = catch_up.colors {
            session.send(SessionCommand::SetColors(colors));
        }
        if let Some(focused) = catch_up.focused {
            session.send(SessionCommand::Focus(focused));
        }
        self.session = Some(session);
        (callback.zip(identity), catch_up)
    }

    fn remote_recorded(&mut self, identity: (String, String, String), generation: u64) -> ShellStarted {
        let callback = self.on_shell_start.clone();
        let remote = self.remote.as_mut()?;
        remote.model.recorded = true;
        if let Some(entry) = remote.entry.as_mut() {
            entry.terminal_id = Some(identity.0.clone());
            entry.daemon_instance = Some(identity.1);
            entry.boot_id = Some(identity.2);
        }
        callback.zip(remote::identity(generation, &identity.0))
    }

    /// The start failed before a session existed: the entry stays, and Enter reattaches it.
    fn remote_failed(&mut self, why: &str) -> Option<Rc<remote::Registry>> {
        let remote = self.remote.as_mut()?;
        remote.starting = false;
        remote.model.ending = Some(remote::Ending::Failed(why.to_owned()));
        // A failed record keeps keys held; Enter reattaches the existing intent.
        remote.again = Some(Again::Attach);
        let registry = remote.registry.clone();
        self.exited = true;
        Some(registry)
    }
}

/// The record worker's half of a server start: the durable intent, then the link and its mirror (`start`), then, for
/// an open, the answer saved before the pane lets its keys through. Production passes `Owner::start`.
fn run_remote_start(
    plan: RemoteStart,
    start: impl FnOnce(
        &remote::Owner,
        PathBuf,
        Option<Again>,
        Option<TerminalEntry>,
        View,
        Box<dyn Fn() + Send>,
    ) -> std::io::Result<(TerminalSession, TerminalEntry, OpenAnswer)>,
    tell: futures_channel::mpsc::UnboundedSender<RemoteReady>,
    wake: futures_channel::mpsc::UnboundedSender<()>,
) {
    let RemoteStart {
        owner,
        work: writes,
        cancelled,
        again,
        retry,
        cwd,
        view,
        ..
    } = plan;
    if cancelled.load(Ordering::SeqCst) {
        return;
    }
    let started = start(
        &owner,
        cwd,
        again,
        retry,
        view,
        Box::new(move || {
            let _ = wake.unbounded_send(());
        }),
    );
    let (session, entry, answer) = match started {
        Ok(started) => started,
        Err(error) => {
            let _ = tell.unbounded_send(RemoteReady::Failed(error.to_string()));
            return;
        }
    };
    let is_attach = entry.terminal_id.is_some();
    let saved = entry.clone();
    if tell
        .unbounded_send(RemoteReady::Started(session, entry, answer.clone()))
        .is_err()
    {
        return;
    }
    if is_attach {
        return;
    }
    // Waiting for the open answer holds no record worker, so a close can queue its stop meanwhile.
    std::thread::spawn(move || {
        while !cancelled.load(Ordering::SeqCst) {
            if let Some(identity) = answer.wait(Instant::now() + Duration::from_secs(1)) {
                writes.run(move || match owner.opened(&saved, &identity) {
                    Ok(()) => {
                        let _ = tell.unbounded_send(RemoteReady::Recorded(identity));
                    }
                    Err(error) => {
                        let _ = tell.unbounded_send(RemoteReady::Failed(format!(
                            "the terminal identity could not be saved: {error}"
                        )));
                    }
                });
                return;
            }
            // A link that ended without an answer never gives one: the GTK side stops listening with this sender.
            if tell.is_closed() || answer.ended() {
                return;
            }
        }
    });
}

fn start_remote(
    state: &Rc<RefCell<State>>,
    area: &GLArea,
    im: &IMMulticontext,
    connection: eitri_core::remote_project::RemoteConnection,
    size: PtySize,
    colors: TerminalColors,
    focused: bool,
) {
    let view = View { size, colors, focused };
    let plan = state.borrow_mut().begin_remote_start(view);
    let Some(plan) = plan else {
        area.queue_render();
        return;
    };
    let generation = plan.generation;
    let (tell, mut told) = futures_channel::mpsc::unbounded();
    let (wake, woken) = futures_channel::mpsc::unbounded();
    let work = plan.work.clone();
    work.run(move || {
        run_remote_start(
            plan,
            |owner, cwd, again, retry, view, wake| {
                owner.start(
                    &connection,
                    cwd,
                    again,
                    retry,
                    view.size,
                    view.colors,
                    view.focused,
                    wake,
                )
            },
            tell,
            wake,
        )
    });
    let state = Rc::downgrade(state);
    let area = area.downgrade();
    let im = im.downgrade();
    glib::spawn_future_local(async move {
        let mut woken = Some(woken);
        while let Ok(ready) = told.recv().await {
            let (Some(state), Some(area), Some(im)) = (state.upgrade(), area.upgrade(), im.upgrade()) else {
                break;
            };
            let current = {
                let s = state.borrow();
                s.generation == generation
                    && s.wanted
                    && s.remote.as_ref().is_some_and(|r| !r.cancelled.load(Ordering::SeqCst))
            };
            if !current {
                break;
            }
            match ready {
                RemoteReady::Started(session, entry, answer) => {
                    let (started, _) =
                        state
                            .borrow_mut()
                            .remote_started(session, entry, answer, view, area.is_mapped());
                    if let Some((callback, identity)) = started {
                        callback(identity);
                    }
                    if let Some(woken) = woken.take() {
                        watch_session(&state, &area, &im, woken);
                    }
                    pump(&state, &area, &im, area.is_mapped());
                }
                RemoteReady::Recorded(identity) => {
                    let started = state.borrow_mut().remote_recorded(identity, generation);
                    if let Some((callback, identity)) = started {
                        callback(identity);
                    }
                    area.queue_render();
                    break;
                }
                RemoteReady::Failed(why) => {
                    let registry = state.borrow_mut().remote_failed(&why);
                    if let Some(registry) = registry {
                        registry.notice(format!(
                            "terminal: {}",
                            eitri_core::onboarding::display::display_text(&why)
                        ));
                        if why == remote::UNCLAIMED_REFUSAL {
                            registry.offer_again();
                        }
                    }
                    area.queue_render();
                    break;
                }
            }
        }
    });
}

fn close_remote(state: &RefCell<State>, by_hand: bool) {
    let operation = {
        let mut s = state.borrow_mut();
        let Some(remote) = s.remote.as_mut() else { return };
        remote.cancelled.store(true, Ordering::SeqCst);
        remote.starting = false;
        remote.ownership_known = true;
        remote.entry = None;
        remote.retry_entry = None;
        remote.restore = false;
        // A pane closed while its window had no slot reads no saved entry when the slot arrives later.
        remote.wants_restore = false;
        remote.model = remote::Model::new(remote.registry.origin().remote().map(|p| p.host.as_str()).unwrap_or(""));
        let owner = remote.current_owner();
        let connection = match remote::source_for(&remote.registry.origin()) {
            TermSource::Server(c) => Some(c),
            _ => None,
        };
        (owner, connection, remote.work.clone(), remote.registry.clone())
    };
    let (Some(owner), connection, work, registry) = operation else {
        return;
    };
    let (tell, mut told) = futures_channel::mpsc::unbounded();
    work.run(move || {
        let _ = tell.unbounded_send(owner.close(by_hand, connection).map_err(|e| e.to_string()));
    });
    let hold = if by_hand {
        gio::Application::default().map(|app| app.hold())
    } else {
        None
    };
    glib::spawn_future_local(async move {
        let _hold = hold;
        if let Ok(answer) = told.recv().await {
            registry.report_close(answer);
        }
    });
}

/// Takes what the session publishes each time it wakes the pane, on the main loop.
fn watch_session(
    state: &Rc<RefCell<State>>,
    area: &GLArea,
    im: &IMMulticontext,
    mut woken: futures_channel::mpsc::UnboundedReceiver<()>,
) {
    let state = Rc::downgrade(state);
    let area = area.downgrade();
    let im = im.downgrade();
    // Ends when the session's thread drops the waker, i.e. when the session is over.
    glib::spawn_future_local(async move {
        while woken.recv().await.is_ok() {
            let (Some(state), Some(area), Some(im)) = (state.upgrade(), area.upgrade(), im.upgrade()) else {
                break;
            };
            let on_screen = area.is_mapped();
            pump(&state, &area, &im, on_screen);
        }
    });
}

/// A frame with one dim line on it, painted like any other: what the pane shows when it has no
/// shell to show.
fn message_frame(size: PtySize, colors: TerminalColors, message: &str) -> PaintList {
    let mut screen = Screen::new(size, colors);
    screen.feed(format!("\x1b[0;2m{message}\x1b[0m").as_bytes());
    screen.render(false)
}

/// Mapped and unmapped: hidden by `Ctrl+a t` or by a zoom of another pane. The session still reads
/// and answers while hidden; it only stops rendering (`SessionCommand::Visible`).
///
/// On the way back, what the session published while hidden is taken first, with bells unflashed
/// (phase 2): a hidden session folds a bell into its pending update without waking this pane, so
/// that bell would otherwise come out with the first frame after the show.
fn connect_visibility(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    {
        let state = state.clone();
        let im = im.clone();
        area.connect_map(move |area| {
            pump(&state, area, &im, false);
            let until = state.borrow_mut().clipboard_notice.show(Instant::now());
            if let Some(until) = until {
                repaint_at(area, until);
            }
            state.borrow().send_command(SessionCommand::Visible(true));
        });
    }
    let state = state.clone();
    area.connect_unmap(move |_| {
        state.borrow().send_command(SessionCommand::Visible(false));
    });
}

/// Takes what the session published: the frame and where its cursor is, how the shell ended, a copy
/// for the desktop, a bell. `on_screen: false` takes a bell without flashing it. After the borrow,
/// because these are calls into GTK: the input method is pointed again when the cursor moved under
/// the keys, and the copies go onto the desktop. The flash-end timer is armed for
/// [`bell::repaint_delay`], not the raw remainder, so it never fires before the flash is actually
/// over. **Known limit:** the clipboard calls, the input method and the timer's own firing are GTK's
/// and are not tested without a display; the decisions are (`State::take_cursor`, `State::bell`,
/// `State::notice_on_update`, `BellFlash`, `bell::repaint_delay`, and the engine's OSC 52 tests).
fn pump(state: &RefCell<State>, area: &GLArea, im: &IMMulticontext, on_screen: bool) {
    let Some(update) = state.borrow().session.as_ref().map(TerminalSession::take_update) else {
        return;
    };
    let (repoint, flash_until, notice_until, closed_by_exit, lost_notice) = {
        let mut s = state.borrow_mut();
        let mut repoint = false;
        let mut closed_by_exit = None;
        if let Some(frame) = update.frame {
            s.frame = Some(frame);
            repoint = s.take_cursor(update.cursor);
            area.queue_render();
        }
        // Independent of `update.frame` (mouse reporting): a synchronized update
        // can hold every frame back for up to 150ms while the mode changes underneath it, and
        // `pointer::route`/`wheel_route` must see the live mode the moment it changes, not only
        // when the next frame happens to render.
        if let Some(m) = update.mouse {
            s.mouse = m;
        }
        if let Some(mut report) = update.mirror {
            // Clipboard reads and parked keys belong to the connection on which they began. A new snapshot
            // also clears them, covering a whole disconnect/reconnect folded into one UI update.
            s.connection_changed(&mut report);
            s.remote_report(report);
            area.queue_render();
        }
        if let Some(exit) = update.exited {
            s.remote_exit(exit);
            println!("[terminal] {}", exit.notice());
            if s.remote.is_none() && super::closes_on_exit(&exit) {
                closed_by_exit = s.on_shell_exit.clone().map(|callback| (callback, s.generation));
            }
        }
        let (notice_until, lost) =
            s.notice_on_update(&update.events, update.exited.as_ref(), on_screen, Instant::now());
        // The close below forgets the pane's notice: what it holds goes to the window instead.
        let lost_notice = closed_by_exit.as_ref().and(s.on_notice_lost.clone()).zip(lost);
        if update.events.clipboard_bytes.is_some() || update.events.primary_bytes.is_some() {
            area.queue_render();
        }
        let flash_until = if update.events.bell {
            s.bell(on_screen, Instant::now())
        } else {
            None
        };
        (repoint, flash_until, notice_until, closed_by_exit, lost_notice)
    };
    if repoint {
        point_input_method(im, state, area);
    }
    // A copy lands whether the pane is shown or not; a read of either clipboard never gets this far
    // (`Term` refuses it, `Osc52::OnlyCopy`).
    if let Some(text) = update.events.clipboard {
        crate::host_clipboard::write_text(&area.clipboard(), text);
    }
    if let Some(text) = update.events.primary {
        crate::host_clipboard::write_text(&area.primary_clipboard(), text);
    }
    // A finished host selection copies on release to the clipboard AND the primary selection
    // (foot's own `selection-target=clipboard` plus every X/Wayland
    // terminal's own primary-selection convention) -- unlike the two OSC 52 copies above, this one
    // is also kept (`State::last_selection`) for `Ctrl+Shift+C` to copy again with no session round
    // trip (`copy_selection`'s own doc).
    if let Some(text) = update.events.selection {
        crate::host_clipboard::write_text(&area.clipboard(), text.clone());
        crate::host_clipboard::write_text(&area.primary_clipboard(), text.clone());
        println!(
            "[terminal] a selection finished: copied {} characters to the clipboard and the primary selection",
            text.chars().count()
        );
        state.borrow_mut().last_selection = Some(text);
    }
    for until in flash_until.into_iter().chain(notice_until) {
        repaint_at(area, until);
    }
    // `update.events.title` has nowhere to go: the title rule gives it to the pane's header "when one
    // exists", and the terminal has no header yet.

    // Last, with nothing borrowed: the hook kills the module, unmapping this pane, whose handler
    // borrows the state. Then the pane forgets the shell, as `TerminalPane::close` does.
    if let Some((say, text)) = lost_notice {
        say(text);
    }
    if let Some((closed_by_exit, generation)) = closed_by_exit {
        if closed_by_exit(generation) && super::exit_is_current(generation, state.borrow().generation) {
            state.borrow_mut().close();
            im.reset();
            area.queue_render();
        }
    }
}

/// Both transient overlays expire through the same nonblocking repaint mechanism.
fn repaint_at(area: &GLArea, until: Instant) {
    area.queue_render();
    let area = area.downgrade();
    // Round up to a whole millisecond: glib truncation could repaint while an overlay is still
    // active and then leave it visible until the next unrelated frame.
    glib::timeout_add_local_once(bell::repaint_delay(until, Instant::now()), move || {
        if let Some(area) = area.upgrade() {
            area.queue_render();
        }
    });
}

/// Enter after the shell exited: a fresh session on a fresh PTY, in the same place.
fn restart(state: &Rc<RefCell<State>>, area: &GLArea, im: &IMMulticontext) {
    state.borrow_mut().reset_for_restart();
    ensure_session(state, area, im);
}

fn connect_resize(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    let state = state.clone();
    let im = im.clone();
    area.connect_resize(move |area, width, height| {
        // A hidden or unallocated pane can report no area. Do not send that size to the PTY:
        // the terminal keeps its last size while hidden.
        if width <= 0 || height <= 0 {
            return;
        }
        let allocation = Allocation {
            width: width as f32,
            height: height as f32,
            scale: area.scale_factor() as f32,
        };
        let resized = {
            let mut s = state.borrow_mut();
            if let Some(skia) = s.skia.as_mut() {
                skia.resize(width, height);
            }
            s.allocate(allocation)
        };
        area.queue_render();
        if let Some(size) = resized {
            state.borrow().send_command(SessionCommand::Resize(size));
        }
        ensure_session(&state, area, &im);
    });
}

/// `connect_resize` above already rescales
/// correctly on a scale change (`State::allocate` -> `TerminalMetrics::rescale`; cols/rows are
/// unchanged, e.g. 160x13 in the sandbox, since the scale-only case never changes the logical
/// size). What is missing is a reason for GTK to *emit* that `resize` at all: it fires only on the
/// `GLArea`'s next snapshot, and nothing here asks for one while the pane is idle, so the old
/// framebuffer sits on screen upscaled (blurry) until the next output redraws it. `queue_render`
/// asks for that snapshot.
fn connect_scale_change(area: &GLArea) {
    area.connect_scale_factor_notify(gtk4::prelude::GLAreaExt::queue_render);
}

fn connect_render(area: &GLArea, state: &Rc<RefCell<State>>) {
    let state = state.clone();
    area.connect_render(move |area, _ctx| {
        let mut s = state.borrow_mut();
        if s.skia.is_none() {
            s.skia = SkiaState::new();
            let (width, height) = (area.width() * area.scale_factor(), area.height() * area.scale_factor());
            if let Some(skia) = s.skia.as_mut() {
                skia.resize(width, height);
            }
        }
        // Laid out before the fields are borrowed apart: it reads the frame, the cursor and the
        // colours together.
        let preedit = s.preedit_layout();
        let now = Instant::now();
        let flashing = s.bell.active(now);
        let clipboard_notice = s.metrics.as_ref().map(|metrics| {
            s.clipboard_notice
                .ops(now, s.number, metrics.cols(), metrics.rows(), s.colors)
        });
        let remote_ops = s.remote.as_ref().zip(s.metrics.as_ref()).map(|(remote, metrics)| {
            let mut ops = Vec::new();
            if let Some(text) = remote.model.overlay(now).or_else(|| remote.model.history.clone()) {
                ops.extend(remote::notice_ops(&text, 0, metrics.cols(), s.colors));
            }
            if let Some(text) = remote.model.notice() {
                ops.extend(remote::notice_ops(
                    &text,
                    remote
                        .model
                        .ending_row(s.cursor.map(|cursor| cursor.row), metrics.rows()),
                    metrics.cols(),
                    s.colors,
                ));
            }
            ops
        });
        let State {
            skia,
            metrics,
            frame,
            colors,
            ..
        } = &mut *s;
        let Some(skia) = skia.as_mut() else {
            return glib::Propagation::Stop;
        };
        // MUST be first: see `SkiaState::invalidate_cached_gl_state`.
        skia.invalidate_cached_gl_state();
        skia.ensure_surface();
        let Some(surface) = skia.surface.as_mut() else {
            return glib::Propagation::Stop;
        };
        match frame.as_ref().zip(metrics.as_ref()) {
            Some((list, metrics)) => {
                eitri_terminal::paint(surface.canvas(), list, metrics);
                // After the frame, over it: the input method's composition at the cursor (phase 2).
                if let Some(preedit) = &preedit {
                    eitri_terminal::paint_ops(surface.canvas(), &preedit.ops, metrics);
                }
                // Over everything, for a moment: the bell (phase 2).
                if flashing {
                    bell::tint(surface.canvas(), colors.foreground);
                }
                if let Some(ops) = &clipboard_notice {
                    eitri_terminal::paint_ops(surface.canvas(), ops, metrics);
                }
            }
            // No frame yet (or no metrics: the terminal has never been started), the theme's
            // background, never GTK's default or black.
            None => {
                let bg = colors.background;
                surface.canvas().clear(skia_safe::Color::from_rgb(bg.r, bg.g, bg.b));
            }
        }
        if let Some((ops, metrics)) = remote_ops.as_ref().zip(metrics.as_ref()) {
            eitri_terminal::paint_ops(surface.canvas(), ops, metrics);
        }
        let submitted = skia.gr_context.flush_and_submit();
        if let Err(error) = &submitted {
            // The frame is lost; the next draw paints a whole one again. Said once a process, since
            // a context that fails once tends to fail every frame.
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| eprintln!("[terminal] skia flush failed: {error:?}"));
        }
        let gone = s.remote.as_mut().filter(|_| submitted.is_ok()).and_then(|remote| {
            if matches!(
                remote.model.ending,
                Some(remote::Ending::Ended(eitri_terminal::mirror::MirrorEnd::Gone(_)))
            ) {
                remote
                    .current_owner()
                    .zip(remote.entry.take())
                    .map(|(owner, entry)| (remote.work.clone(), owner, entry))
            } else {
                None
            }
        });
        drop(s);
        if let Some((work, owner, entry)) = gone {
            work.run(move || {
                if let Err(error) = owner.show_gone(&entry) {
                    eprintln!("[terminal] could not forget the gone terminal: {error}");
                }
            });
        }
        glib::Propagation::Stop
    });
}

/// Clicking the pane focuses it (GTK4 does not do this for a plain widget; the frozen pane's own
/// doc records the editor losing click-focus the day a second focusable pane appeared), the primary
/// button also drags a selection out of it, and the middle
/// button pastes the primary selection -- foot's and alacritty's own convention. **Any of that is
/// instead a mouse report when the program's own mode asks for one and
/// `Shift` is not held** (`pointer::route`: Shift always selects): what changed
/// is which of two already-connected controllers acts, never a new one for reporting.
///
/// **Two controllers on the primary button, not one**, because they answer different questions:
/// `GestureClick` is the only one of the two GTK tells the click COUNT (`n_press`), which
/// `pointer::kind_for` needs and `GestureDrag` has no notion of; `GestureDrag` is the only one GTK
/// tells the drag's motion and release. Both attached to the same widget and button see the same
/// press, since neither claims the event sequence away from the other (`set_state` is never
/// called): the click gesture starts the selection at `pressed`, and the drag gesture extends and
/// finishes it as the button moves and lifts. A plain click with no motion still fires
/// `drag-end` at zero offset -- `Finish` on a selection that never left its single starting cell,
/// which for `Simple` is empty (`Term::selection_to_string` reports `None`, same as never having
/// selected anything) and for `Word`/`Line` is exactly the click's own word or line, no drag
/// required (`select.rs`'s own tests already prove `Start` + `Finish` alone is enough for those two).
/// **Known limit:** whether GTK really dispatches to both controllers rather than one claiming the
/// sequence away from the other is GTK's own runtime behaviour and is not tested without a display
/// (the same limit `paste_clipboard`'s own doc already names for the clipboard read and its timer).
///
/// **The drag gesture is widened to button 0** (every button, not only the primary): a `GestureClick`
/// stops delivering `released` once the pointer crosses the drag threshold, so it cannot be a
/// press/release pair's own source once a real drag is possible -- `GestureDrag`'s `drag-begin` and
/// `drag-end` always fire, at zero offset for a plain click. Its own selection code (button 1,
/// `Shift` not held) is unchanged; every other case (a report, or a local button 2/3 with nothing
/// defined for it) is new here and does nothing to the selection path.
fn connect_pointer(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    let focus_click = GestureClick::new();
    focus_click.set_button(0);
    {
        let target = area.clone();
        focus_click.connect_pressed(move |_, _, _, _| {
            if !target.has_focus() {
                target.grab_focus();
            }
        });
    }
    area.add_controller(focus_click);

    let select_click = GestureClick::new();
    select_click.set_button(BUTTON_PRIMARY);
    {
        let state = state.clone();
        select_click.connect_pressed(move |gesture, n_press, x, y| {
            let modes = state.borrow().routing_modes();
            if pointer::route(shift_held(gesture), modes) == pointer::PointerRoute::Program {
                return; // the widened drag gesture below reports this press instead.
            }
            send_select(&state, SelectStage::Start(n_press), x, y);
        });
    }
    area.add_controller(select_click);

    let drag = GestureDrag::new();
    drag.set_button(0);
    {
        let state = state.clone();
        drag.connect_drag_begin(move |gesture, x, y| {
            let modes = state.borrow().routing_modes();
            let shift = shift_held(gesture);
            if pointer::route(shift, modes) == pointer::PointerRoute::Program {
                if let Some(button) = mouse_button(gesture.current_button()) {
                    report_press(&state, button, gesture.current_event_state(), x, y);
                }
            }
            // A local button 1 press is `select_click`'s own `pressed`, above; a local button 2/3
            // press has nothing defined for it here (button 2's paste is `middle_click`'s own).
        });
    }
    {
        let state = state.clone();
        drag.connect_drag_update(move |gesture, offset_x, offset_y| {
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let (x, y) = (start_x + offset_x, start_y + offset_y);
            let modes = state.borrow().routing_modes();
            let shift = shift_held(gesture);
            if pointer::route(shift, modes) == pointer::PointerRoute::Program {
                report_motion(&state, gesture.current_event_state(), x, y);
                return;
            }
            if gesture.current_button() == BUTTON_PRIMARY {
                send_select(&state, SelectStage::Extend, x, y);
            }
        });
    }
    {
        let state = state.clone();
        drag.connect_drag_end(move |gesture, offset_x, offset_y| {
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let (x, y) = (start_x + offset_x, start_y + offset_y);
            // Decide from `reports.is_held()`, never from `route()`
            // sampled at release. `route` re-derives its answer from *current* Shift state with no
            // memory of press time, so gating the release report on `route(shift, modes) ==
            // Program` (the old code) missed a release with Shift held only at that moment --
            // `report_release` was skipped, `ReportTracker::held` stayed stranded, and the next
            // ordinary motion reported a phantom left-button drag (`is_held`'s own doc,
            // `pointer::tests::a_release_reports_even_when_shift_is_held_only_at_release`). A button
            // `report_press` reported is real regardless of the Shift state at release (xterm's own
            // convention: the release follows the press it matches), so `report_release` runs
            // unconditionally here too -- it is already a no-op if nothing is held, the same fact
            // `connect_cancel` below already relies on.
            let was_held = state.borrow().reports.is_held();
            report_release(&state, gesture.current_event_state(), x, y);
            if was_held {
                return;
            }
            if gesture.current_button() == BUTTON_PRIMARY {
                send_select(&state, SelectStage::Extend, x, y);
                state
                    .borrow()
                    .send_command(SessionCommand::Select(SelectCommand::Finish));
            }
        });
    }
    {
        let state = state.clone();
        // A sequence cancelled (another widget claimed it, a touch was cancelled) fires no
        // `drag-end`. A button `report_press` reported and never released would leave the program
        // thinking it is still held down; `report_release` itself is a no-op if nothing is held
        // (Local route, or nothing was ever pressed), so this is safe to call unconditionally.
        drag.connect_cancel(move |_, _| {
            let (x, y) = state.borrow().last_pointer;
            report_release(&state, ModifierType::empty(), x, y);
        });
    }
    area.add_controller(drag);

    let middle_click = GestureClick::new();
    middle_click.set_button(BUTTON_MIDDLE);
    {
        let state = state.clone();
        let area = area.clone();
        let im = im.clone();
        middle_click.connect_pressed(move |gesture, _, _, _| {
            let modes = state.borrow().routing_modes();
            if pointer::route(shift_held(gesture), modes) == pointer::PointerRoute::Program {
                return; // the widened drag gesture above reports this press instead.
            }
            paste_primary(&state, &area, &im);
        });
    }
    area.add_controller(middle_click);

    connect_pointer_motion(area, state);
}

/// Whether `Shift` is held on the event a controller is currently handling
/// (`EventControllerExt::current_event_state`, which every gesture below inherits).
fn shift_held(controller: &impl IsA<gtk4::EventController>) -> bool {
    controller.current_event_state().contains(ModifierType::SHIFT_MASK)
}

/// GDK's button numbering (1/2/3) to [`MouseButton`]; any other button (4/5, the wheel's own
/// "buttons" on some setups; a stylus button) is not reportable.
fn mouse_button(gdk_button: u32) -> Option<MouseButton> {
    match gdk_button {
        1 => Some(MouseButton::Left),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::Right),
        _ => None,
    }
}

fn mouse_mods(modifiers: ModifierType) -> MouseMods {
    MouseMods {
        shift: modifiers.contains(ModifierType::SHIFT_MASK),
        alt: modifiers.contains(ModifierType::ALT_MASK),
        ctrl: modifiers.contains(ModifierType::CONTROL_MASK),
    }
}

/// A widget-relative pixel turned into the absolute (line, column) cell a mouse report names --
/// [`selection_geometry`]'s own mapping, reused rather than reimplemented (`pointer.rs`'s
/// own module doc). `None` before a shell has ever spawned or rendered once.
fn report_cell(state: &State, x: f64, y: f64) -> Option<(i32, u16)> {
    let (cell_w, cell_h, cols, rows, top_line) = selection_geometry(state)?;
    let (line, col, _) = pointer::cell_at(x, y, cell_w, cell_h, cols, rows, top_line);
    Some((line, col))
}

/// A press at `(x, y)`: turned into a cell, fed to [`pointer::ReportTracker::press`], and sent
/// through the pane's one door. Dropped with no session/metrics/frame yet, or a scrollback cell
/// (`ReportTracker`'s own doc).
fn report_press(state: &Rc<RefCell<State>>, button: MouseButton, modifiers: ModifierType, x: f64, y: f64) {
    let mut s = state.borrow_mut();
    let Some(cell) = report_cell(&s, x, y) else { return };
    let Some(mut input) = s.reports.press(button, cell) else {
        return;
    };
    input.mods = mouse_mods(modifiers);
    drop(s);
    submit(state, PaneInput::Mouse(input));
}

/// A motion to `(x, y)`, deduped by cell (`ReportTracker`'s own doc): fed from both the drag
/// gesture's own update and the plain motion controller below, which is harmless (design decision,
/// spec Phase 3).
fn report_motion(state: &Rc<RefCell<State>>, modifiers: ModifierType, x: f64, y: f64) {
    let mut s = state.borrow_mut();
    let Some(cell) = report_cell(&s, x, y) else { return };
    let Some(mut input) = s.reports.motion(cell) else {
        return;
    };
    input.mods = mouse_mods(modifiers);
    drop(s);
    submit(state, PaneInput::Mouse(input));
}

/// A release at `(x, y)`: `None` from [`pointer::ReportTracker::release`] with no button held (a
/// gesture that started before the mode turned on) is silently dropped, same as a press/motion with
/// no geometry yet.
fn report_release(state: &Rc<RefCell<State>>, modifiers: ModifierType, x: f64, y: f64) {
    let mut s = state.borrow_mut();
    let Some(cell) = report_cell(&s, x, y) else { return };
    let Some(mut input) = s.reports.release(cell) else {
        return;
    };
    input.mods = mouse_mods(modifiers);
    drop(s);
    submit(state, PaneInput::Mouse(input));
}

/// Hover motion (no button held): `MOUSE_MOTION` (1003) reporting, and the pointer position wheel
/// events need but GDK does not carry on a scroll event (`connect_scroll`'s own doc). Its own report
/// is filtered by `encode_mouse` against the live mode exactly like the drag gesture's -- `wants`
/// does not accept `Motion(None)` under 1002 alone, only 1003 (`mouse.rs`'s own tests).
fn connect_pointer_motion(area: &GLArea, state: &Rc<RefCell<State>>) {
    let motion = gtk4::EventControllerMotion::new();
    {
        let state = state.clone();
        motion.connect_enter(move |controller, x, y| {
            state.borrow_mut().last_pointer = (x, y);
            let modes = state.borrow().routing_modes();
            if pointer::route(shift_held(controller), modes) == pointer::PointerRoute::Program {
                report_motion(&state, controller.current_event_state(), x, y);
            }
        });
    }
    {
        let state = state.clone();
        motion.connect_motion(move |controller, x, y| {
            state.borrow_mut().last_pointer = (x, y);
            let modes = state.borrow().routing_modes();
            if pointer::route(shift_held(controller), modes) == pointer::PointerRoute::Program {
                report_motion(&state, controller.current_event_state(), x, y);
            }
        });
    }
    area.add_controller(motion);
}

/// Which half of a `Start`/`Extend` pair [`send_select`] is sending: `Start` carries the click count
/// `pointer::kind_for` turns into a [`eitri_terminal::SelectKind`], `Extend` carries none.
enum SelectStage {
    Start(i32),
    Extend,
}

/// The geometry a pointer position needs to become a [`SelectCommand`]: the cell size and grid the
/// pane's own metrics are built at, and the absolute top line its LAST FRAME reported
/// (`PaintList::top_line`, provenance-only for painting but exactly the correlation selection needs --
/// the host has no other way to learn `Screen::top_line`, which lives on the session's own thread).
/// `None` before a shell has ever spawned (no metrics yet) or rendered once (no frame yet).
///
/// The cell size is returned in **widget-logical** pixels, the unit GTK's gesture and motion
/// controllers report `(x, y)` in. The metrics themselves are in device pixels (the font is built at
/// `size_pt * scale`, the GLArea's allocation is device pixels), so they are divided by
/// [`TerminalMetrics::scale`] here -- the same conversion [`im_cursor_rect`] makes the other way.
/// Without it, at GTK scale 2 (which includes 125% and 150% outputs, since GTK ceils) a click landed
/// on about half its row and column.
fn selection_geometry(state: &State) -> Option<(f64, f64, u16, u16, i32)> {
    let metrics = state.metrics.as_ref()?;
    let top_line = state.frame.as_ref().map_or(0, |f| f.top_line);
    let scale = f64::from(metrics.scale()).max(1.0);
    Some((
        f64::from(metrics.cell_width()) / scale,
        f64::from(metrics.cell_height()) / scale,
        metrics.cols(),
        metrics.rows(),
        top_line,
    ))
}

/// Turns one pointer position into a [`SelectCommand`] and sends it, dropped with no session or no
/// metrics yet (nothing to select against).
fn send_select(state: &Rc<RefCell<State>>, stage: SelectStage, x: f64, y: f64) {
    let s = state.borrow();
    let Some(session) = &s.session else { return };
    let Some((cell_w, cell_h, cols, rows, top_line)) = selection_geometry(&s) else {
        return;
    };
    let (line, col, right_half) = pointer::cell_at(x, y, cell_w, cell_h, cols, rows, top_line);
    let cmd = match stage {
        SelectStage::Start(n_press) => SelectCommand::Start {
            line,
            col,
            right_half,
            kind: pointer::kind_for(n_press),
        },
        SelectStage::Extend => SelectCommand::Extend { line, col, right_half },
    };
    session.send(SessionCommand::Select(cmd));
}

/// `Ctrl+Shift+C`: copies the pane's last finished selection to the clipboard again. Reads `State::last_selection` -- what a drag's own release already put there
/// (`pump`'s `events.selection` handling) -- rather than asking the session for `SelectCommand::Finish`
/// again: that would need a round trip through its update channel for no benefit, since the session
/// already told the host once and the text has not changed since (its own doc: "the session is the
/// single source either way"). With nothing selected, this is a no-op but for the log line: the key
/// is still swallowed either way (forwarding it would send `terminal_input`'s own encoding of
/// `Ctrl+Shift+C` -- `keys::clipboard_chord`'s own doc explains why that must never reach the child).
fn copy_selection(state: &Rc<RefCell<State>>, area: &GLArea) {
    let Some(text) = state.borrow().last_selection.clone() else {
        println!("[terminal] Ctrl+Shift+C: nothing is selected");
        return;
    };
    crate::host_clipboard::write_text(&area.clipboard(), text.clone());
    println!(
        "[terminal] Ctrl+Shift+C: copied {} characters to the clipboard",
        text.chars().count()
    );
}

/// How many grid lines one wheel notch scrolls (3 lines a notch,
/// foot/alacritty's own `multiplier`) -- the same multiplier
/// `eitri_terminal::mouse::LINES_PER_NOTCH` uses for alternate-scroll's own arrow-key repeats,
/// so the two conventions cannot drift apart.
const LINES_PER_WHEEL_NOTCH: i32 = eitri_terminal::mouse::LINES_PER_NOTCH as i32;

/// One scroll event, in `unit`, folded into `wheel`'s carry-over state -- the same accumulator
/// `crate::wheel_zoom::WheelZoom` uses for Ctrl+wheel text size, reused rather than reimplemented so
/// a slow touchpad gesture and a reversal mid-scroll behave identically here (its own module doc and
/// tests already cover both). How many whole notches this event completed, positive up
/// (`WheelZoom`'s own "up is larger" convention, `dy` negative -- "up is into history" for
/// `scroll_request` below, "up" for a mouse-mode wheel report either way). **Called at most once
/// per scroll event**: `WheelZoom::scroll_in` is stateful, and calling it twice for one event (once
/// for a report, once for the scrollback) would double-count it.
fn wheel_notches(wheel: &mut WheelZoom, unit: gtk4::gdk::ScrollUnit, dy: f64) -> i32 {
    wheel.scroll_in(unit, dy)
}

/// `notches` (as [`wheel_notches`] returns them) turned into what the scrollback asks for.
/// `None` until a whole notch completes. `WheelZoom::scroll_in`'s positive-is-up convention
/// multiplies straight through with no sign flip of its own ("up is into history" here).
fn scroll_request(notches: i32) -> Option<ScrollRequest> {
    (notches != 0).then_some(ScrollRequest::Lines(notches * LINES_PER_WHEEL_NOTCH))
}

/// A plain (Ctrl-less) vertical scroll over the pane: `Shift+PageUp`'s wheel equivalent
/// (bottom-terminal phase 3a). No `KINETIC` flag -- a kinetic fling would keep delivering scroll
/// events after the gesture ends, spending notches the user's hand never asked for. Ctrl+wheel
/// never reaches this controller: `crate::wheel_zoom::install`'s own controller is capture-phase on
/// the module host (outside this widget) and claims Ctrl+wheel first, for the pane's text size.
/// **A plain wheel notch is not always the pane's own scrollback**:
/// `pointer::wheel_route` decides between a report (the program's own mouse mode), the alternate
/// screen's arrow-key convention (`less`/`man`, `ALTERNATE_SCROLL`), and the scrollback, and
/// only the session thread can tell a report from arrows apart (it needs the live `APP_CURSOR` bit,
/// `mouse::alternate_scroll`'s own doc) -- so both ride `SessionCommand::Mouse(Wheel)` and the
/// session picks the bytes; only `Scrollback` still sends `SessionCommand::Scroll` directly. One
/// report per NOTCH, not per line (xterm/foot send one button-4/5 event per notch, and nvim's
/// default `mousescroll` already multiplies -- three would scroll nine).
fn connect_scroll(area: &GLArea, state: &Rc<RefCell<State>>) {
    let controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    let wheel = RefCell::new(WheelZoom::default());
    let state = state.clone();
    controller.connect_scroll(move |controller, _dx, dy| {
        let notches = wheel_notches(&mut wheel.borrow_mut(), controller.unit(), dy);
        if notches != 0 {
            let modes = state.borrow().routing_modes();
            match pointer::wheel_route(shift_held(controller), modes) {
                pointer::WheelRoute::Scrollback => {
                    if let Some(req) = scroll_request(notches) {
                        state.borrow().send_command(SessionCommand::Scroll(req));
                    }
                }
                pointer::WheelRoute::Report | pointer::WheelRoute::Arrows => {
                    let dir = if notches > 0 { WheelDir::Up } else { WheelDir::Down };
                    let (x, y) = state.borrow().last_pointer;
                    let modifiers = controller.current_event_state();
                    for _ in 0..notches.unsigned_abs() {
                        report_wheel(&state, dir, modifiers, x, y);
                    }
                }
            }
        }
        glib::Propagation::Stop
    });
    area.add_controller(controller);
}

/// A wheel notch: unlike a press/release/motion, a wheel report needs no [`pointer::ReportTracker`]
/// state (it is never deduped and carries no held button) -- just the current cell, dropped the
/// same way a scrollback cell is (`pointer::mouse_input`'s own doc).
fn report_wheel(state: &Rc<RefCell<State>>, dir: WheelDir, modifiers: ModifierType, x: f64, y: f64) {
    let s = state.borrow();
    let Some(cell) = report_cell(&s, x, y) else { return };
    drop(s);
    let Some(mut input) = pointer::mouse_input(MouseKind::Wheel(dir), cell) else {
        return;
    };
    input.mods = mouse_mods(modifiers);
    submit(state, PaneInput::Mouse(input));
}

fn connect_keyboard(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    let keys = EventControllerKey::new();
    // GTK runs the input method on every key before `key-pressed`/`key-released` below, and a key it
    // consumes never reaches them (phase 2; the editor's own wiring, `neovide-editor/src/keyboard.rs`).
    // Eitri's own chords never get this far: they are taken in the capture phase.
    keys.set_im_context(Some(im));
    {
        let state = state.clone();
        let im = im.clone();
        keys.connect_key_pressed(move |controller, keyval, keycode, modifier| {
            let Some(area) = controller.widget().and_downcast::<GLArea>() else {
                return glib::Propagation::Stop;
            };
            let repeat = state.borrow_mut().repeats.press(keyval);
            if state.borrow().exited {
                if keys::restarts(keyval, modifier) {
                    restart(&state, &area, &im);
                }
                return glib::Propagation::Stop;
            }
            // Copy mode, ahead of `scroll_chord`: a copy-mode key never reaches
            // the shell, whether or not it is also one of `scroll_chord`'s own bindings.
            // Only the keys the input method passes on get here (the arrows, the page keys, `Esc`,
            // the `Ctrl` chords); every printable one arrives as a commit (`route_commit`).
            if state.borrow().copy_mode {
                let key = keys::copy_mode_key(keyval, modifier);
                state.borrow_mut().apply_copy_mode_key(key);
                return glib::Propagation::Stop;
            }
            if let Some(req) = keys::scroll_chord(keyval, modifier) {
                state.borrow().send_command(SessionCommand::Scroll(req));
                // Never forwarded to the child: `Shift+PageUp`/`Shift+PageDown`/`Shift+Home`/
                // `Shift+End` are this pane's own scrollback chords (bottom-terminal phase 3a).
                return glib::Propagation::Stop;
            }
            match keys::clipboard_chord(keyval, modifier) {
                Some(ClipboardChord::Copy) => {
                    copy_selection(&state, &area);
                    return glib::Propagation::Stop;
                }
                Some(ClipboardChord::Paste) => {
                    paste_clipboard(&state, &area, &im);
                    return glib::Propagation::Stop;
                }
                None => {}
            }
            // Recorded before delivery: a release is forwarded only for a keyval whose press
            // reached here, never re-derived from the release's own
            // modifier state, which can have changed (Shift let go before a clipboard chord's
            // letter, for one).
            state.borrow_mut().delivered.mark(keyval);
            deliver(&state, raw_key(&area, keyval, keycode, modifier, true, repeat));
            // Claimed unconditionally: every key that reaches the terminal is the child's. The keys
            // Eitri keeps were taken before this controller ever saw them.
            glib::Propagation::Stop
        });
    }
    {
        let state = state.clone();
        keys.connect_key_released(move |controller, keyval, keycode, modifier| {
            state.borrow_mut().repeats.release(keyval);
            // A release reaches this pane for a keyval whose press never did -- the key that moved
            // focus in, the real key behind a `Ctrl+a Ctrl+a`/`Ctrl+a Ctrl+l` literal, a HINT label.
            // Forward only what was actually delivered.
            let was_delivered = state.borrow_mut().delivered.take(keyval);
            if !was_delivered {
                return;
            }
            let Some(area) = controller.widget().and_downcast::<GLArea>() else {
                return;
            };
            if !state.borrow().exited {
                deliver(&state, raw_key(&area, keyval, keycode, modifier, false, false));
            }
        });
    }
    area.add_controller(keys);

    // Focus-out must clear the held key: its release goes to whichever widget gets focus next.
    let focus = EventControllerFocus::new();
    {
        let state = state.clone();
        focus.connect_leave(move |_| {
            let mut s = state.borrow_mut();
            s.repeats.reset();
            s.delivered.reset();
        });
    }
    area.add_controller(focus);
}

/// Throws away a composition in progress rather than letting it commit: a paste or a `Ctrl+a`
/// literal is about to reach the shell, and half-typed pinyin must neither follow it nor precede it.
/// What the input method commits out of this reset is dropped (`ImeGate::begin_reset`). Nothing to
/// do, and no call into the input method, when nothing is being composed.
fn discard_composition(state: &RefCell<State>, im: &IMMulticontext, area: &GLArea) {
    {
        let mut s = state.borrow_mut();
        if !s.ime.composing() {
            return;
        }
        s.ime.begin_reset();
    }
    println!("[terminal] a composition in progress was discarded");
    im.reset();
    state.borrow_mut().ime.end_reset();
    area.queue_render();
}

/// The input method's news (`ime.rs`): a commit goes through the gate to [`submit`]; a composition
/// that starts, changes or ends is kept, redrawn, and the input method told where its caret is.
/// **Known limit:** these handlers and the GTK calls they make are not tested without a display;
/// what they decide with is (`ImeGate`, `State::preedit_layout`, `State::ime_caret`,
/// `State::ime_rect`, `State::take_cursor`, `im_cursor_rect`).
fn connect_ime(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    {
        let state = state.clone();
        let area = area.downgrade();
        im.connect_commit(move |_, text| {
            // GTK focuses the input method out from inside a focus change, before the focus
            // report says so, and fcitx5-gtk commits its composition right there (`ime.rs`'s module
            // doc). GTK's own state is already updated by then, so ask it, not only the gate. Keep
            // BOTH halves (`GtkFocus`'s doc): a focus move clears `has_focus` before that commit,
            // but alt-tab clears only `is_active` -- `has_focus` alone would type `ni hao` there.
            let gtk = area.upgrade().map_or(GtkFocus::default(), |area| GtkFocus {
                widget_has_focus: area.has_focus(),
                window_is_active: area
                    .root()
                    .and_downcast::<gtk4::Window>()
                    .is_some_and(|window| window.is_active()),
            });
            let route = state.borrow_mut().route_commit(text, gtk);
            match route {
                CommitRoute::Shell(input) => submit(&state, PaneInput::Key(input)),
                CommitRoute::CopyMode(key) => state.borrow_mut().apply_copy_mode_key(key),
                CommitRoute::Dropped(why) => println!(
                    "[terminal] an input-method commit ({} characters) was dropped: {why}",
                    text.chars().count()
                ),
            }
            // On every commit, plain keys included, not only when a composition was drawn: this
            // paint stops GTK's frame clock until the compositor's frame callback, and the thaw
            // paints the echo at once. Skipping it leaves the echo to GTK's hold one period after
            // the key's cycle, which is no sooner (the module doc).
            if let Some(area) = area.upgrade() {
                area.queue_render();
            }
        });
    }
    let on_preedit = {
        let state = state.clone();
        let area = area.downgrade();
        move |im: &IMMulticontext| {
            if let Some(area) = area.upgrade() {
                preedit_changed(im, &state, &area);
            }
        }
    };
    im.connect_preedit_start(on_preedit.clone());
    im.connect_preedit_changed(on_preedit.clone());
    im.connect_preedit_end(on_preedit);
}

/// A composition started, changed or ended: keep it, redraw it, and point the input method at its
/// caret. On all three, as the editor does: `start` so the candidate window is in place, `changed`
/// so it follows the composition, `end` so the next one never starts against a stale place. The
/// preedit's pango attributes (rime's `HighLight` on the segment being converted) are not drawn.
///
/// In copy mode a composition is never kept ([`State::take_preedit`]): it is reset, from an idle
/// callback rather than from inside this signal -- fcitx5-gtk emits `preedit-start` from inside its
/// own preedit update and would re-enter it (`fcitximcontext.cpp`'s
/// `_fcitx_im_context_update_formatted_preedit_cb`). Whatever that reset commits is dropped by the
/// gate (`ImeGate::begin_reset`), and a commit before it is a copy-mode key or nothing.
fn preedit_changed(im: &IMMulticontext, state: &Rc<RefCell<State>>, area: &GLArea) {
    let (text, _attributes, caret) = im.preedit_string();
    let discard = state.borrow_mut().take_preedit(&text, caret);
    if discard {
        let im = im.clone();
        let state = state.clone();
        glib::idle_add_local_once(move || {
            state.borrow_mut().ime.begin_reset();
            im.reset();
            state.borrow_mut().ime.end_reset();
        });
    }
    point_input_method(im, state, area);
    area.queue_render();
}

/// Tells the input method where its candidate window belongs ([`State::ime_rect`]); nothing while
/// that is unknown. fcitx5-gtk skips a rectangle it already has (`fcitximcontext.cpp:827-832`), so
/// calling this more often than needed costs nothing there.
fn point_input_method(im: &IMMulticontext, state: &RefCell<State>, area: &GLArea) {
    let rect = state.borrow().ime_rect(area.scale_factor());
    if let Some(rect) = rect {
        im.set_cursor_location(&rect);
    }
}

fn raw_key(
    area: &GLArea,
    keyval: gtk4::gdk::Key,
    keycode: u32,
    modifier: ModifierType,
    pressed: bool,
    repeat: bool,
) -> RawKey {
    // Re-translating the hardware keycode with no modifiers recovers the base key (`1` under `!`),
    // and the same call says whether the layout itself consumed Control. See `keys`.
    let display = area.display();
    let unmodified_keyval = display
        .translate_key(keycode, ModifierType::empty(), 0)
        .map(|(key, _, _, _)| key);
    let consumed = display
        .translate_key(keycode, modifier, 0)
        .map(|(_, _, _, consumed)| consumed)
        .unwrap_or_else(ModifierType::empty);
    RawKey {
        keyval,
        unmodified_keyval,
        state: modifier,
        consumed,
        pressed,
        repeat,
    }
}

fn deliver(state: &RefCell<State>, raw: RawKey) {
    let Some(input) = keys::normalize_key(raw) else { return };
    submit(state, PaneInput::Key(input));
}

/// The one door to the shell: straight through, or behind a paste still being read.
fn submit(state: &RefCell<State>, input: PaneInput) {
    state.borrow_mut().submit(input);
}

/// `Ctrl+Shift+V`: reads the clipboard's text and pastes it. Its place in line is taken now, so
/// what is typed while the read is in flight follows the paste; [`PASTE_TIMEOUT`] gives up on a
/// clipboard that does not answer. **Known limit:** the read and the timer are GTK's and are not
/// tested without a display; what they call (`State::finish_paste`, `InputQueue`) is.
fn paste_clipboard(state: &Rc<RefCell<State>>, area: &GLArea, im: &IMMulticontext) {
    paste_from(state, area, im, area.clipboard(), "Ctrl+Shift+V");
}

/// A middle click: pastes the PRIMARY selection (foot's and alacritty's
/// own convention). The same door as `Ctrl+Shift+V` (queued behind a composition in flight, the
/// same [`PASTE_TIMEOUT`]): a middle click during a fast paste-then-type is exactly the race
/// `InputQueue` already exists to order correctly, and reimplementing that ordering a second time
/// for one gesture would be the second implementation this crate's own module doc (`lib.rs`) warns
/// against.
fn paste_primary(state: &Rc<RefCell<State>>, area: &GLArea, im: &IMMulticontext) {
    paste_from(state, area, im, area.primary_clipboard(), "middle click");
}

/// What [`paste_clipboard`] and [`paste_primary`] share: read `clipboard`'s text asynchronously and
/// paste it, `label` naming the gesture in every log line so the two are told apart.
fn paste_from(
    state: &Rc<RefCell<State>>,
    area: &GLArea,
    im: &IMMulticontext,
    clipboard: Clipboard,
    label: &'static str,
) {
    discard_composition(state, im, area);
    if !state.borrow_mut().remote_input(true) {
        return;
    }
    let (ticket, cancellable) = state.borrow_mut().begin_paste();
    {
        let state = Rc::downgrade(state);
        let cancelled = cancellable.clone();
        clipboard.read_text_async(Some(&cancellable), move |result| {
            if cancelled.is_cancelled() {
                return;
            }
            let text = match result {
                Ok(Some(text)) => text.to_string(),
                Ok(None) => {
                    println!("[terminal] {label}: the clipboard holds no text");
                    String::new()
                }
                Err(err) => {
                    eprintln!("[terminal] {label}: could not read the clipboard: {err}");
                    String::new()
                }
            };
            let Some(state) = state.upgrade() else { return };
            let input = keys::paste(&text);
            let had_text = input.is_some();
            if !state.borrow_mut().finish_paste(ticket, input) && had_text {
                eprintln!("[terminal] {label}: the clipboard answered too late; that paste is dropped");
            }
        });
    }
    let state = Rc::downgrade(state);
    glib::timeout_add_local_once(PASTE_TIMEOUT, move || {
        let Some(state) = state.upgrade() else { return };
        if state.borrow_mut().finish_paste(ticket, None) {
            eprintln!("[terminal] {label}: no answer from the clipboard in {PASTE_TIMEOUT:?}; typing goes on");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::gdk::ScrollUnit;

    #[test]
    fn a_reconnect_cancels_a_paste_and_every_key_parked_behind_it() {
        use eitri_terminal::mirror::{HistoryNote, LinkState, MirrorReport};
        for mut report in [
            MirrorReport {
                link: Some(LinkState::Disconnected { since_ms: 0 }),
                ..Default::default()
            },
            MirrorReport {
                link: Some(LinkState::Live),
                synced: Some(HistoryNote::default()),
                ..Default::default()
            },
        ] {
            let mut state = State::new("/srv/project".into());
            let (ticket, cancelled) = state.begin_paste();
            state.submit(PaneInput::Key(keys::paste("old command").unwrap()));
            state.connection_changed(&mut report);
            assert!(cancelled.is_cancelled());
            assert!(state.queue.is_empty());
            assert!(!state.finish_paste(ticket, keys::paste("stale paste")));
            assert!(report.refused_input_bytes > 0);
        }
    }

    #[test]
    fn a_remote_snapshot_returns_printable_keys_to_the_shell() {
        let mut state = State::new("/srv/project".into());
        state.ime.set_focused(true);
        state.copy_mode = true;
        assert_eq!(
            state.route_commit("x", HAS_THE_KEYS),
            CommitRoute::CopyMode(CopyModeKey::Swallow)
        );

        let mut report = MirrorReport {
            synced: Some(Default::default()),
            ..Default::default()
        };
        state.connection_changed(&mut report);
        assert!(matches!(state.route_commit("x", HAS_THE_KEYS), CommitRoute::Shell(_)));
    }

    use super::super::remote::test_support as remote_test;
    use eitri_core::remote_project::terminal_link::TerminalTarget;
    use eitri_terminal::mirror::{FrameRefused, MirrorEnd, MirrorReport};

    const REMOTE_SIZE: PtySize = PtySize {
        cols: 80,
        rows: 24,
        cell_width_px: 8,
        cell_height_px: 16,
    };

    struct QuietLink {
        read: std::io::PipeReader,
        _write: std::io::PipeWriter,
    }

    impl eitri_terminal::mirror::TermLink for QuietLink {
        fn readiness(&self) -> std::os::fd::BorrowedFd<'_> {
            use std::os::fd::AsFd;
            self.read.as_fd()
        }
        fn take(&mut self) -> Vec<eitri_terminal::mirror::LinkItem> {
            Vec::new()
        }
        fn write_input(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::NotConnected.into())
        }
        fn request_resize(&mut self, _: PtySize) {}
        fn set_colors(&mut self, _: &TerminalColors) {}
    }

    enum Seen {
        Size(PtySize),
        Colors(TerminalColors),
    }

    /// Hands the mirror `items` once and reports every size and colour the session asks of its link.
    struct RecordingLink {
        read: std::io::PipeReader,
        _write: std::io::PipeWriter,
        items: Vec<eitri_terminal::mirror::LinkItem>,
        seen: std::sync::mpsc::Sender<Seen>,
    }

    impl eitri_terminal::mirror::TermLink for RecordingLink {
        fn readiness(&self) -> std::os::fd::BorrowedFd<'_> {
            use std::os::fd::AsFd;
            self.read.as_fd()
        }
        fn take(&mut self) -> Vec<eitri_terminal::mirror::LinkItem> {
            if !self.items.is_empty() {
                // The one byte that made the link readable; read only while it is there.
                let _ = std::io::Read::read(&mut self.read, &mut [0]);
            }
            std::mem::take(&mut self.items)
        }
        fn write_input(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::NotConnected.into())
        }
        fn request_resize(&mut self, size: PtySize) {
            let _ = self.seen.send(Seen::Size(size));
        }
        fn set_colors(&mut self, colors: &TerminalColors) {
            let _ = self.seen.send(Seen::Colors(*colors));
        }
    }

    fn quiet_link(
        _: eitri_core::remote_project::terminal_link::TerminalLinkOptions,
    ) -> std::io::Result<(Box<dyn eitri_terminal::mirror::TermLink>, OpenAnswer)> {
        let (read, write) = std::io::pipe()?;
        Ok((Box::new(QuietLink { read, _write: write }), OpenAnswer::default()))
    }

    fn failing_link(
        _: eitri_core::remote_project::terminal_link::TerminalLinkOptions,
    ) -> std::io::Result<(Box<dyn eitri_terminal::mirror::TermLink>, OpenAnswer)> {
        Err(std::io::Error::other("scripted connection failure"))
    }

    type ScriptedLink = fn(
        eitri_core::remote_project::terminal_link::TerminalLinkOptions,
    ) -> std::io::Result<(Box<dyn eitri_terminal::mirror::TermLink>, OpenAnswer)>;

    /// The record worker's job and the GTK thread's handling of its answer, as `start_remote` runs them, with
    /// `link` in place of the network. `Ok` is what the arriving session was told; `Err` the start's failure.
    fn finish_scripted(
        state: &Rc<RefCell<State>>,
        plan: RemoteStart,
        view: View,
        link: impl FnOnce(
            eitri_core::remote_project::terminal_link::TerminalLinkOptions,
        ) -> std::io::Result<(Box<dyn eitri_terminal::mirror::TermLink>, OpenAnswer)>,
    ) -> Result<CatchUp, String> {
        let (tell, mut told) = futures_channel::mpsc::unbounded();
        let (wake, _woken) = futures_channel::mpsc::unbounded();
        run_remote_start(
            plan,
            |owner, cwd, again, retry, view, wake| {
                owner.start_with(
                    &owner.project.host_key_sha256,
                    "installation",
                    &cwd,
                    again,
                    retry,
                    eitri_terminal::mirror::MirrorConfig {
                        size: view.size,
                        colors: view.colors,
                        focused: view.focused,
                    },
                    wake,
                    link,
                )
            },
            tell,
            wake,
        );
        match told.try_recv() {
            Ok(RemoteReady::Started(session, entry, answer)) => {
                Ok(state.borrow_mut().remote_started(session, entry, answer, view, false).1)
            }
            Ok(RemoteReady::Failed(why)) => {
                state.borrow_mut().remote_failed(&why);
                Err(why)
            }
            _ => panic!("the start answered nothing"),
        }
    }

    /// `ensure_session`'s server branch without its GTK glue.
    fn start_scripted(state: &Rc<RefCell<State>>, link: ScriptedLink) -> Option<Result<CatchUp, String>> {
        let view = View {
            size: REMOTE_SIZE,
            colors: state.borrow().colors,
            focused: state.borrow().focused,
        };
        let plan = state.borrow_mut().begin_remote_start(view)?;
        Some(finish_scripted(state, plan, view, link))
    }

    /// A pane bound as the module wiring binds it, whose wake starts its server terminal through the production
    /// start with a scripted link (failing the first time when `fail_first`).
    fn scripted_pane(
        registry: Rc<remote::Registry>,
        module: &str,
        restored: bool,
        fail_first: bool,
    ) -> Rc<RefCell<State>> {
        let state = Rc::new(RefCell::new(State::new("/srv/project".into())));
        state.borrow_mut().wanted = true;
        let weak = Rc::downgrade(&state);
        let fail = std::cell::Cell::new(fail_first);
        state.borrow_mut().bind_remote(
            registry,
            module.into(),
            restored,
            Rc::new(move || {
                let Some(state) = weak.upgrade() else { return };
                if matches!(state.borrow().source(), TermSource::Server(_)) {
                    let link: ScriptedLink = if fail.replace(false) { failing_link } else { quiet_link };
                    let _ = start_scripted(&state, link);
                }
            }),
        );
        state
    }

    fn bound_pane(registry: Rc<remote::Registry>, module: &str, restored: bool) -> Rc<RefCell<State>> {
        scripted_pane(registry, module, restored, false)
    }

    /// An answer whose link said the terminal no longer exists.
    fn gone_answer() -> OpenAnswer {
        let answer = OpenAnswer::default();
        answer.mark_gone();
        answer
    }

    fn pane_options(
        state: &mut State,
    ) -> (
        eitri_core::remote_project::terminals::TerminalEntry,
        eitri_core::remote_project::terminal_link::TerminalLinkOptions,
    ) {
        let binding = state.remote.as_mut().unwrap();
        let owner = binding.current_owner().unwrap();
        let answer = owner
            .options(
                &owner.project.host_key_sha256,
                "installation",
                &owner.project.root,
                binding.again.take(),
                binding.retry_entry.take(),
                REMOTE_SIZE,
                state.colors,
            )
            .unwrap();
        binding.entry = Some(answer.0.clone());
        answer
    }

    #[test]
    fn waiting_becomes_server_when_connected() {
        for failed_first in [false, true] {
            let registry = remote_test::registry(1);
            let bottom = scripted_pane(registry.clone(), "terminal", false, failed_first);
            let extra = bound_pane(registry.clone(), "terminal#2", false);
            for pane in [&bottom, &extra] {
                assert!(matches!(pane.borrow().source(), TermSource::Waiting));
                assert!(!pane.borrow().remote.as_ref().unwrap().starting);
            }
            let connection = remote_test::connection();
            // A connection for another server hands nothing off; both bindings keep waiting.
            let mut wrong = connection.project().clone();
            wrong.host = "other-host".into();
            assert!(!remote_test::reject_hand_off(&registry, wrong, connection.clone()));
            assert!(matches!(bottom.borrow().source(), TermSource::Waiting));
            assert!(matches!(extra.borrow().source(), TermSource::Waiting));
            assert!(remote_test::hand_off(&registry, connection));
            if failed_first {
                // The failed start keeps its durable intent, and Enter's restart reattaches that same intent.
                let intent = {
                    let state = bottom.borrow();
                    assert!(state.exited && state.session.is_none());
                    let binding = state.remote.as_ref().unwrap();
                    assert!(matches!(binding.model.ending, Some(remote::Ending::Failed(_))));
                    assert_eq!(binding.again, Some(Again::Attach));
                    binding.current_owner().unwrap().saved().unwrap().unwrap()
                };
                bottom.borrow_mut().reset_for_restart();
                assert!(start_scripted(&bottom, quiet_link).unwrap().is_ok());
                let state = bottom.borrow();
                assert_eq!(
                    state.remote.as_ref().unwrap().entry.as_ref().unwrap().command_id,
                    intent.command_id
                );
            }
            for (pane, module) in [(&bottom, "terminal"), (&extra, "terminal#2")] {
                let state = pane.borrow();
                assert!(matches!(state.source(), TermSource::Server(_)));
                let binding = state.remote.as_ref().unwrap();
                assert!(state.session.is_some());
                assert_eq!(state.session.as_ref().unwrap().local_pid(), None);
                assert!(!binding.starting && binding.ownership_known);
                let entry = binding.entry.as_ref().unwrap();
                assert_eq!((entry.slot, entry.module.as_str()), (1, module));
                // An open's keys stay held until its answer is durable.
                assert!(entry.terminal_id.is_none() && !binding.model.recorded);
                assert_eq!(binding.current_owner().unwrap().saved().unwrap().as_ref(), Some(entry));
            }
            assert_eq!(remote_test::listeners(&registry), 2);
            drop(extra);
            assert_eq!(remote_test::listeners(&registry), 1);
            drop(bottom);
            assert_eq!(remote_test::listeners(&registry), 0);
        }
    }

    #[test]
    fn changes_made_while_a_server_start_runs_reach_the_session_when_it_arrives() {
        for changed in [false, true] {
            let registry = remote_test::registry(1);
            let pane = Rc::new(RefCell::new(State::new("/srv/project".into())));
            pane.borrow_mut()
                .bind_remote(registry.clone(), "terminal".into(), false, Rc::new(|| {}));
            remote_test::hand_off(&registry, remote_test::connection());
            let (plan, view) = {
                let mut state = pane.borrow_mut();
                state.wanted = true;
                state.allocate(FIRST_SHOW);
                let size = state.spawn_size(build_metrics).unwrap().unwrap();
                let view = state.view(size);
                (state.begin_remote_start(view).unwrap(), view)
            };
            let mut expected = CatchUp::default();
            if changed {
                // While the start is on the record worker and the network: the window settles, the theme and the
                // focus change. Each went nowhere then, with no session to send it to.
                let mut state = pane.borrow_mut();
                expected.size = state.allocate(Allocation {
                    height: FIRST_SHOW.height * 2.0,
                    ..FIRST_SHOW
                });
                assert!(expected.size.is_some());
                state.colors.background.r = state.colors.background.r.wrapping_add(1);
                expected.colors = Some(state.colors);
                state.focused = !view.focused;
                expected.focused = Some(state.focused);
            }
            let (seen, sizes_and_colors) = std::sync::mpsc::channel();
            assert_eq!(
                finish_scripted(&pane, plan, view, move |_| {
                    let (read, mut write) = std::io::pipe()?;
                    std::io::Write::write_all(&mut write, &[1])?;
                    Ok((
                        Box::new(RecordingLink {
                            read,
                            _write: write,
                            // Owed a snapshot too large to send: the mirror then asks for each new size at once
                            // instead of waiting for the first request's answer.
                            items: vec![eitri_terminal::mirror::LinkItem::Waiting(
                                eitri_terminal::headless::WaitWhy::SnapshotTooLarge,
                            )],
                            seen,
                        }),
                        OpenAnswer::default(),
                    ))
                }),
                Ok(CatchUp {
                    size: expected.size,
                    colors: expected.colors,
                    focused: expected.focused,
                })
            );
            // What reached the session, as its link was asked: the new grid and the new colours.
            let now = pane.borrow().view(view.size);
            let deadline = Instant::now() + Duration::from_secs(if changed { 10 } else { 1 });
            let (mut size_sent, mut colors_sent) = (false, false);
            while let Ok(item) = sizes_and_colors.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                match item {
                    Seen::Size(size) if (size.cols, size.rows) != (view.size.cols, view.size.rows) => {
                        assert_eq!((size.cols, size.rows), (now.size.cols, now.size.rows));
                        size_sent = true;
                    }
                    Seen::Colors(colors) if colors != view.colors => {
                        assert_eq!(colors, now.colors);
                        colors_sent = true;
                    }
                    _ => {}
                }
                if changed && size_sent && colors_sent {
                    break;
                }
            }
            assert_eq!((size_sent, colors_sent), (changed, changed));
        }
    }

    #[test]
    fn a_chat_or_page_window_neither_restores_nor_stops_nor_offers_a_slot_terminal() {
        use remote::TerminalWindow;
        assert_eq!(remote::terminal_window(false, false, false), TerminalWindow::Full);
        assert_eq!(remote::terminal_window(false, false, true), TerminalWindow::Alone);
        assert_eq!(remote::terminal_window(true, false, false), TerminalWindow::Second);
        assert_eq!(remote::terminal_window(false, true, false), TerminalWindow::Chat);
        assert_eq!(remote::terminal_window(true, true, false), TerminalWindow::Chat);
        let window_two = remote_test::registry(2);
        let p = remote_test::project();
        let records = remote_test::records(&window_two);
        let shell = records
            .terminals()
            .begin_open(&p.host_key_sha256, &p.root, 2, "terminal", "installation", 1)
            .unwrap();
        records
            .terminals()
            .opened(
                &p.host_key_sha256,
                &p.root,
                &shell.command_id,
                &"a".repeat(32),
                "daemon",
                "boot",
            )
            .unwrap();
        let saved = records.terminals().record(&p.host_key_sha256, &p.root).unwrap().entries;
        let module = remote_test::module_registry(&window_two);
        assert!(!module.binds_terminals());
        // The offer band has nothing to read orphans as, and is not built.
        assert!(module.owner(String::new()).is_none());
        let pane = bound_pane(module.clone(), "terminal", true);
        {
            let state = pane.borrow();
            let binding = state.remote.as_ref().unwrap();
            assert!(binding.current_owner().is_none());
            assert!(binding.ownership_known && !binding.restore);
            assert!(!binding.claims_module() && !binding.may_have_job());
        }
        restore_remote_owner(&pane);
        assert!(pane.borrow().remote.as_ref().unwrap().entry.is_none());
        remote_test::hand_off(&module, remote_test::connection());
        {
            let state = pane.borrow();
            assert!(state.exited && state.session.is_none());
            assert_eq!(
                state.remote.as_ref().unwrap().model.ending,
                Some(remote::Ending::Failed(remote::NO_SERVER_TERMINALS.into()))
            );
        }
        close_remote(&pane, true);
        assert_eq!(
            records.terminals().record(&p.host_key_sha256, &p.root).unwrap().entries,
            saved,
            "window 2's shell is untouched"
        );
    }

    #[test]
    fn an_unbound_pane_in_a_server_projects_window_never_starts_a_local_shell() {
        let pane = Rc::new(RefCell::new(State::new("/srv/project".into())));
        pane.borrow_mut().shell_env = Some(Rc::new(|| {
            panic!("a server project's pane must not evaluate the local shell environment")
        }));
        remote_test::local_window();
        assert!(matches!(pane.borrow().source(), TermSource::Local));
        let registry = remote_test::registry(1);
        let (source, extra) = State::start_source(&pane);
        assert!(matches!(source, TermSource::Waiting));
        assert!(extra.is_empty());
        remote_test::hand_off(&registry, remote_test::connection());
        assert!(
            matches!(pane.borrow().source(), TermSource::Waiting),
            "only a binding may start a server terminal"
        );
    }

    #[test]
    fn a_refused_connection_keeps_the_record_and_enter_reattaches() {
        for gone in [false, true] {
            let registry = remote_test::registry(1);
            let pane = bound_pane(registry.clone(), "terminal", false);
            remote_test::hand_off(&registry, remote_test::connection());
            let mut state = pane.borrow_mut();
            state.remote.as_mut().unwrap().answer = Some(if gone { gone_answer() } else { OpenAnswer::default() });
            state.remote_report(MirrorReport {
                ended: Some(MirrorEnd::Gone(
                    "the terminal connection was refused (PermissionDenied)".into(),
                )),
                ..Default::default()
            });
            let model = &state.remote.as_ref().unwrap().model;
            if gone {
                assert!(matches!(model.ending, Some(remote::Ending::Ended(MirrorEnd::Gone(_)))));
                assert_eq!(model.again(), Some(Again::Open));
            } else {
                assert!(matches!(model.ending, Some(remote::Ending::Unreachable(_))));
                assert_eq!(model.again(), Some(Again::Attach));
                assert!(model.notice().unwrap().contains("Enter tries again"));
                // Painting forgets only a gone terminal's entry: this one stays the binding's.
                assert!(state.remote.as_ref().unwrap().claims_module());
            }
        }
    }

    #[test]
    fn the_answer_watcher_of_an_open_whose_link_ended_lets_the_start_go() {
        let registry = remote_test::registry(1);
        let pane = Rc::new(RefCell::new(State::new("/srv/project".into())));
        pane.borrow_mut()
            .bind_remote(registry.clone(), "terminal".into(), false, Rc::new(|| {}));
        remote_test::hand_off(&registry, remote_test::connection());
        let view = View {
            size: REMOTE_SIZE,
            colors: TerminalColors::default(),
            focused: false,
        };
        let plan = pane.borrow_mut().begin_remote_start(view).unwrap();
        let answer = OpenAnswer::default();
        let (tell, mut told) = futures_channel::mpsc::unbounded();
        let (wake, _woken) = futures_channel::mpsc::unbounded();
        let handle = answer.clone();
        run_remote_start(
            plan,
            |owner, cwd, again, retry, view, wake| {
                owner.start_with(
                    &owner.project.host_key_sha256,
                    "installation",
                    &cwd,
                    again,
                    retry,
                    eitri_terminal::mirror::MirrorConfig {
                        size: view.size,
                        colors: view.colors,
                        focused: view.focused,
                    },
                    wake,
                    move |options| quiet_link(options).map(|(link, _)| (link, handle)),
                )
            },
            tell,
            wake,
        );
        assert!(matches!(told.try_recv(), Ok(RemoteReady::Started(..))));
        // The open was refused before an identity arrived: the link ends with no answer to wait for.
        answer.finish();
        let guard = Instant::now() + Duration::from_secs(600);
        loop {
            match told.try_recv() {
                Err(error) if error.is_closed() => break,
                Err(_) if Instant::now() < guard => std::thread::sleep(Duration::from_millis(10)),
                other => panic!("the watcher kept its sender: {}", other.is_ok()),
            }
        }
    }

    /// A saved slot-1 base terminal of a window 1 that is still stopping its terminals, and the registry of a full
    /// window of the same project opened meanwhile.
    fn window_beside_an_exiting_owner() -> (Rc<remote::Registry>, Rc<remote::Registry>, Vec<TerminalEntry>) {
        let exiting = remote_test::full_window(None);
        let p = remote_test::project();
        let records = remote_test::records(&exiting);
        let saved = records
            .terminals()
            .begin_open(&p.host_key_sha256, &p.root, 1, "terminal", "installation", 1)
            .unwrap();
        records
            .terminals()
            .opened(
                &p.host_key_sha256,
                &p.root,
                &saved.command_id,
                &"a".repeat(32),
                "daemon",
                "boot",
            )
            .unwrap();
        let saved = records.terminals().record(&p.host_key_sha256, &p.root).unwrap().entries;
        let window = remote_test::full_window(Some(&exiting));
        (exiting, window, saved)
    }

    /// A pane bound at the window's start whose wake is the production one without its GTK glue:
    /// `restore_remote_owner`, then `ensure_session`'s start once the record worker has run what was queued before.
    fn startup_pane(window: &Rc<remote::Registry>) -> Rc<RefCell<State>> {
        let pane = Rc::new(RefCell::new(State::new("/srv/project".into())));
        pane.borrow_mut().wanted = true;
        let weak = Rc::downgrade(&pane);
        pane.borrow_mut().bind_remote(
            window.clone(),
            "terminal".into(),
            true,
            Rc::new(move || {
                let Some(state) = weak.upgrade() else { return };
                restore_remote_owner(&state);
                let work = state.borrow().remote.as_ref().unwrap().work.clone();
                let (done, ran) = std::sync::mpsc::channel();
                work.run(move || {
                    let _ = done.send(());
                });
                ran.recv_timeout(Duration::from_secs(600)).unwrap();
                if !retry_slot_before_start(&state) && matches!(state.borrow().source(), TermSource::Server(_)) {
                    let _ = start_scripted(&state, quiet_link);
                }
            }),
        );
        pane
    }

    #[test]
    fn a_window_that_lost_the_first_slot_restores_and_starts_once_the_owner_has_gone() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let (exiting, window, saved) = window_beside_an_exiting_owner();
                assert!(!window.binds_terminals() && window.may_bind_terminals());
                let pane = startup_pane(&window);
                remote_test::hand_off(&window, remote_test::connection());
                {
                    let state = pane.borrow();
                    let binding = state.remote.as_ref().unwrap();
                    assert_eq!(
                        binding.model.ending,
                        Some(remote::Ending::Failed(remote::ANOTHER_WINDOW_OWNS.into()))
                    );
                    assert!(binding.current_owner().is_none() && !binding.claims_module());
                }
                pane.borrow_mut().reset_for_restart();
                assert!(!retry_slot_before_start(&pane), "the owner still runs");
                let p = remote_test::project();
                let records = remote_test::records(&window);
                assert_eq!(
                    records.terminals().record(&p.host_key_sha256, &p.root).unwrap().entries,
                    saved,
                    "a window without the slot touches no slot-1 record"
                );
                drop(exiting);
                // Enter: the start tries for the slot again, and taking it wakes the panes.
                pane.borrow_mut().reset_for_restart();
                assert!(retry_slot_before_start(&pane));
                let state = pane.borrow();
                let binding = state.remote.as_ref().unwrap();
                assert!(state.session.is_some(), "started with no relaunch");
                let entry = binding.entry.as_ref().unwrap();
                assert_eq!(
                    entry.command_id, saved[0].command_id,
                    "the saved shell is attached, not a new one"
                );
                assert_eq!(entry.terminal_id.as_deref(), Some("a".repeat(32).as_str()));
            })
            .unwrap();
    }

    #[test]
    fn a_pane_closed_while_its_window_had_no_slot_reads_no_entry_when_the_slot_comes() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let (exiting, window, _) = window_beside_an_exiting_owner();
                let closed = startup_pane(&window);
                closed.borrow_mut().close();
                close_remote(&closed, true);
                let other = Rc::new(RefCell::new(State::new("/srv/project".into())));
                other
                    .borrow_mut()
                    .bind_remote(window.clone(), "terminal#2".into(), false, Rc::new(|| {}));
                drop(exiting);
                assert!(window.retry_slot());
                window.wake();
                let state = closed.borrow();
                let binding = state.remote.as_ref().unwrap();
                assert!(binding.current_owner().is_some(), "the window owns the slot now");
                assert!(!binding.restore && binding.entry.is_none());
                assert!(!binding.claims_module(), "the closed pane holds nothing of window 1's");
            })
            .unwrap();
    }

    #[test]
    fn enter_opens_a_new_shell_when_the_kept_entry_was_cleared_from_the_record() {
        let registry = remote_test::registry(1);
        let pane = bound_pane(registry.clone(), "terminal", false);
        remote_test::hand_off(&registry, remote_test::connection());
        let old = pane.borrow().remote.as_ref().unwrap().entry.clone().unwrap();
        pane.borrow_mut().remote_report(MirrorReport {
            ended: Some(MirrorEnd::Gone("incompatible Eitri server".into())),
            ..Default::default()
        });
        // `eitri host stop`, as the notice tells the person, clears this host's terminal records.
        let p = remote_test::project();
        let records = remote_test::records(&registry);
        records.terminals().clear_host(&p.host_key_sha256).unwrap();
        for _ in 0..2 {
            pane.borrow_mut().reset_for_restart();
            assert_eq!(
                start_scripted(&pane, quiet_link).map(|started| started.is_ok()),
                Some(true)
            );
        }
        let state = pane.borrow();
        let entry = state.remote.as_ref().unwrap().entry.as_ref().unwrap();
        assert_ne!(entry.command_id, old.command_id);
        assert!(entry.terminal_id.is_none(), "a new open");
        assert_eq!(
            records.terminals().record(&p.host_key_sha256, &p.root).unwrap().entries,
            [entry.clone()]
        );
    }

    #[test]
    fn bindings_with_the_same_module_keep_their_own_listeners() {
        let registry = remote_test::registry(1);
        let mut panes = Vec::new();
        for _ in 0..2 {
            let state = Rc::new(RefCell::new(State::new("/srv/project".into())));
            let weak = Rc::downgrade(&state);
            state.borrow_mut().bind_remote(
                registry.clone(),
                "terminal".into(),
                false,
                Rc::new(move || {
                    if let Some(state) = weak.upgrade() {
                        state.borrow_mut().remote.as_mut().unwrap().model.opening = true;
                    }
                }),
            );
            panes.push(state);
        }
        assert_eq!(remote_test::listeners(&registry), 2);
        panes.remove(0);
        assert_eq!(remote_test::listeners(&registry), 1);
        remote_test::hand_off(&registry, remote_test::connection());
        assert!(panes[0].borrow().remote.as_ref().unwrap().model.opening);
        panes.clear();
        assert_eq!(remote_test::listeners(&registry), 0);
    }

    #[test]
    fn window_two_gets_the_connection_too() {
        let connection = remote_test::connection();
        for slot in [1, 2] {
            let registry = remote_test::registry(slot);
            let pane = bound_pane(registry.clone(), "terminal", false);
            remote_test::hand_off(&registry, connection.clone());
            let state = pane.borrow();
            let entry = state.remote.as_ref().unwrap().entry.as_ref().unwrap();
            assert_eq!(entry.slot, slot);
            assert!(state.session.is_some());
            assert_eq!(state.session.as_ref().unwrap().local_pid(), None);
        }
    }

    #[test]
    fn each_ending_keeps_the_screen_and_adds_its_line() {
        let registry = remote_test::registry(1);
        for (ending, words) in [
            (MirrorEnd::Replaced, "Enter takes it back"),
            (MirrorEnd::Stopped, "eitri host stop"),
            (MirrorEnd::Closed, "was closed"),
            (MirrorEnd::Gone("host ended\u{1b}".into()), "gone: host ended\\u{1b}"),
            (MirrorEnd::Refused(FrameRefused::ReplayUnfinished), "Enter reconnects"),
        ] {
            let pane = bound_pane(registry.clone(), "terminal", false);
            let mut state = pane.borrow_mut();
            state.remote.as_mut().unwrap().answer = Some(gone_answer());
            state.frame = Some(message_frame(REMOTE_SIZE, state.colors, "the last server screen"));
            let before = state.frame.clone();
            state.remote_report(MirrorReport {
                ended: Some(ending),
                ..Default::default()
            });
            assert_eq!(state.frame, before);
            assert!(state.exited);
            let model = &state.remote.as_ref().unwrap().model;
            let ops = remote::notice_ops(
                &model.notice().unwrap(),
                model.ending_row(Some(0), REMOTE_SIZE.rows),
                240,
                state.colors,
            );
            assert!(
                matches!(&ops[0], terminal_render::PaintOp::DrawNotice { row: 1, text, .. } if text.contains(words))
            );
        }
        let pane = bound_pane(registry, "terminal", false);
        let mut state = pane.borrow_mut();
        state.frame = Some(message_frame(REMOTE_SIZE, state.colors, "last output before exit"));
        let before = state.frame.clone();
        state.remote_exit(EXITED);
        assert_eq!(state.frame, before);
        assert!(state
            .remote
            .as_ref()
            .unwrap()
            .model
            .notice()
            .unwrap()
            .contains("process exited 0"));
    }

    #[test]
    fn enter_after_each_ending_does_the_right_call() {
        for end in [
            Some(MirrorEnd::Replaced),
            Some(MirrorEnd::Refused(FrameRefused::OffsetOverflow)),
            Some(MirrorEnd::Stopped),
            Some(MirrorEnd::Closed),
            Some(MirrorEnd::Gone("gone".into())),
            None,
        ] {
            let registry = remote_test::registry(1);
            let pane = bound_pane(registry.clone(), "terminal", false);
            pane.borrow_mut().remote.as_mut().unwrap().answer = Some(gone_answer());
            let old = pane_options(&mut pane.borrow_mut()).0;
            let owner = pane.borrow().remote.as_ref().unwrap().current_owner().unwrap();
            owner
                .opened(&old, &("ab".into(), "daemon".into(), "boot".into()))
                .unwrap();
            let records = remote_test::records(&registry);
            let attach = matches!(end, Some(MirrorEnd::Replaced | MirrorEnd::Refused(_)));
            if end == Some(MirrorEnd::Replaced) {
                records
                    .terminals()
                    .claim(
                        &owner.project.host_key_sha256,
                        &owner.project.root,
                        &old.command_id,
                        2,
                        "terminal#3",
                    )
                    .unwrap();
            }
            {
                let mut state = pane.borrow_mut();
                if let Some(end) = end {
                    state.remote_report(MirrorReport {
                        ended: Some(end),
                        ..Default::default()
                    });
                } else {
                    state.remote_exit(EXITED);
                }
                state.reset_for_restart();
                assert_eq!(
                    state.remote.as_ref().unwrap().again,
                    Some(if attach { Again::Attach } else { Again::Open })
                );
                let (entry, options) = pane_options(&mut state);
                // This is the production link-construction boundary: inspect durable state before using its target.
                let disk = records
                    .terminals()
                    .record(&owner.project.host_key_sha256, &owner.project.root)
                    .unwrap();
                assert_eq!(disk.entries, [entry.clone()]);
                assert_eq!((entry.slot, entry.module.as_str()), (1, "terminal"));
                if attach {
                    assert_eq!(entry.command_id, old.command_id);
                    assert!(
                        matches!(options.target, TerminalTarget::Attach { terminal_id, .. } if terminal_id == "ab")
                    );
                } else {
                    assert_ne!(entry.command_id, old.command_id);
                    assert!(
                        matches!(options.target, TerminalTarget::Open { command_id, .. } if command_id == entry.command_id)
                    );
                }
                assert!(!state.remote.as_ref().unwrap().model.recorded);
            }
        }
    }

    #[test]
    fn restart_resets_the_remote_binding_once() {
        let registry = remote_test::registry(1);
        let pane = bound_pane(registry, "terminal", false);
        pane.borrow_mut().remote_report(MirrorReport {
            ended: Some(MirrorEnd::Replaced),
            ..Default::default()
        });
        pane.borrow_mut().reset_for_restart();
        assert_eq!(pane.borrow().remote.as_ref().unwrap().again, Some(Again::Attach));
        let source = eitri_core::source_scan::read_required("shell/src/terminal/pane.rs");
        let code = eitri_core::source_scan::code_only(&source.text);
        let restart = code
            .split("fn restart(")
            .nth(1)
            .unwrap()
            .split("fn connect_resize")
            .next()
            .unwrap();
        assert_eq!(restart.matches("reset_for_restart()").count(), 1);
        assert_eq!(restart.matches("Model::new").count(), 0);
        let reset = code
            .split("fn reset_for_restart(")
            .nth(1)
            .unwrap()
            .split("fn detach(")
            .next()
            .unwrap();
        assert_eq!(reset.matches("remote.reset()").count(), 1);
    }

    #[test]
    fn replaced_then_closed_sends_no_stop() {
        let registry = remote_test::registry(1);
        let pane = bound_pane(registry.clone(), "terminal", false);
        let old = pane_options(&mut pane.borrow_mut()).0;
        pane.borrow_mut().remote_report(MirrorReport {
            ended: Some(MirrorEnd::Replaced),
            ..Default::default()
        });
        assert!(pane.borrow().remote.as_ref().unwrap().entry.is_none());
        let owner = pane.borrow().remote.as_ref().unwrap().current_owner().unwrap();
        owner.close(true, None).unwrap();
        assert_eq!(owner.saved().unwrap(), Some(old));
    }

    #[test]
    fn a_gone_entry_forgotten_after_its_notice_no_longer_belongs_to_the_binding() {
        let registry = remote_test::registry(1);
        let pane = bound_pane(registry.clone(), "terminal", false);
        let entry = pane_options(&mut pane.borrow_mut()).0;
        let owner = pane.borrow().remote.as_ref().unwrap().current_owner().unwrap();
        pane.borrow_mut().remote_report(MirrorReport {
            ended: Some(MirrorEnd::Gone("gone".into())),
            ..Default::default()
        });
        owner.show_gone(&entry).unwrap();
        pane.borrow_mut().remote.as_mut().unwrap().entry.take();
        owner.close(true, None).unwrap();
        assert!(owner.saved().unwrap().is_none());
    }

    #[test]
    fn retired_and_failed_placement_bindings_stop_nothing() {
        let registry = remote_test::registry(1);
        let old_pane = bound_pane(registry.clone(), "terminal#2", false);
        let owner = old_pane.borrow().remote.as_ref().unwrap().current_owner().unwrap();
        owner.close(true, None).unwrap();
        let live = bound_pane(registry.clone(), "terminal#2", false);
        let entry = pane_options(&mut live.borrow_mut()).0;
        owner.close(true, None).unwrap();
        assert_eq!(owner.saved().unwrap(), Some(entry.clone()));
        let failed = bound_pane(registry, "terminal#2", false);
        failed
            .borrow()
            .remote
            .as_ref()
            .unwrap()
            .current_owner()
            .unwrap()
            .close(true, None)
            .unwrap();
        assert_eq!(owner.saved().unwrap(), Some(entry));
    }

    #[test]
    fn only_restored_or_claimed_entries_are_attached() {
        let registry = remote_test::registry(1);
        let records = remote_test::records(&registry);
        let p = remote_test::project();
        let stale = records
            .terminals()
            .begin_open(&p.host_key_sha256, &p.root, 1, "terminal", "installation", 1)
            .unwrap();
        let new_pane = bound_pane(registry.clone(), "terminal", false);
        let binding = new_pane.borrow();
        let owner = binding.remote.as_ref().unwrap().current_owner().unwrap();
        assert!(!binding.remote.as_ref().unwrap().claims_module());
        assert!(owner
            .options(
                &p.host_key_sha256,
                "installation",
                &p.root,
                None,
                None,
                REMOTE_SIZE,
                binding.colors
            )
            .is_err());
        assert_eq!(owner.orphans(&[], None).unwrap(), [stale.clone()]);
        let restored = bound_pane(registry, "terminal", true);
        let mut state = restored.borrow_mut();
        let binding = state.remote.as_mut().unwrap();
        let owner = binding.current_owner().unwrap();
        binding.entry = owner.restore().unwrap();
        binding.ownership_known = true;
        assert_eq!(pane_options(&mut state).0.command_id, stale.command_id);
    }

    #[test]
    fn adoption_belongs_to_the_instance_created_for_it() {
        let registry = remote_test::registry(1);
        let records = remote_test::records(&registry);
        let p = remote_test::project();
        let orphan = records
            .terminals()
            .begin_open(&p.host_key_sha256, &p.root, 2, "terminal#4", "installation", 1)
            .unwrap();
        let bottom = bound_pane(registry.clone(), "terminal", false);
        let mut adopted = None;
        registry.adopt(&orphan, || {
            adopted = Some(bound_pane(registry.clone(), "terminal#2", false));
        });
        let bottom_entry = pane_options(&mut bottom.borrow_mut()).0;
        assert_ne!(bottom_entry.command_id, orphan.command_id);
        assert_eq!(
            pane_options(&mut adopted.unwrap().borrow_mut()).0.command_id,
            orphan.command_id
        );
        let other = records
            .terminals()
            .begin_open(&p.host_key_sha256, &p.root, 2, "terminal#5", "installation", 1)
            .unwrap();
        registry.adopt(&other, || {}); // Controller refused creation.
        let later = bound_pane(registry.clone(), "terminal#3", false);
        assert_ne!(pane_options(&mut later.borrow_mut()).0.command_id, other.command_id);
        assert!(records
            .terminals()
            .orphans(
                &p.host_key_sha256,
                &p.root,
                &[
                    (1, "terminal".into()),
                    (1, "terminal#2".into()),
                    (1, "terminal#3".into())
                ]
            )
            .unwrap()
            .contains(&other));
    }

    #[test]
    fn an_adoption_into_a_reused_idle_pane_keeps_its_instance_id() {
        let registry = remote_test::registry(1);
        let records = remote_test::records(&registry);
        let p = remote_test::project();
        let pane = bound_pane(registry.clone(), "terminal#2", false);
        let old = pane_options(&mut pane.borrow_mut()).0;
        let retired_owner = pane.borrow().remote.as_ref().unwrap().current_owner().unwrap();
        retired_owner.close(true, None).unwrap();
        pane.borrow_mut().close();
        let orphan = records
            .terminals()
            .begin_open(&p.host_key_sha256, &p.root, 2, "terminal#4", "installation", 1)
            .unwrap();
        registry.adopt(&orphan, || {
            pane.borrow_mut().set_start_dir(p.root.clone());
        });
        // A queued close still holds the retired owner; it must not clear this instance's adoption.
        retired_owner.close(true, None).unwrap();
        let entry = pane_options(&mut pane.borrow_mut()).0;
        assert_eq!(entry.command_id, orphan.command_id);
        assert_eq!((entry.slot, entry.module.as_str()), (1, "terminal#2"));
        let disk = records.terminals().record(&p.host_key_sha256, &p.root).unwrap();
        assert_eq!(disk.entries.len(), 2);
        assert!(disk.entries.iter().any(|e| e.command_id == old.command_id
            && e.state == eitri_core::remote_project::terminals::TerminalState::StopQueued));
    }

    #[test]
    fn server_terminal_gets_no_open_socket_or_ide_port() {
        let registry = remote_test::registry(1);
        let pane = bound_pane(registry, "terminal", false);
        pane.borrow_mut().shell_env = Some(Rc::new(|| {
            panic!("server starts must not evaluate the local socket/IDE environment")
        }));
        let (source, extra) = State::start_source(&pane);
        assert!(matches!(source, TermSource::Waiting));
        assert!(extra.is_empty());
        let registry = pane.borrow().remote.as_ref().unwrap().registry.clone();
        remote_test::hand_off(&registry, remote_test::connection());
        let (source, extra) = State::start_source(&pane);
        let mut state = pane.borrow_mut();
        assert!(matches!(source, TermSource::Server(_)));
        assert!(extra.is_empty());
        let (entry, options) = pane_options(&mut state);
        assert_eq!(options.installation, "installation");
        assert_eq!(options.size, REMOTE_SIZE);
        assert!(
            matches!(options.target, TerminalTarget::Open { cwd, command_id } if cwd == PathBuf::from("/srv/project") && command_id == entry.command_id)
        );
        assert_eq!(state.session.as_ref().unwrap().local_pid(), None);
    }

    #[test]
    fn a_server_terminal_move_is_refused_with_words() {
        let registry = remote_test::registry(1);
        let pane = bound_pane(registry, "terminal", false);
        assert_eq!(
            pane.borrow_mut().detach().unwrap_err(),
            "a terminal on the server does not move to another window yet"
        );
        assert!(!pane.borrow().frozen);
    }

    #[test]
    fn a_remote_binding_refuses_incoming_local_adoption() {
        let registry = remote_test::registry(1);
        let pane = bound_pane(registry, "terminal", false);
        let error = pane
            .borrow_mut()
            .adopt(&serde_json::json!({}), vec![], || {})
            .err()
            .unwrap();
        assert_eq!(error, "a terminal on the server does not move to another window yet");
        assert!(pane.borrow().session.is_none());
        assert!(State::new("/tmp".into()).allow_adopt().is_ok());
    }

    #[test]
    fn a_signal_close_leaves_server_terminals_running() {
        const CHILD: &str = "EITRI_TERMINAL_SIGNAL_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "terminal::pane::tests::a_signal_close_leaves_server_terminals_running",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let registry = remote_test::registry(1);
        let pane = bound_pane(registry.clone(), "terminal", false);
        let entry = pane_options(&mut pane.borrow_mut()).0;
        assert!(!crate::end_signals::signal_close_started());
        crate::end_signals::mark_closing();
        close_remote(&pane, !crate::end_signals::signal_close_started());
        let work = pane.borrow().remote.as_ref().unwrap().work.clone();
        let (done, wait) = std::sync::mpsc::channel();
        work.run(move || {
            done.send(()).unwrap();
        });
        wait.recv_timeout(Duration::from_secs(10)).unwrap();
        while context.pending() {
            context.iteration(false);
        }
        let p = remote_test::project();
        assert_eq!(
            remote_test::records(&registry)
                .terminals()
                .record(&p.host_key_sha256, &p.root)
                .unwrap()
                .entries,
            [entry]
        );
    }

    fn osc52_write(bytes: usize) -> HostEvents {
        HostEvents {
            clipboard: Some("x".repeat(bytes)),
            clipboard_bytes: Some(bytes),
            ..HostEvents::default()
        }
    }

    const EXITED: ExitInfo = ExitInfo {
        code: Some(0),
        signal: None,
        detached: false,
    };

    #[test]
    fn an_osc52_notice_owed_when_the_shell_exit_closes_the_pane_is_handed_on() {
        let now = Instant::now();
        // A hidden pane's write, never shown, then the exit in a later update.
        let mut state = State::new(PathBuf::from("/work"));
        assert_eq!(state.notice_on_update(&osc52_write(5), None, false, now), (None, None));
        assert_eq!(
            state.notice_on_update(&HostEvents::default(), Some(&EXITED), false, now),
            (None, Some("Terminal 1: OSC 52 wrote clipboard: 5 bytes".to_string()))
        );

        // The last command's write and the exit in one update: the write is counted before the close takes it.
        for on_screen in [false, true] {
            let mut state = State::new(PathBuf::from("/work"));
            let (until, lost) = state.notice_on_update(&osc52_write(7), Some(&EXITED), on_screen, now);
            assert_eq!(until.is_some(), on_screen);
            assert_eq!(lost.as_deref(), Some("Terminal 1: OSC 52 wrote clipboard: 7 bytes"));
        }

        // A detached or unknown exit keeps the module: nothing is handed on, and the notice stays with the pane.
        let detached = ExitInfo {
            code: None,
            signal: None,
            detached: true,
        };
        for exit in [detached, ExitInfo::UNKNOWN] {
            let mut state = State::new(PathBuf::from("/work"));
            assert_eq!(
                state.notice_on_update(&osc52_write(3), Some(&exit), false, now),
                (None, None)
            );
            assert!(
                state.clipboard_notice.show(now).is_some(),
                "shown once the pane is mapped"
            );
        }
    }

    #[test]
    fn a_timed_out_paste_cancels_its_read_and_releases_typing() {
        let mut state = State::new(PathBuf::from("/work"));
        let (ticket, _) = state.begin_paste();
        let read = state.paste_reads[0].1.clone();
        state.submit(PaneInput::Key(keys::paste("next").unwrap()));
        assert!(state.finish_paste(ticket, None));
        assert!(read.is_cancelled());
        assert!(state.paste_reads.is_empty());
        assert!(state.queue.is_empty());
        assert!(!state.finish_paste(ticket, keys::paste("late")));
    }

    #[test]
    fn closing_or_restarting_cancels_every_paste_read() {
        for close in [false, true] {
            let mut state = State::new(PathBuf::from("/work"));
            let tickets = [state.begin_paste().0, state.begin_paste().0];
            let reads: Vec<_> = state.paste_reads.iter().map(|(_, read)| read.clone()).collect();
            if close {
                state.close();
            } else {
                state.forget_session();
            }
            assert!(reads.iter().all(|read| read.is_cancelled()));
            assert!(state.paste_reads.is_empty());
            for ticket in tickets {
                assert!(!state.finish_paste(ticket, keys::paste("late")));
            }
        }
    }

    #[test]
    fn the_fallback_spec_keeps_the_extra_list() {
        let extra = vec![(OsString::from("CLAUDE_CODE_SSE_PORT"), OsString::from("23456"))];
        let cwd = PathBuf::from("/work");
        let first = shell_spec(None, cwd.clone(), &extra);
        assert_eq!(first.extra_env, extra);
        let fallback = shell_spec(Some(PathBuf::from("/bin/sh")), cwd.clone(), &extra);
        assert_eq!(fallback.program, PathBuf::from("/bin/sh"));
        assert_eq!(fallback.cwd, cwd);
        assert_eq!(fallback.extra_env, extra);
    }

    #[test]
    fn no_env_hook_means_an_empty_list() {
        let state = State::new(PathBuf::from("/work"));
        assert!(state.shell_env.is_none());
        let extra = state.shell_env.as_ref().map(|env| env()).unwrap_or_default();
        assert!(extra.is_empty());
        assert!(shell_spec(None, state.cwd.clone(), &extra).extra_env.is_empty());
    }

    /// One wheel notch scrolls 3 lines UP (into history): `WheelZoom`'s own "up is larger"
    /// convention (`dy` negative) carries straight through to "up is into history" with no sign
    /// flip in `scroll_request` itself.
    #[test]
    fn one_wheel_notch_is_three_lines_up() {
        let mut wheel = WheelZoom::default();
        let notches = wheel_notches(&mut wheel, ScrollUnit::Wheel, -1.0);
        assert_eq!(scroll_request(notches), Some(ScrollRequest::Lines(3)));
    }

    /// A touchpad's small `dy`s must accumulate to the same whole
    /// notch a real wheel click would be, not step on every event.
    #[test]
    fn ten_surface_pixel_events_worth_one_notch_make_one_request() {
        let mut wheel = WheelZoom::default();
        let mut requests = Vec::new();
        for _ in 0..10 {
            requests.extend(scroll_request(wheel_notches(&mut wheel, ScrollUnit::Surface, -1.5)));
        }
        assert_eq!(requests, vec![ScrollRequest::Lines(3)], "one notch (15px), not ten");
    }

    /// `WheelZoom`'s own semantics, reused rather than reimplemented: a reversal mid-gesture drops
    /// the fractional remainder instead of netting it against the new direction.
    #[test]
    fn a_reversal_drops_the_remainder_instead_of_netting_it() {
        let mut wheel = WheelZoom::default();
        assert_eq!(
            scroll_request(wheel_notches(&mut wheel, ScrollUnit::Wheel, -0.75)),
            None
        );
        assert_eq!(
            scroll_request(wheel_notches(&mut wheel, ScrollUnit::Wheel, 1.0)),
            Some(ScrollRequest::Lines(-3)),
            "the -0.75 is dropped, not netted into this one"
        );
    }

    /// An unused, hidden terminal must not build metrics -- a
    /// Skia `FontMgr`, a fontconfig match, and (before the fix) an `.expect("no usable font")`
    /// panic path -- at every launch, only because a window exists. `State::new` is the
    /// construction seam `TerminalPane::new` uses before any `gtk4::GLArea` is even built, so this
    /// needs no display and no `gtk4::init` -- consistent with every other test in this module,
    /// none of which touches real GTK.
    #[test]
    fn a_freshly_constructed_pane_builds_no_metrics() {
        let state = State::new(PathBuf::from("/tmp"));
        assert!(
            state.metrics.is_none(),
            "metrics must wait for ensure_session's first spawn attempt, not be built at construction"
        );
    }

    /// A restart in place decided on a read of an idle shell compares this count: a key or a mouse report after the
    /// read counts, a focus report (sent when the keys move, typing nothing) does not, and a pane with no shell to
    /// take the input counts it all the same.
    #[test]
    fn keys_and_mouse_reports_are_counted_and_focus_reports_are_not() {
        let mut state = State::new(PathBuf::from("/tmp"));
        assert_eq!(state.typed, 0);
        state.send_input(PaneInput::Key(keys::paste("ls\r").expect("pasted text")));
        assert_eq!(state.typed, 1);
        state.send_input(PaneInput::Focus(true));
        assert_eq!(state.typed, 1, "a focus report types nothing");
        state.frozen = true;
        state.send_input(PaneInput::Key(keys::paste("x").expect("pasted text")));
        assert_eq!(state.typed, 2);
    }

    #[test]
    fn input_parked_behind_a_paste_counts_as_typed() {
        use eitri_core::wm::ShellStart;

        let mut state = State::new(PathBuf::from("/tmp"));
        let before = state.typed;
        state.begin_paste();
        assert!(state.typed > before);
        let after_paste = state.typed;
        state.submit(PaneInput::Key(keys::paste("x").unwrap()));
        assert!(state.typed > after_paste);
        assert!(!ShellStart::of(0, Some(1))
            .typed(before)
            .untouched(Some(ShellStart::of(0, Some(1)).typed(state.typed))));
    }

    #[test]
    fn a_focus_report_parked_behind_a_paste_is_not_counted_as_typing() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.begin_paste();
        let before = state.typed;
        state.submit(PaneInput::Focus(true));
        assert_eq!(state.typed, before);
    }

    #[test]
    fn a_paste_alone_makes_a_restart_refuse() {
        use eitri_core::wm::ShellStart;

        let mut state = State::new(PathBuf::from("/tmp"));
        let before = state.typed;
        state.begin_paste();
        assert!(state.typed > before);
        assert!(!ShellStart::of(0, Some(1))
            .typed(before)
            .untouched(Some(ShellStart::of(0, Some(1)).typed(state.typed))));
    }

    #[test]
    fn a_terminal_with_input_waiting_behind_a_paste_is_not_frozen_for_a_move() {
        let mut state = State::new(PathBuf::from("/tmp"));
        assert!(state.freeze_refusal().is_none());
        let (ticket, _) = state.begin_paste();
        state.submit(PaneInput::Key(keys::paste("x").unwrap()));
        assert!(state.freeze_refusal().is_some());
        assert!(state.finish_paste(ticket, None));
        assert!(state.freeze_refusal().is_none());
    }

    /// `prefix x` or the shell's own end (2026-09-26): after a close nothing spawns until a start
    /// wants a shell again, and then a fresh one does -- even after an exit, which on its own waits
    /// for Enter -- on a blank pane, at the allocation the pane already had.
    #[test]
    fn a_closed_pane_forgets_its_shell_and_the_next_start_spawns_a_fresh_one() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        state.allocate(FIRST_SHOW);
        let first = state.spawn_size(build_metrics).unwrap().unwrap();
        // The shell exited: its notice is on screen and Enter would restart it.
        state.exited = true;
        state.frame = Some(message_frame(first, state.colors, "[process exited 0]"));
        state.cursor = Some(CursorCell {
            row: 1,
            col: 0,
            visible: true,
        });

        state.close();
        assert!(!state.wanted && !state.exited && state.session.is_none());
        assert!(
            state.frame.is_none() && state.cursor.is_none(),
            "nothing of the last shell stays"
        );
        assert_eq!(state.spawn_size(build_metrics), None, "closed: nothing spawns");

        state.wanted = true;
        assert_eq!(
            state.spawn_size(build_metrics).unwrap().unwrap(),
            first,
            "a fresh shell, at the allocation the pane already had"
        );
    }

    /// A pane prepared for a terminal from the other window has no allocation of its own yet: the screen it takes
    /// over is sized to the other window's size, its own allocation wins once it has one, and with neither it is a
    /// nominal size the first allocation resizes, never a refusal that would lose the shell.
    #[test]
    fn an_adopted_terminal_takes_the_prepared_size_until_the_pane_is_allocated() {
        let at = |allocation: Allocation| {
            let metrics = default_metrics(allocation.width, allocation.height, allocation.scale);
            (metrics.cols(), metrics.rows())
        };
        let prepared = Allocation {
            width: 600.0,
            height: 400.0,
            scale: 1.0,
        };
        let mut state = State::new(PathBuf::from("/tmp"));
        state.prepared = Some(prepared);
        let size = state.adopt_size(build_metrics).unwrap();
        assert_eq!((size.cols, size.rows), at(prepared), "the other window's size");

        let mut state = State::new(PathBuf::from("/tmp"));
        state.prepared = Some(prepared);
        state.allocate(FIRST_SHOW);
        let size = state.adopt_size(build_metrics).unwrap();
        assert_eq!((size.cols, size.rows), at(FIRST_SHOW), "the pane's own allocation");

        let mut state = State::new(PathBuf::from("/tmp"));
        let size = state.adopt_size(build_metrics).unwrap();
        assert!(size.cols > 1 && size.rows > 1, "a nominal size: {size:?}");
    }

    /// The metrics a pane builds before it is given a face: the default one, at zoom 1.
    fn default_metrics(width: f32, height: f32, scale: f32) -> TerminalMetrics {
        let face = TerminalFace::default();
        TerminalMetrics::with_families(&face.families, face.base_px, width, height, scale)
    }

    /// The pane's own allocation in the GUI pass that found defect 1: 1280x234 device pixels at
    /// scale 1, which the old eagerly built metrics spawned at 13x160.
    const FIRST_SHOW: Allocation = Allocation {
        width: 1280.0,
        height: 234.0,
        scale: 1.0,
    };

    /// GUI pass 2026-09-23, defect 1: the first `Ctrl+a t` spawned the shell on a 1x1 PTY. Driven
    /// in the order GTK drives it -- `start` before the pane has any allocation, then the GLArea's
    /// first `resize`, which finds no metrics to resize, then `ensure_session` -- with the real
    /// `build_metrics`, so it fails against the placeholder build this replaced (checked: `1x1`).
    #[test]
    fn a_lazily_started_shell_spawns_at_the_panes_allocation_never_1x1() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        assert_eq!(
            state.spawn_size(build_metrics),
            None,
            "no allocation yet: nothing to spawn at"
        );
        assert_eq!(state.allocate(FIRST_SHOW), None, "no metrics yet, and no PTY to tell");

        let size = state
            .spawn_size(build_metrics)
            .expect("wanted and allocated: a shell is due")
            .expect("this machine has a monospace font");
        let expected = default_metrics(1280.0, 234.0, 1.0);
        assert_eq!(
            (size.cols, size.rows),
            (expected.cols(), expected.rows()),
            "the first PTY size is the allocation's grid"
        );
        assert!(size.cols > 1 && size.rows > 1, "never 1x1: {size:?}");
    }

    /// The scale is part of the allocation kept for the lazy build: metrics built at scale 1 on a
    /// scale-2 output would carry half-size cells into the first spawn.
    #[test]
    fn the_lazy_metrics_are_built_at_the_allocations_scale() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        let doubled = Allocation {
            width: 2560.0,
            height: 468.0,
            scale: 2.0,
        };
        state.allocate(doubled);
        let size = state.spawn_size(build_metrics).unwrap().unwrap();
        let metrics = state.metrics.as_ref().expect("built by spawn_size");
        assert_eq!(metrics.scale(), 2.0);
        let expected = default_metrics(2560.0, 468.0, 2.0);
        assert_eq!((size.cols, size.rows), (expected.cols(), expected.rows()));
    }

    /// A pointer position arrives in widget-logical pixels while the metrics
    /// are device pixels. At scale 2 a logical point in the middle of cell (row 3, col 10) must name
    /// that cell -- not (1, 5), which dividing by the device cell size gave.
    #[test]
    fn a_pointer_at_scale_2_lands_on_the_cell_it_is_over() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        state.allocate(Allocation {
            width: 2560.0,
            height: 936.0,
            scale: 2.0,
        });
        state.spawn_size(build_metrics).unwrap().unwrap();
        let metrics = state.metrics.as_ref().expect("built by spawn_size");
        assert_eq!(metrics.scale(), 2.0);
        assert!(
            metrics.rows() > 3 && metrics.cols() > 10,
            "{}x{}",
            metrics.cols(),
            metrics.rows()
        );
        let logical_w = f64::from(metrics.cell_width()) / 2.0;
        let logical_h = f64::from(metrics.cell_height()) / 2.0;
        let (x, y) = (logical_w * 10.25, logical_h * 3.5);
        assert_eq!(report_cell(&state, x, y), Some((3, 10)));
        let (cw, ch, cols, rows, top) = selection_geometry(&state).unwrap();
        assert_eq!(pointer::cell_at(x, y, cw, ch, cols, rows, top), (3, 10, false));
    }

    /// Once the metrics exist, a later allocation resizes them and is reported to the PTY -- and
    /// the latest allocation, not the first, is what a lazy build would use.
    #[test]
    fn an_allocation_after_the_metrics_exist_is_a_resize() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        state.allocate(FIRST_SHOW);
        let first = state.spawn_size(build_metrics).unwrap().unwrap();
        let taller = Allocation {
            height: FIRST_SHOW.height * 2.0,
            ..FIRST_SHOW
        };
        let resized = state.allocate(taller).expect("twice the height is more rows");
        assert!(resized.rows > first.rows, "{first:?} -> {resized:?}");
        assert_eq!(resized.cols, first.cols);
        assert_eq!(
            state.allocate(taller),
            None,
            "the same allocation again is not a change"
        );
    }

    /// A larger zoom is a larger font, so fewer cells fit: the rebuilt grid is what the PTY is told, and
    /// it is the grid the metrics now draw. The same zoom again changes nothing.
    #[test]
    fn a_zoom_with_metrics_rebuilds_them_and_resizes_the_pty() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        state.allocate(FIRST_SHOW);
        let first = state.spawn_size(build_metrics).unwrap().unwrap();
        let face = state.face.clone();
        let Refont::Changed { resize: Some(size) } = state.refont(face.clone(), 2.0) else {
            panic!("twice the size must change the grid of a {first:?} pane");
        };
        assert!(
            size.cols < first.cols && size.rows < first.rows,
            "{first:?} -> {size:?}"
        );
        let metrics = state.metrics.as_ref().unwrap();
        assert_eq!((metrics.cols(), metrics.rows()), (size.cols, size.rows));
        assert_eq!(size, PtySize::from_metrics(metrics), "the drawn grid is the PTY's grid");
        let expected = default_metrics(FIRST_SHOW.width, FIRST_SHOW.height, FIRST_SHOW.scale);
        assert!(
            (metrics.cell_height() - expected.cell_height() * 2.0).abs() <= 2.0,
            "{} vs {}",
            metrics.cell_height(),
            expected.cell_height()
        );
        assert_eq!(state.refont(face.clone(), 2.0), Refont::Same);
        assert_eq!(
            state.refont(face.clone(), 2.0 * (1.0 / 1.1) * 1.1),
            Refont::Same,
            "a zoom a float's width off is the same size"
        );
        let Refont::Changed { resize: Some(back) } = state.refont(face, 1.0) else {
            panic!("back to zoom 1 must give the first grid back");
        };
        assert_eq!(back, first);
    }

    /// A new face is a rebuild too: a larger un-zoomed size from the editor's `guifont` is fewer cells.
    #[test]
    fn a_new_face_with_metrics_rebuilds_them_at_the_current_zoom() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        state.allocate(FIRST_SHOW);
        let first = state.spawn_size(build_metrics).unwrap().unwrap();
        let expected = state_resize(&state, 1.5);
        assert_eq!(
            state.refont(state.face.clone(), 1.5),
            Refont::Changed { resize: expected }
        );
        let larger = TerminalFace {
            base_px: state.face.base_px * 2.0,
            ..state.face.clone()
        };
        let Refont::Changed { resize: Some(size) } = state.refont(larger.clone(), 1.5) else {
            panic!("a larger face must change the grid");
        };
        assert!(size.rows < first.rows, "{first:?} -> {size:?}");
        assert_eq!(state.face, larger);
        assert_eq!(state.zoom, 1.5, "the zoom stays over the new face");
    }

    /// The grid `zoom` would give the state's pane now, for comparing a refont against.
    fn state_resize(state: &State, zoom: f32) -> Option<PtySize> {
        let now = PtySize::from_metrics(state.metrics.as_ref()?);
        let built = build_metrics(state.kept_allocation(), &state.face, zoom).ok()?;
        let then = PtySize::from_metrics(&built);
        (then != now).then_some(then)
    }

    /// No metrics yet (the terminal never shown): the face and zoom are kept, no font manager is touched,
    /// and the first build is already in them.
    #[test]
    fn without_metrics_a_face_and_zoom_wait_for_the_first_build() {
        let mut state = State::new(PathBuf::from("/tmp"));
        let face = TerminalFace {
            families: vec!["monospace".into()],
            base_px: 20.0,
        };
        assert_eq!(state.refont(face.clone(), 1.5), Refont::Changed { resize: None });
        assert!(state.metrics.is_none(), "nothing built for a terminal never shown");
        assert_eq!(state.refont(face.clone(), 1.5), Refont::Same);
        assert_eq!(
            state.refont(face.clone(), f32::NAN),
            Refont::Same,
            "a bad zoom is ignored"
        );
        assert_eq!(state.refont(face.clone(), 0.0), Refont::Same);
        state.wanted = true;
        state.allocate(FIRST_SHOW);
        state.spawn_size(build_metrics).unwrap().unwrap();
        let metrics = state.metrics.as_ref().unwrap();
        assert!(
            (metrics.size() - 30.0).abs() < 1e-3,
            "built at the face's 20px times 1.5"
        );
        assert_eq!(metrics.families(), ["monospace"]);
    }

    /// Bottom-terminal phase 2: Enter after an exit starts a new shell, and what was typed for the
    /// old one while a paste was being read must not reach the new one when that paste ends.
    #[test]
    fn restarting_forgets_input_that_waited_behind_a_paste() {
        let mut state = State::new(PathBuf::from("/tmp"));
        let ticket = state.queue.begin_paste();
        let typed = NormalizedInput::Paste {
            text: "for the old shell".to_string(),
            bracketed: false,
        };
        assert_eq!(
            state.queue.submit(typed.clone().into()),
            None,
            "it waits behind the paste"
        );
        state.exited = true;
        state.forget_session();
        assert!(!state.exited);
        assert!(
            !state.finish_paste(ticket, None),
            "that paste is gone with the old shell"
        );
        assert_eq!(
            state.queue.submit(typed.clone().into()),
            Some(typed.into()),
            "nothing waits any more"
        );
    }

    /// Bottom-terminal phase 3c: a restart forgets the live mouse mode too -- a program that turned
    /// mouse reporting on must not have its old mode outlive it into a fresh shell.
    #[test]
    fn restarting_forgets_the_mouse_mode() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.mouse = MouseModes {
            report: true,
            alt_screen: true,
            alternate_scroll: true,
        };
        state.forget_session();
        assert_eq!(state.mouse, MouseModes::default());
    }

    /// A program that turned mouse reporting on and then died (`kill -9 htop`) must not leave its
    /// final screen unselectable: once the pane knows the shell has exited, routing sees the mode
    /// off, whatever the session last reported.
    #[test]
    fn routing_modes_ignores_a_stale_report_mode_once_exited() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.mouse = MouseModes {
            report: true,
            alt_screen: false,
            alternate_scroll: true,
        };
        assert!(!state.routing_modes().report, "no session yet: routing ignores it");
        state.exited = true;
        assert!(!state.routing_modes().report, "exited: routing ignores it too");
    }

    /// Bottom-terminal phase 2: a composition is drawn at the cursor the session reported with the
    /// latest frame, the candidate window follows its caret, and without one the input method is
    /// pointed at the cursor itself. Losing the keys takes the composition off the screen.
    #[test]
    fn a_composition_is_drawn_at_the_reported_cursor_and_the_candidates_follow_its_caret() {
        use terminal_render::PaintOp;
        let mut state = State::new(PathBuf::from("/tmp"));
        assert_eq!(state.ime_caret(), None, "no frame yet: nowhere to point");
        let mut screen = Screen::new(
            PtySize {
                cols: 20,
                rows: 5,
                cell_width_px: 9,
                cell_height_px: 18,
            },
            TerminalColors::default(),
        );
        screen.feed(b"$ ");
        state.frame = Some(screen.render(true));
        state.cursor = Some(screen.cursor_cell());
        assert_eq!(state.preedit_layout(), None);
        assert_eq!(state.ime_caret(), Some((0, 2)), "no composition: the cursor");
        state.ime.set_focused(true);
        state.ime.preedit_changed("ni", 2);
        let layout = state.preedit_layout().expect("a composition over a frame");
        assert!(matches!(
            layout.ops.first(),
            Some(PaintOp::FillCells {
                row: 0,
                col: 2,
                cols: 2,
                ..
            })
        ));
        assert_eq!(state.ime_caret(), Some((0, 4)), "the composition's caret");
        state.ime.set_focused(false);
        assert_eq!(state.preedit_layout(), None, "the keys left: nothing is drawn");
    }

    /// fcitx5 places the first candidate window before `preedit-*`, so it must already know the
    /// cursor location. A frame requests a new location only if the cell moved while the terminal
    /// held the keys. Changing cursor visibility alone is not movement.
    #[test]
    fn a_frame_repoints_the_input_method_only_when_the_cursor_moved_under_the_keys() {
        let cell = |row, col, visible| CursorCell { row, col, visible };
        let mut state = State::new(PathBuf::from("/tmp"));
        assert!(
            !state.take_cursor(Some(cell(0, 2, true))),
            "moved, but the keys are elsewhere"
        );
        state.focused = true;
        assert!(!state.take_cursor(Some(cell(0, 2, true))), "the same cell");
        assert!(
            !state.take_cursor(Some(cell(0, 2, false))),
            "hidden in place is not a move"
        );
        assert!(state.take_cursor(Some(cell(1, 0, true))), "moved under the keys");
        assert_eq!(state.cursor, Some(cell(1, 0, true)));
    }

    /// A frame without a cursor and a restart both clear the cursor, so composition is not drawn
    /// at a stale cell.
    #[test]
    fn an_unknown_cursor_draws_no_composition_and_points_nowhere() {
        let mut state = State::new(PathBuf::from("/tmp"));
        let mut screen = Screen::new(
            PtySize {
                cols: 20,
                rows: 5,
                cell_width_px: 9,
                cell_height_px: 18,
            },
            TerminalColors::default(),
        );
        screen.feed(b"$ ");
        state.frame = Some(screen.render(true));
        state.focused = true;
        state.ime.set_focused(true);
        state.take_cursor(Some(screen.cursor_cell()));
        state.ime.preedit_changed("ni", 0);
        assert!(state.preedit_layout().is_some());
        assert!(!state.take_cursor(None), "nothing to point at");
        assert_eq!(state.preedit_layout(), None, "no composition at a stale cell");
        assert_eq!(state.ime_caret(), None);
        state.take_cursor(Some(screen.cursor_cell()));
        state.forget_session();
        assert_eq!(state.cursor, None, "a restart forgets where the old shell's cursor was");
        assert_eq!(state.ime_caret(), None);
    }

    /// Bottom-terminal phase 2: a bell rung while the terminal was hidden comes out with the first
    /// update after it is shown again, and must not flash then; one rung on screen flashes.
    ///
    /// This only reaches `State::bell`'s `on_screen` branch: it does not
    /// drive `connect_visibility`'s map handler, which is what actually calls `pump(.., false)`
    /// before sending `Visible(true)`, nor the session's own folding of a hidden bell
    /// (`session.rs:571-576`) into one `Update` never woken for. That whole path needs a display and
    /// is the GUI pass's job; this test is the pure half only.
    #[test]
    fn a_bell_from_while_the_pane_was_hidden_does_not_flash() {
        let mut state = State::new(PathBuf::from("/tmp"));
        let now = Instant::now();
        assert_eq!(state.bell(false, now), None);
        assert!(!state.bell.active(now), "nothing shows");
        assert!(state.bell(true, now).is_some());
        assert!(state.bell.active(now));
    }

    /// The gate itself: nothing is spawned for a pane nobody asked for, and nothing twice.
    #[test]
    fn nothing_spawns_unless_wanted_and_the_font_failure_is_reported() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.allocate(FIRST_SHOW);
        assert_eq!(state.spawn_size(build_metrics), None, "not wanted");
        assert!(state.metrics.is_none(), "and no font manager touched for it");
        state.wanted = true;
        state.exited = true;
        assert_eq!(state.spawn_size(build_metrics), None, "an exited shell waits for Enter");
        state.exited = false;
        let failed = state.spawn_size(|_, _, _| Err("no fonts".to_string()));
        assert_eq!(failed, Some(Err("no fonts".to_string())));
        assert!(state.metrics.is_none(), "a failed build leaves nothing half-made");
    }

    const HAS_THE_KEYS: GtkFocus = GtkFocus {
        widget_has_focus: true,
        window_is_active: true,
    };

    /// With an input method attached every printable
    /// key arrives as a commit, never as `key-pressed`, so copy mode's letters are decided here.
    /// In copy mode a commit is a copy-mode key or nothing -- never the shell's, whatever it
    /// carries -- and once `q` has left, the next commit is typing again.
    #[test]
    fn in_copy_mode_a_commit_is_a_copy_mode_key_and_never_reaches_the_shell() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.focused = true;
        state.ime.set_focused(true);
        assert!(
            matches!(state.route_commit("k", HAS_THE_KEYS), CommitRoute::Shell(_)),
            "outside copy mode a letter is typing"
        );

        state.copy_mode = true;
        assert_eq!(
            state.route_commit("k", HAS_THE_KEYS),
            CommitRoute::CopyMode(CopyModeKey::Scroll(ScrollRequest::Lines(1)))
        );
        assert_eq!(
            state.route_commit("G", HAS_THE_KEYS),
            CommitRoute::CopyMode(CopyModeKey::Scroll(ScrollRequest::Bottom))
        );
        for swallowed in ["x", "ls", "\u{4f60}\u{597d}", "echo hi\n"] {
            assert_eq!(
                state.route_commit(swallowed, HAS_THE_KEYS),
                CommitRoute::CopyMode(CopyModeKey::Swallow),
                "{swallowed:?} reaches nothing in copy mode"
            );
        }
        assert!(
            matches!(state.route_commit("k", GtkFocus::default()), CommitRoute::Dropped(_)),
            "the gate still decides first: no keys, no key"
        );

        let leave = state.route_commit("q", HAS_THE_KEYS);
        assert_eq!(leave, CommitRoute::CopyMode(CopyModeKey::Leave));
        let CommitRoute::CopyMode(key) = leave else {
            unreachable!()
        };
        state.apply_copy_mode_key(key);
        assert!(!state.copy_mode, "q left copy mode");
        assert!(
            matches!(state.route_commit("k", HAS_THE_KEYS), CommitRoute::Shell(_)),
            "and the next letter is typing again"
        );
    }

    #[test]
    fn a_terminal_moved_in_copy_mode_keeps_its_letters_out_of_the_shell() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.focused = true;
        state.ime.set_focused(true);
        state.took_over(true);
        assert!(
            matches!(
                state.route_commit("j", HAS_THE_KEYS),
                CommitRoute::CopyMode(CopyModeKey::Scroll(_))
            ),
            "j scrolls the arrived view"
        );
        assert_eq!(
            state.route_commit("q", HAS_THE_KEYS),
            CommitRoute::CopyMode(CopyModeKey::Leave)
        );
        state.took_over(false);
        assert!(
            matches!(state.route_commit("j", HAS_THE_KEYS), CommitRoute::Shell(_)),
            "an arrival outside copy mode types"
        );
    }

    /// With rime on, a letter in copy mode starts a composition instead of committing. It is never
    /// kept or drawn, and the pane is told to reset the input method, so nothing it composes can be
    /// committed into the shell once copy mode ends. Outside copy mode a composition is kept as ever.
    #[test]
    fn a_composition_started_in_copy_mode_is_never_kept_and_is_thrown_away() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.ime.set_focused(true);
        state.copy_mode = true;
        assert!(state.take_preedit("k", 1), "a composition in copy mode is reset");
        assert!(!state.ime.composing(), "and never kept or drawn");
        assert!(!state.take_preedit("", 0), "its end asks for nothing more");

        state.copy_mode = false;
        assert!(!state.take_preedit("ni", 2));
        assert!(state.ime.composing(), "outside copy mode a composition is kept");
    }
}
