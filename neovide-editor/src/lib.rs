//! neovide-editor: the embeddable Neovide/Skia/GTK4 editor surface, extracted from the
//! validated poc/neovide_embed_live probe. See docs/superpowers/specs/2026-09-06-shell-scaffolding-design.md.
//!
//! `NeovideEditorPane` is this crate's public surface: a `GtkGLArea`-backed pane that owns a
//! `LiveHarness` (real `nvim --embed` connection) and wires resize/IME/keyboard/mouse/tick/render
//! callbacks onto it -- but, unlike the `poc/neovide_embed_live` probe it's extracted from, it
//! never constructs its own `Application`/`ApplicationWindow`. An embeddable editor surface must
//! not assume it owns the top-level window: creating the `Application`/`Window`, calling
//! `.present()`, and deciding what a window-close request should do are all left to the host
//! (e.g. the shell binary that embeds this pane). The one place the reference probe *did* reach
//! for its own `window` -- the tick callback calling `window.close()` directly when
//! `LiveHarness::has_neovim_exited()` comes back true on its own (nvim quit itself, e.g. via
//! `:qa!` typed inside it, not via the host closing anything) -- is replaced here by invoking a
//! caller-registered callback (`on_exited_unrequested`), so the host decides what "nvim exited
//! unrequested" should actually do to its own window. Similarly, the reference's
//! `connect_close_request` handler (which calls `LiveHarness::shutdown()` and logs the result)
//! becomes the public `shutdown()` method, for the host's own close-request handler to call.

mod gl_interop;
mod keyboard;
mod mouse;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    EventControllerMotion, EventControllerScroll, EventControllerScrollFlags, GLArea,
    GestureClick, IMMulticontext,
};

use skia_safe::gpu::direct_contexts;
use skia_safe::Color4f;

use neovide::live_harness::{LiveHarness, LiveHarnessOptions};
use neovide::units::GridSize;

use gl_interop::{
    snap_region_to_grid,
    compute_content_region, fill_content_region, grid_size_for_content_region, make_gl_interface,
    SkiaState,
};
use mouse::DragState;

/// Log a frame-pacing line every N frames instead of spamming stdout every frame.
const LOG_EVERY_N_FRAMES: u64 = 60;

/// How many `add_tick_callback` invocations between `[tick]` summary log lines (see `TickStats`).
const TICK_LOG_EVERY_N_TICKS: u64 = 300;

/// The clear colour painted across the whole framebuffer before each `LiveHarness::render_frame`
/// call (`canvas.clear(OUTSIDE_COLOR)` in the render callback below) -- a cheap defensive clear,
/// not a colour a host can override any more (see the removal note below).
///
/// **2026-09-07:** this used to be a loud, deliberately-wrong magenta (`(0.55, 0.15, 0.55)`) so a
/// viewport-clear regression bleeding past `CONTENT_MARGIN` would be visible at a glance during
/// P0-P5's validation. That validation is done (P5: real screenshots at 6 scale factors, numeric
/// geometry cross-checks) and this pane is now embedded in the real `shell` product, where a
/// bright magenta strip around every real window reads as a bug rather than a debug aid -- a real
/// user asked "what is that purple border, can it go away" the first time they saw it. Now a
/// neutral near-black that blends with typical dark UI chrome.
///
/// **2026-09-16, established with a positive control, not just argued:** `CONTENT_MARGIN` is 0.0,
/// so there is no inset band left for this to paint, and **no pixel on screen shows it at all.**
/// `compute_content_region` returns the whole framebuffer at margin 0, and every arm of the render
/// callback repaints that entire rect on top of this clear:
///
/// - `Ready` hands `content_region` to the fork's `Renderer::draw_frame`, which `clip_rect`s to it
///   and *then* does `root_canvas.clear(default_background)` -- an `SkBlendMode::kSrc` fill of the
///   whole clip with nvim's own `Normal` background (`src/renderer/mod.rs:272-283` at the pinned
///   rev, fork commit `09b304a`, "only clear the renderer's own clipped viewport").
/// - `NotStarted`/`Starting`/`Failed` fill the same rect with `STARTING_COLOR`/`FAILED_COLOR`,
///   both opaque.
///
/// A GUI pass proved this with a positive control rather than assuming it from the code alone: a
/// magenta probe at margin 0 produced 0 pixels anywhere, including across 220 frames of a live
/// divider drag, while the identical probe at `CONTENT_MARGIN = 40` produced exactly the 40px
/// frame's own pixel count. A host-settable clear colour (`set_clear_color`) existed for one
/// release so a host could track nvim's own background in case this ever became visible again; it
/// was removed once that control confirmed the colour reaches no pixel today, and the sub-cell
/// remainder a floored `grid_size_for_content_region` leaves at the right and bottom edge lies
/// *inside* `content_region` and has always been painted by the renderer in nvim's own background,
/// not this colour. Removing the margin is what fixed the near-black frame on its own, and the
/// value also matches nothing any more: `#1a1b1f` was the background of the placeholder palette
/// `shell`'s theme pipeline replaced. If `CONTENT_MARGIN` ever becomes non-zero again, this
/// constant -- and a reason to bring back a host-settable override -- becomes reachable once more.
///
/// **2026-09-19: reachable again, exactly as the paragraph above predicted, and now only the
/// STARTING value.** `snap_region_to_grid` moves the grid's sub-cell vertical remainder to the top
/// edge so the bottom of the pane sits flush against the host's status bar, which means
/// `content_region` no longer covers the whole framebuffer and these pixels are on screen -- a thin
/// band under the top bar. A fixed near-black there would reinstate the near-black frame the margin
/// removal fixed, so [`NeovideEditorPane::set_clear_color`] is back, and `shell` drives it from the
/// same `tokens.bg` its theme pipeline already pushes at every colorscheme change. This value is
/// what the band shows for the ~190ms before the first theme payload arrives, and until a host sets
/// it at all.
const OUTSIDE_COLOR: Color4f = Color4f::new(0.102, 0.106, 0.122, 1.0);
/// Painted across the whole `content_region` while `LiveHarness` is being constructed (the one
/// real, observed blocking call in this crate) -- distinct from both `OUTSIDE_COLOR` and anything
/// the real renderer would draw, so it's obvious on screen which phase is showing.
const STARTING_COLOR: Color4f = Color4f::new(0.12, 0.12, 0.16, 1.0);
/// Painted across `content_region` if `LiveHarness::with_options` itself returned `Err` (e.g. no
/// `nvim` on `$PATH`) -- distinct from every other state color here.
const FAILED_COLOR: Color4f = Color4f::new(0.5, 0.05, 0.05, 1.0);

/// Lifecycle of the `LiveHarness` this pane drives, kept explicit (rather than a bare
/// `Option<LiveHarness>`) so the render callback can paint a "starting nvim..." placeholder frame
/// *before* the one real, observed blocking call in this crate (`LiveHarness::with_options`) runs.
/// `NotStarted` -> one placeholder frame painted+presented -> `Starting` -> next render callback
/// performs the blocking construction -> `Ready`/`Failed`.
pub enum LiveState {
    NotStarted,
    Starting,
    Ready(Box<LiveSession>),
    Failed(String),
}

pub struct LiveSession {
    pub(crate) harness: LiveHarness,
    start: Instant,
    last_frame: Instant,
    frame_count: u64,
    logged_ready: bool,
    /// `render_frame`'s own returned `animating` value, set after every render callback
    /// invocation below. Starts `true` so the tick callback keeps rendering continuously through
    /// the first few Ready-state frames, until a real `render_frame` call has actually reported a
    /// real value -- erring toward "render" rather than "skip" whenever this value hasn't been
    /// established yet.
    last_animating: Cell<bool>,
    /// `harness.redraw_batches_seen()` as of the last time either the render callback or the tick
    /// callback looked at it. The tick callback calls `LiveHarness::pump` every tick specifically
    /// so a change here is visible at full display-refresh-rate latency even on ticks that don't
    /// render -- this is the "did nvim actually send anything new" half of the fix, independent
    /// of `last_animating`.
    last_seen_batches: Cell<u64>,
    /// Set by the resize handler and the keyboard input handler: both are real external events
    /// that deserve a guaranteed next frame regardless of what `last_animating`/
    /// `last_seen_batches` currently say (a resize needs its own frame at the new size even if
    /// nvim sent nothing new; a keypress deserves a same-tick-latency render rather than waiting
    /// on nvim's async redraw round-trip to eventually move `last_seen_batches`). Read-and-cleared
    /// by the tick callback every tick.
    pub(crate) wants_frame: Cell<bool>,
    /// The grid (cols, rows) size nvim was last asked to resize to via
    /// `LiveHarness::resize_grid`. Set once at construction (to whatever the initial
    /// `content_region` computed to) and re-checked on every `connect_resize` callback so a real
    /// RPC is only sent when the *grid-cell* size actually changes, not on every pixel-level
    /// resize event mid-drag.
    last_grid_size: Cell<GridSize<u32>>,
    /// Set once the tick callback has invoked the registered `on_exited_unrequested` callback (if
    /// any) because `LiveHarness::has_neovim_exited()` came back true on its own (i.e. nvim quit
    /// itself, e.g. via `:qa!` typed inside it -- not via the host calling `shutdown()`). This is
    /// the P2 "dead-looking-alive window" bugfix, adapted for a pane that doesn't own a `Window`:
    /// `has_neovim_exited()` is monotonic (never goes back to `false`), so without this guard the
    /// tick callback would fire the callback again on every subsequent tick until the host's own
    /// teardown actually completes -- harmless in principle but noisy (a duplicate
    /// `[live] nvim exited...` log line and a duplicate callback invocation every tick in
    /// between). Read-and-set via `Cell::replace` so the check-and-flag is a single atomic step
    /// from this single-threaded GTK main-loop caller's point of view.
    ///
    /// Also set (via a plain `Cell::set(true)`, not `replace`) by `NeovideEditorPane::shutdown()`
    /// itself, for the opposite direction: a *host*-initiated shutdown must trip this same guard
    /// so a later tick doesn't independently notice `has_neovim_exited() == true` once nvim
    /// actually exits and fire `on_exited_unrequested` a second time for a shutdown the host
    /// already knows about.
    close_requested: Cell<bool>,
    /// Set on a `GestureClick` press, cleared on its matching release -- see `mouse::DragState`'s
    /// own doc for what this tracks and why.
    pub(crate) active_drag: Cell<Option<DragState>>,
    /// Running fractional (x, y) grid-line scroll accumulator: only the *change* in `floor()`
    /// between one scroll event and the next determines how many whole-grid-line `Scroll` RPCs to
    /// send (see `mouse::handle_mouse_scroll`), so this is never reset back to zero -- only ever
    /// added to, for the life of the session.
    pub(crate) scroll_position: Cell<(f32, f32)>,
    /// Widget-local logical-pixel pointer position last reported by `EventControllerMotion`'s
    /// `motion`/`enter` signals. `GtkEventControllerScroll`'s own `scroll` signal carries no
    /// position at all, so a scroll event's target grid cell has to come from here -- the last
    /// place the pointer was actually seen.
    pub(crate) last_pointer_pos: Cell<(f64, f64)>,
}

impl LiveSession {
    fn new(harness: LiveHarness, grid_size: GridSize<u32>) -> Self {
        let now = Instant::now();
        Self {
            harness,
            start: now,
            last_frame: now,
            frame_count: 0,
            logged_ready: false,
            last_animating: Cell::new(true),
            last_seen_batches: Cell::new(0),
            wants_frame: Cell::new(false),
            last_grid_size: Cell::new(grid_size),
            close_requested: Cell::new(false),
            active_drag: Cell::new(None),
            scroll_position: Cell::new((0.0, 0.0)),
            last_pointer_pos: Cell::new((0.0, 0.0)),
        }
    }

    /// Advance real elapsed time and return (dt, instantaneous_fps).
    fn tick(&mut self) -> (f32, f32) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        self.frame_count += 1;
        let fps = if dt > 0.0 { 1.0 / dt } else { 0.0 };
        (dt, fps)
    }
}

/// Counts, over rolling windows of `TICK_LOG_EVERY_N_TICKS` tick-callback invocations, how many
/// ticks actually issued a `queue_render()` vs. how many skipped it because nothing needed another
/// frame -- an idle pane should show `skipped` dominating, while a typing/scrolling/animating pane
/// should show `issued` dominating. See `poc/p11_measurements/PHASE_REPORT.md` for the bug this
/// directly addresses.
struct TickStats {
    ticks: Cell<u64>,
    issued: Cell<u64>,
    skipped: Cell<u64>,
    issued_total: Cell<u64>,
    skipped_total: Cell<u64>,
}

impl TickStats {
    fn new() -> Self {
        Self {
            ticks: Cell::new(0),
            issued: Cell::new(0),
            skipped: Cell::new(0),
            issued_total: Cell::new(0),
            skipped_total: Cell::new(0),
        }
    }

    /// Record one tick's outcome; every `TICK_LOG_EVERY_N_TICKS` ticks, print a `[tick]` summary
    /// of the just-finished window and reset the windowed counters (the `_total` counters keep
    /// accumulating for the life of the process).
    fn record(&self, issued_this_tick: bool) {
        self.ticks.set(self.ticks.get() + 1);
        if issued_this_tick {
            self.issued.set(self.issued.get() + 1);
            self.issued_total.set(self.issued_total.get() + 1);
        } else {
            self.skipped.set(self.skipped.get() + 1);
            self.skipped_total.set(self.skipped_total.get() + 1);
        }

        if self.ticks.get() >= TICK_LOG_EVERY_N_TICKS {
            let ticks = self.ticks.get();
            let issued = self.issued.get();
            let skipped = self.skipped.get();
            println!(
                "[tick] last {ticks} ticks: issued={issued} skipped={skipped} \
                 skip_ratio={:.1}% (cumulative issued={} skipped={})",
                (skipped as f64 / ticks as f64) * 100.0,
                self.issued_total.get(),
                self.skipped_total.get(),
            );
            self.ticks.set(0);
            self.issued.set(0);
            self.skipped.set(0);
        }
    }
}

/// Slot for the callback `on_exited_unrequested` registers, shared with the tick callback set up
/// in `NeovideEditorPane::new()`. Factored into a named alias purely to satisfy
/// `clippy::type_complexity` -- no behavior difference from writing the nested type out inline.
type ExitedCallbackSlot = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// The embeddable Neovide/Skia editor surface. Wraps a single `GtkGLArea` driving a real
/// `nvim --embed` connection (`neovide::live_harness::LiveHarness`), with resize/IME/keyboard/
/// mouse/tick/render callbacks all wired up by `new()`. Deliberately does not own an
/// `Application`/`ApplicationWindow` -- see this module's own doc for why -- so the host must
/// call `.widget()` to place this pane's `GLArea` into its own window, `.grab_focus()` once that
/// window is shown, and wire its own window's `connect_close_request` to `.shutdown()`.
pub struct NeovideEditorPane {
    widget: GLArea,
    /// Kept alive here (rather than only living inside the closures set up in `new()`) so
    /// `grab_focus()` can call `im_context.focus_in()` after the host actually shows/focuses this
    /// pane's widget -- mirroring the reference probe's own `gl_area.grab_focus()` +
    /// `im_context.focus_in()` pairing, which only makes sense once the widget is realized, not
    /// at construction time.
    im_context: IMMulticontext,
    live_state: Rc<RefCell<LiveState>>,
    exited_callback: ExitedCallbackSlot,
    /// Shared with the render callback. See [`NeovideEditorPane::set_clear_color`].
    clear_color: Rc<Cell<Color4f>>,
    /// The focus state the host last reported, shared with the render callback so a report that
    /// arrives before nvim has started is applied the moment the harness exists. See
    /// [`NeovideEditorPane::set_focused`].
    focused: Rc<Cell<Option<bool>>>,
}

/// Construction-time options for [`NeovideEditorPane::with_options`]. Each field is forwarded to
/// the `LiveHarnessOptions` field of the same name, so this crate stays a pass-through rather than
/// growing a `with_this_and_that` constructor per combination a host happens to need.
#[derive(Default)]
pub struct NeovideEditorPaneOptions {
    /// Launch nvim with `--clean` (no user plugins/config), for deterministic verification runs.
    pub clean: bool,
    /// Extra `(name, value)` environment variables set on the spawned `nvim --embed` child process
    /// **only** -- forwarded verbatim to `LiveHarnessOptions::child_env`, which reaches the child
    /// through `Command::env` at spawn time. This pane's own process environment is never touched,
    /// so nothing else the host spawns can observe the injection.
    ///
    /// This crate takes no view on *what* a host injects or why; it only owns the plumbing. The
    /// one real caller today is `shell`, which uses it to make the embedded nvim believe it is
    /// running inside tmux so `vim-tmux-navigator` forwards boundary-crossing `Ctrl-h`/`Ctrl-l`
    /// out to the host (see `shell/src/pane_switch.rs`). Values are applied on top of the
    /// inherited environment, so a host prepending to `PATH` must compose the whole value itself.
    pub child_env: Vec<(String, String)>,
    /// Working directory for the spawned `nvim --embed` child. `None` inherits this process's own
    /// cwd, which is exactly what `None` means to `LiveHarnessOptions::cwd`.
    ///
    /// A host that resolves a project root for its other panes must set this too: an editor left
    /// on the process cwd while the agent panel and terminal were pointed somewhere else is a
    /// window whose three panes disagree about which project is open, which is worse than any of
    /// them being "wrong" consistently.
    pub cwd: Option<std::path::PathBuf>,
    /// Extra arguments for the `nvim` binary itself, forwarded to
    /// `LiveHarnessOptions::extra_nvim_args` after `--clean` (when `clean` is set).
    ///
    /// Like `child_env`, this crate takes no view on what a host passes. `shell` uses it for one
    /// `--cmd` that loads its theme feed (see `shell/src/theme/feed.rs`); `--cmd` runs before the
    /// user's own config, so what it installs is autocommands, not settings.
    pub extra_nvim_args: Vec<String>,
}

/// The `--cmd` that makes the editor draw **no cursor** while it does not have the keys.
///
/// `g:neovide_cursor_unfocused_outline_width` is Neovide's own setting (default 1/8 em, a hollow
/// block). The fork (`0862c35`) makes a width `<= 0` mean "draw nothing while unfocused, any
/// shape" -- stock Neovide at width 0 still leaves an anti-aliased sliver at fractional cell sizes
/// (measured: 33 pixels at scale 1.5). The owner asked for no cursor at all, 2026-09-19.
///
/// It is a `--cmd`, so it runs before the user's own config: a user who wants the hollow block
/// back sets the variable in their `init.lua` (e.g. `vim.g.neovide_cursor_unfocused_outline_width
/// = 0.125`) and wins. Neovide reads a `--cmd` value once, in `settings.read_initial_values`, BEFORE
/// `ui_attach`; `init.lua` runs AT `ui_attach`, so its value, like a runtime `:let`, reaches the
/// renderer by the other path -- the `WatchGlobal` dict watcher and `setting_changed`. Both paths are
/// proved on real frames by `tests/unfocused_cursor.rs`: a later `--cmd`, and a `:let` typed after
/// nvim is ready. A real `init.lua` itself was not loaded (the test runs `--clean`); the runtime
/// `:let` stands in for it because it takes the same watcher path.
pub const HIDE_UNFOCUSED_CURSOR_CMD: &str = "let g:neovide_cursor_unfocused_outline_width = 0";

/// The whole argument list this pane hands to `LiveHarnessOptions::extra_nvim_args`: `--clean`
/// if asked for, then this pane's own default (`HIDE_UNFOCUSED_CURSOR_CMD`), then the host's
/// extras -- so a host's `--cmd` can override the default too.
fn nvim_args(clean: bool, extra: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(extra.len() + 3);
    if clean {
        args.push("--clean".to_string());
    }
    args.push("--cmd".to_string());
    args.push(HIDE_UNFOCUSED_CURSOR_CMD.to_string());
    args.extend(extra.iter().cloned());
    args
}

impl NeovideEditorPane {
    /// The colour painted where the editor's own grid does not reach -- today exactly one place:
    /// the band at the TOP that `gl_interop::snap_region_to_grid` creates by moving the sub-cell
    /// remainder off the bottom edge.
    ///
    /// **A host that does not call this gets `OUTSIDE_COLOR`, and on a light colorscheme that is
    /// visibly wrong** -- a near-black line under the top bar. This is not an optional refinement;
    /// it is the other half of the snap. `shell` calls it from the same theme listener that already
    /// drives its GTK CSS and the agent panel, so the band is nvim's own `Normal` background and
    /// therefore indistinguishable from the first text row below it.
    ///
    /// Takes straight RGB bytes rather than a `Color4f` so a host does not need `skia_safe` in its
    /// own dependency graph to call it; alpha is always opaque, since a translucent band would let
    /// through whatever GTK last painted there.
    pub fn set_clear_color(&self, (r, g, b): (u8, u8, u8)) {
        self.clear_color.set(Color4f::new(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            1.0,
        ));
        self.widget.queue_render();
    }

    /// Builds the `GtkGLArea` and wires every resize/IME/keyboard/mouse/tick/render callback onto
    /// it and the shared `Rc<RefCell<LiveState>>` -- everything `poc/neovide_embed_live::build_ui`
    /// did except constructing an `Application`/`ApplicationWindow` and calling
    /// `.present()`/`.grab_focus()` (the host does those, via `.widget()`/`.grab_focus()` below).
    /// `clean` is forwarded to `LiveHarnessOptions::extra_nvim_args` as `--clean`, for
    /// deterministic manual-verification runs; everything else takes its default. Use
    /// [`NeovideEditorPane::with_options`] to set anything more.
    pub fn new(clean: bool) -> Self {
        Self::with_options(NeovideEditorPaneOptions { clean, ..Default::default() })
    }

    /// Same as [`NeovideEditorPane::new`], with every construction-time knob this pane forwards to
    /// `LiveHarnessOptions` spelled out. See [`NeovideEditorPaneOptions`] for what each one means.
    pub fn with_options(options: NeovideEditorPaneOptions) -> Self {
        let NeovideEditorPaneOptions { clean, child_env, cwd, extra_nvim_args } = options;
        let gl_area = GLArea::builder()
            .hexpand(true)
            .vexpand(true)
            .has_stencil_buffer(true)
            .auto_render(true)
            .focusable(true)
            .can_focus(true)
            .build();

        let skia_state: Rc<RefCell<Option<SkiaState>>> = Rc::new(RefCell::new(None));
        let live_state: Rc<RefCell<LiveState>> = Rc::new(RefCell::new(LiveState::NotStarted));
        let clear_color = Rc::new(Cell::new(OUTSIDE_COLOR));
        let focused: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let exited_callback: ExitedCallbackSlot = Rc::new(RefCell::new(None));

        // --- resize: GtkGLArea's FBO can be resized/recreated under us, so drop the cached
        // Surface and let the next render() rebuild it against the new framebuffer dimensions
        // (device pixels, not logical widget units). Also the P2 frozen-scroll-bug fix:
        // recompute the grid size that actually fits the new content_region and, if it differs
        // (in grid *cells*, not raw pixels) from what nvim was last told, call
        // `LiveHarness::resize_grid` so nvim's own viewport tracks the real host size.
        {
            let skia_state = skia_state.clone();
            let live_state = live_state.clone();
            gl_area.connect_resize(move |widget, width, height| {
                let mut live = live_state.borrow_mut();
                let (forced_next_frame, resized_grid) = if let LiveState::Ready(session) = &mut *live {
                    session.wants_frame.set(true);
                    let content_region = snap_region_to_grid(
                        &compute_content_region(width, height),
                        session.harness.grid_scale(),
                    );
                    let new_grid_size = grid_size_for_content_region(&session.harness, &content_region);
                    let resized_grid = if new_grid_size != session.last_grid_size.get() {
                        session.harness.resize_grid(new_grid_size);
                        session.last_grid_size.set(new_grid_size);
                        Some(new_grid_size)
                    } else {
                        None
                    };
                    (true, resized_grid)
                } else {
                    (false, None)
                };
                drop(live);
                println!(
                    "[resize] fb={}x{}px scale_factor={} logical={}x{} forced_next_frame={} \
                     resized_grid={:?}",
                    width,
                    height,
                    widget.scale_factor(),
                    widget.width(),
                    widget.height(),
                    forced_next_frame,
                    resized_grid.map(|g| (g.width, g.height)),
                );
                let mut state = skia_state.borrow_mut();
                match state.as_mut() {
                    Some(state) => {
                        state.fb_width = width;
                        state.fb_height = height;
                        state.surface = None; // force rebuild on next render
                    }
                    None => {
                        // GrContext not created yet; render() will pick up the GLArea's current
                        // size directly.
                    }
                }
            });
        }

        // --- IME + keyboard input: see `keyboard::attach_keyboard_input`'s own doc for exactly
        // what this wires onto `gl_area` (GtkIMMulticontext composition + GtkEventControllerKey
        // plain-text input). The constructed `IMMulticontext` is handed back so it can be stored
        // in `Self::im_context` below for `grab_focus()` to use later.
        let im_context = keyboard::attach_keyboard_input(&gl_area, &live_state);

        // --- mouse input: three GTK4 controllers on the GLArea for click/drag-selection/
        // scroll-wheel. All three share the exact content_region/grid-scale coordinate math
        // `gl_interop::current_content_region`/`gl_interop::pixel_to_grid_pos` already provide,
        // and all three unconditionally target `LiveHarness`'s single base grid.
        {
            let click = GestureClick::new();
            click.set_button(0);
            {
                let live_state = live_state.clone();
                let gl_area = gl_area.clone();
                click.connect_pressed(move |gesture, _n_press, x, y| {
                    mouse::handle_mouse_button(&live_state, &gl_area, gesture, x, y, true);
                });
            }
            {
                let live_state = live_state.clone();
                let gl_area = gl_area.clone();
                click.connect_released(move |gesture, _n_press, x, y| {
                    mouse::handle_mouse_button(&live_state, &gl_area, gesture, x, y, false);
                });
            }
            gl_area.add_controller(click);
        }

        {
            let motion = EventControllerMotion::new();
            {
                let live_state = live_state.clone();
                let gl_area = gl_area.clone();
                motion.connect_motion(move |controller, x, y| {
                    mouse::handle_mouse_motion(&live_state, &gl_area, controller, x, y);
                });
            }
            {
                let live_state = live_state.clone();
                let gl_area = gl_area.clone();
                motion.connect_enter(move |controller, x, y| {
                    mouse::handle_mouse_motion(&live_state, &gl_area, controller, x, y);
                });
            }
            gl_area.add_controller(motion);
        }

        {
            let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
            let live_state = live_state.clone();
            let gl_area_for_scroll = gl_area.clone();
            scroll.connect_scroll(move |controller, dx, dy| {
                mouse::handle_mouse_scroll(&live_state, &gl_area_for_scroll, controller, dx, dy)
            });
            gl_area.add_controller(scroll);
        }

        // --- render: build (lazily) and drive one LiveHarness frame every tick. See the
        // `LiveState` doc for why construction is deliberately split across two render callbacks
        // instead of happening inline here.
        {
            let skia_state = skia_state.clone();
            let live_state = live_state.clone();
            // `child_env` and `cwd` are moved into this closure by the `move` below -- neither is
            // needed again in this function, and this closure is the only place `LiveHarness` is
            // ever constructed.
            let clear_color = clear_color.clone();
            let focused = focused.clone();
            gl_area.connect_render(move |widget, _gl_ctx| {
                let mut state_slot = skia_state.borrow_mut();

                if state_slot.is_none() {
                    let interface = make_gl_interface();
                    let gr_context = direct_contexts::make_gl(interface, None)
                        .expect("failed to create Skia GL DirectContext");
                    let width = widget.width() * widget.scale_factor();
                    let height = widget.height() * widget.scale_factor();
                    println!(
                        "[init] Skia DirectContext created; initial fb={}x{}px scale_factor={}",
                        width,
                        height,
                        widget.scale_factor()
                    );
                    *state_slot = Some(SkiaState { gr_context, surface: None, fb_width: width, fb_height: height });
                }

                let state = state_slot.as_mut().unwrap();
                // Must come before anything draws this frame: GTK shares this GL context with us
                // and rebinds textures behind Skia's back (on a resize in particular), which makes
                // every glyph paint as a solid block until Skia is told to distrust its state
                // cache. See `SkiaState::invalidate_cached_gl_state` for the full root-cause
                // record and the measurements behind doing this every frame rather than only on
                // resize frames.
                state.invalidate_cached_gl_state();
                state.ensure_surface();

                let Some(surface) = state.surface.as_mut() else {
                    return glib::Propagation::Stop;
                };

                let (fb_w, fb_h) = (state.fb_width, state.fb_height);
                let content_region = compute_content_region(fb_w, fb_h);
                let canvas = surface.canvas();

                // Cheap defensive clear, predating this work. Paint the entire framebuffer
                // `OUTSIDE_COLOR` first, then hand only `content_region` to whatever's actually
                // drawing this frame. At CONTENT_MARGIN 0 those two are the same rect and *every*
                // arm below repaints all of it -- the renderer clip-clears `content_region` to
                // nvim's own background, the placeholder arms fill it opaquely -- so nothing this
                // clear writes survives the frame today; see `OUTSIDE_COLOR`'s own doc for the
                // positive-control evidence.
                canvas.clear(clear_color.get());

                let mut live = live_state.borrow_mut();
                match &mut *live {
                    LiveState::NotStarted => {
                        // Paint+present a placeholder frame *before* the blocking
                        // LiveHarness::with_options call below ever runs (that call happens on
                        // the *next* render callback, once this state transition has actually
                        // been presented to the compositor).
                        println!(
                            "[live] showing 'starting nvim...' placeholder; LiveHarness::with_options \
                             will run (and block this thread) on the next frame"
                        );
                        fill_content_region(canvas, &content_region, STARTING_COLOR);
                        *live = LiveState::Starting;
                    }
                    LiveState::Starting => {
                        fill_content_region(canvas, &content_region, STARTING_COLOR);

                        let os_scale_factor = widget.scale_factor() as f64;
                        let options = LiveHarnessOptions {
                            os_scale_factor,
                            extra_nvim_args: nvim_args(clean, &extra_nvim_args),
                            // Cloned rather than moved because this closure is `Fn` and runs on
                            // every frame -- but this arm is the one-shot construction pass, so
                            // the clone happens exactly once per pane.
                            child_env: child_env.clone(),
                            cwd: cwd.clone(),
                            // Off, unconditionally, for this embedding -- the opposite of the
                            // `LiveHarnessOptions` default, and the one place this crate takes a
                            // view rather than passing a host's choice through.
                            //
                            // Neovide's startup-message capture attaches with `ext_messages` on
                            // and, on the first flush, restores the built-in message UI by writing
                            // the *pre-attach* `cmdheight` back. That value is the stock `1`: nvim
                            // has not read the user's config when it is sampled. For a config that
                            // externalises the cmdline itself (noice.nvim, which LazyVim ships),
                            // the `1` lands on top of the `0` that config chose and nvim then
                            // reserves a row nothing ever paints -- measured on this pane as a
                            // one-cell band of nvim's own `Normal` background along the bottom,
                            // 44px at a 44px cell, sitting above the shell's status bar and never
                            // filling.
                            //
                            // Turning the capture off is not just a smaller evil here, it is the
                            // right shape: `ext_messages` is set only inside the same branch that
                            // reads the pre-attach `cmdheight`, so opting out leaves nvim's own
                            // message UI in place rather than externalising messages with nothing
                            // to restore them. Verified at the nvim protocol level against both a
                            // cmdline-externalising config and a plain one with no message handler
                            // at all: the plain config keeps a real, functional command line (still
                            // `cmdheight=1`, still painted -- an `:echomsg` reaches the built-in
                            // message grid either way), and the externalising config gets the row
                            // back. What is given up is narrow and was measured too: an error
                            // raised while loading the config is painted onto nvim's own message
                            // grid, where it can want a keypress to dismiss, instead of being held
                            // and replayed after the first frame.
                            startup_message_capture: false,
                            ..Default::default()
                        };
                        println!(
                            "[live] constructing LiveHarness::with_options(os_scale_factor={os_scale_factor}, \
                             clean={clean}, cwd={cwd:?}) -- this performs a real, synchronous nvim \
                             launch and WILL block the GTK main loop until it returns"
                        );
                        let t0 = Instant::now();
                        match LiveHarness::with_options(options) {
                            Ok(mut harness) => {
                                let elapsed = t0.elapsed();
                                println!(
                                    "[live] LiveHarness::with_options returned after {elapsed:?} \
                                     (blocked the GTK main loop for that long)"
                                );
                                // The P2 frozen-scroll-bug fix: `LiveHarnessOptions::grid_size`
                                // (left `None` above, so `DEFAULT_GRID_SIZE` 100x50) only ever
                                // sets nvim's grid size at `nvim_ui_attach` time and has no
                                // relationship to this pane's actual `content_region` -- resize
                                // it immediately to what the real content_region fits.
                                let grid_size = grid_size_for_content_region(&harness, &content_region);
                                println!(
                                    "[live] resizing nvim grid to {}x{} to match initial \
                                     content_region ({}x{}px) -- fixes the P2 frozen-scroll bug \
                                     (nvim's grid no longer stays stuck at LiveHarnessOptions' \
                                     launch-time default)",
                                    grid_size.width,
                                    grid_size.height,
                                    (content_region.max.x - content_region.min.x) as i32,
                                    (content_region.max.y - content_region.min.y) as i32,
                                );
                                harness.resize_grid(grid_size);
                                // A focus report can arrive before nvim exists -- the host tracks
                                // GTK focus from the first frame. Hand it over now, or the cursor
                                // would stay solid until the next focus change.
                                if let Some(state) = focused.get() {
                                    harness.set_focused(state);
                                }
                                *live = LiveState::Ready(Box::new(LiveSession::new(harness, grid_size)));
                            }
                            Err(err) => {
                                let elapsed = t0.elapsed();
                                let message = format!("{err:#}");
                                println!(
                                    "[live] LiveHarness::with_options failed after {elapsed:?}: {message}"
                                );
                                *live = LiveState::Failed(message);
                            }
                        }
                    }
                    LiveState::Ready(session) => {
                        let (dt, fps) = session.tick();
                        // Snapped HERE rather than where `content_region` is computed above,
                        // because the two users want different rects. The grid must sit on whole
                        // cells (see `snap_region_to_grid`), while the placeholder arms below have
                        // no grid at all and should cover every pixel they can -- a "starting
                        // nvim..." screen with a band of clear colour along one edge would be a
                        // regression, not a fix.
                        let grid_region =
                            snap_region_to_grid(&content_region, session.harness.grid_scale());
                        let animating =
                            session.harness.render_frame(canvas, Some(&grid_region), dt);
                        // Share this frame's "do we still need more frames" signals with the tick
                        // callback.
                        session.last_animating.set(animating);
                        session.last_seen_batches.set(session.harness.redraw_batches_seen());

                        if !session.logged_ready && session.harness.is_ready() {
                            session.logged_ready = true;
                            println!(
                                "[live] LiveHarness reports is_ready()=true after {:?} \
                                 ({} redraw batch(es) applied) -- first real nvim content should be \
                                 visible now",
                                session.start.elapsed(),
                                session.harness.redraw_batches_seen()
                            );
                        }

                        if session.frame_count.is_multiple_of(LOG_EVERY_N_FRAMES) {
                            let elapsed = session.start.elapsed().as_secs_f32();
                            println!(
                                "[frame {:>6}] t={:>7.2}s dt={:>6.2}ms instant_fps={:>6.1} avg_fps={:>6.1} \
                                 fb={}x{} region={}x{}@({},{}) animating={} ready={} batches={} \
                                 nvim_exited={}",
                                session.frame_count,
                                elapsed,
                                dt * 1000.0,
                                fps,
                                session.frame_count as f32 / elapsed.max(0.0001),
                                fb_w,
                                fb_h,
                                (content_region.max.x - content_region.min.x) as i32,
                                (content_region.max.y - content_region.min.y) as i32,
                                content_region.min.x as i32,
                                content_region.min.y as i32,
                                animating,
                                session.harness.is_ready(),
                                session.harness.redraw_batches_seen(),
                                session.harness.has_neovim_exited(),
                            );
                        }
                    }
                    LiveState::Failed(message) => {
                        fill_content_region(canvas, &content_region, FAILED_COLOR);
                        let _ = message; // already logged once when the transition happened
                    }
                }
                drop(live);

                state.gr_context.flush_and_submit();

                glib::Propagation::Stop
            });
        }

        // --- drive redraws off the display's frame clock, tying frame pacing to actual
        // vsync-reported timing rather than a fixed timer -- but, per the P11 fix, only actually
        // calls `queue_render()` when something genuinely needs another frame.
        {
            let live_state = live_state.clone();
            let gl_area_for_tick = gl_area.clone();
            // Needed for the nvim-exited-on-its-own case just below: invoking the registered
            // `exited_callback` may, on the host side, lead straight back into `shutdown()` (e.g.
            // if the callback closes the host's own window, whose `connect_close_request` handler
            // calls `pane.shutdown()`), which itself needs `live_state.borrow_mut()` -- so this
            // closure must never invoke the callback while still holding `live` borrowed, or
            // that reentrant borrow would hit a `RefCell` `BorrowMutError` panic. See
            // `should_fire_exited_callback` below for how that's kept safe.
            let exited_callback_for_tick = exited_callback.clone();
            let tick_stats = Rc::new(TickStats::new());
            gl_area.add_tick_callback(move |widget, _clock| {
                let mut live = live_state.borrow_mut();
                // Set from inside the `Ready` arm below, then acted on only *after* `live` is
                // dropped -- see the comment on `exited_callback_for_tick` above for why the
                // ordering matters.
                let mut should_fire_exited_callback = false;
                let issued = match &mut *live {
                    LiveState::Ready(session) => {
                        // Cheap, non-blocking drain of any nvim redraw traffic that arrived since
                        // the last tick.
                        session.harness.pump(Duration::ZERO);
                        let batches_now = session.harness.redraw_batches_seen();
                        let new_content = batches_now != session.last_seen_batches.get();
                        if new_content {
                            session.last_seen_batches.set(batches_now);
                        }

                        // Resize-resync safety net (found 2026-09-07 on real GNOME/Mutter
                        // hardware, via `shell`, not the headless-sway sandbox every earlier
                        // resize check ran in): `connect_resize` above is the only other place
                        // that calls `resize_grid`, and it only fires on a `GtkGLArea` `resize`
                        // signal. On this real desktop, a window shown already-maximized (or
                        // resized by the compositor while `LiveHarness::with_options`'s
                        // synchronous, main-loop-blocking nvim launch was in flight) can settle at
                        // its final size without ever emitting a *further* `resize` signal after
                        // the Ready-time snapshot the constructor already takes -- leaving nvim's
                        // grid permanently sized for a stale, smaller content_region than the
                        // widget's real one, visible as an undrawn band past the grid's real edge.
                        // (This comment said "leftover `OUTSIDE_COLOR`" until 2026-09-16; that was
                        // never right -- `draw_frame` clip-clears the *current* content_region to
                        // nvim's own background, so the band is that background, not the host's
                        // clear colour.) Re-deriving the grid size from the widget's *current*
                        // actual size on every tick (cheap: a handful of integer ops, no
                        // allocation) and only calling `resize_grid` -- which actually costs --
                        // when it disagrees with `last_grid_size` makes this self-correcting
                        // regardless of which exact GTK/Wayland/compositor timing produced the
                        // staleness, instead of chasing that one root cause.
                        let width = widget.width() * widget.scale_factor();
                        let height = widget.height() * widget.scale_factor();
                        let content_region = snap_region_to_grid(
                            &compute_content_region(width, height),
                            session.harness.grid_scale(),
                        );
                        let new_grid_size = grid_size_for_content_region(&session.harness, &content_region);
                        let grid_resynced = if new_grid_size != session.last_grid_size.get() {
                            session.harness.resize_grid(new_grid_size);
                            session.last_grid_size.set(new_grid_size);
                            println!(
                                "[tick] resize-resync: grid was stale for the widget's current \
                                 size (fb={width}x{height}px) -- corrected to {}x{}",
                                new_grid_size.width, new_grid_size.height,
                            );
                            true
                        } else {
                            false
                        };

                        // Read-and-clear: a resize or a keypress since the last tick each force
                        // exactly one more frame, on top of the ongoing-animation and new-content
                        // signals above.
                        let wants_frame = session.wants_frame.replace(false) || grid_resynced;

                        // The P2 "dead-looking-alive pane" bugfix: `shutdown()` already calls
                        // `LiveHarness::shutdown()` when the host closes its own window, but
                        // nothing previously reacted when nvim exits *on its own* (e.g. `:qa!`
                        // typed inside it). `pump()` just above is what actually observes a real
                        // `UserEvent::NeovimExited` arriving asynchronously, so checking right
                        // here, every tick, catches it at the same latency the frame log already
                        // did. `close_requested.replace(true)` is the one-shot guard documented on
                        // `LiveSession::close_requested`; only the tick that flips it false->true
                        // actually fires the callback.
                        if session.harness.has_neovim_exited() && !session.close_requested.replace(true)
                        {
                            should_fire_exited_callback = true;
                        }

                        session.last_animating.get() || new_content || wants_frame
                    }
                    // NotStarted/Starting: the placeholder-frame dance and the one blocking
                    // `LiveHarness::with_options` call both happen *inside* the render callback
                    // and only run when a render is actually requested -- keep rendering
                    // continuously here so that state machine can advance. Failed: a rare
                    // terminal state; keep rendering rather than risk the one remaining
                    // Starting->Failed state-transition frame never actually getting painted.
                    LiveState::NotStarted | LiveState::Starting | LiveState::Failed(_) => true,
                };
                drop(live);

                if should_fire_exited_callback {
                    println!(
                        "[live] nvim exited on its own (not via a caller-initiated shutdown) -- \
                         invoking the registered on_exited_unrequested callback (if any) instead \
                         of closing a window this pane doesn't own"
                    );
                    if let Some(cb) = exited_callback_for_tick.borrow().as_ref() {
                        cb();
                    }
                }

                if issued {
                    gl_area_for_tick.queue_render();
                }
                tick_stats.record(issued);

                glib::ControlFlow::Continue
            });
        }

        Self { widget: gl_area, im_context, live_state, exited_callback, clear_color, focused }
    }

    /// The `GtkGLArea` this pane renders into. The host places this into its own window (e.g. as
    /// an `ApplicationWindow`'s child) -- this pane never does so itself.
    pub fn widget(&self) -> &gtk4::GLArea {
        &self.widget
    }

    /// Grabs keyboard focus for this pane's `GLArea` and tells the IME it's now the focused
    /// widget -- call this once the host's window is actually shown (mirroring the reference
    /// probe's own `window.present(); gl_area.grab_focus(); im_context.focus_in();` sequence,
    /// minus the `.present()`, which is the host's own window's job).
    /// Tells the editor whether it has the keyboard, so it can say so through its cursor: a solid
    /// block when it has focus, **no cursor at all** when it does not (`HIDE_UNFOCUSED_CURSOR_CMD`;
    /// a user who sets the width back gets upstream's hollow block). It forwards to
    /// `LiveHarness::set_focused` (fork `8f043a2`), which also tells nvim through
    /// `nvim_ui_set_focus`. Before this, nvim never heard about focus at all, and nothing fired
    /// `FocusGained` or `FocusLost`.
    ///
    /// This is a separate call from [`grab_focus`](Self::grab_focus), and the host decides what
    /// "focused" means. `shell` passes `true` only while this pane holds the window's focus widget
    /// AND the window is active, so alt-tabbing away hides the cursor too. It may be called before nvim has started, and on every focus event:
    /// repeats are dropped by the harness.
    pub fn set_focused(&self, focused: bool) {
        self.focused.set(Some(focused));
        if let LiveState::Ready(session) = &mut *self.live_state.borrow_mut() {
            session.harness.set_focused(focused);
            session.wants_frame.set(true);
        }
        self.widget.queue_render();
    }

    pub fn grab_focus(&self) {
        self.widget.grab_focus();
        self.im_context.focus_in();
    }

    /// Registers a callback fired (at most once) when the tick callback observes that nvim
    /// exited on its own -- i.e. `LiveHarness::has_neovim_exited()` became true without the host
    /// ever having called `shutdown()` first (e.g. `:qa!` typed inside nvim). The reference probe
    /// handled this by calling `window.close()` directly; since this pane doesn't own a `Window`,
    /// the host registers whatever it wants done instead (closing its own window, showing a
    /// dialog, etc).
    pub fn on_exited_unrequested(&self, callback: impl Fn() + 'static) {
        *self.exited_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Cleanly shuts down the live `nvim --embed` connection, if one was ever started. Mirrors
    /// the reference probe's `connect_close_request` handler body exactly: the host wires this to
    /// its own window's `connect_close_request`. Returns `LiveHarness::shutdown()`'s own return
    /// value (`true` if a real `NeovimExited` was observed, `false` if it timed out waiting for
    /// one and the nvim child may be orphaned) -- or `true` if `LiveState` never reached `Ready`
    /// at all, since there is no live connection to shut down in that case.
    pub fn shutdown(&self) -> bool {
        let mut live = self.live_state.borrow_mut();
        if let LiveState::Ready(session) = &mut *live {
            // Set before calling `LiveHarness::shutdown()` below so the tick callback's own
            // `close_requested.replace(true)` guard (see `LiveSession::close_requested`'s doc) is
            // already tripped by the time nvim actually exits -- without this, a host-initiated
            // shutdown wouldn't stop a later tick from independently noticing
            // `has_neovim_exited() == true` and firing `on_exited_unrequested` a second time, for
            // a shutdown the host itself already initiated.
            session.close_requested.set(true);
            println!("[live] shutdown() called: calling LiveHarness::shutdown()...");
            let exited_cleanly = session.harness.shutdown();
            println!(
                "[live] LiveHarness::shutdown() returned {exited_cleanly} \
                 ({})",
                if exited_cleanly {
                    "real NeovimExited observed"
                } else {
                    "timed out waiting for NeovimExited -- nvim child may be orphaned, see \
                     LiveHarness::shutdown's own doc"
                }
            );
            exited_cleanly
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::nvim_args;

    #[test]
    fn clean_comes_first_then_the_cursor_default_then_the_hosts_extras() {
        let hide = super::HIDE_UNFOCUSED_CURSOR_CMD;
        let extra = vec!["--cmd".to_string(), "lua print(1)".to_string()];
        assert_eq!(
            nvim_args(true, &extra),
            vec!["--clean", "--cmd", hide, "--cmd", "lua print(1)"]
        );
        assert_eq!(nvim_args(false, &extra), vec!["--cmd", hide, "--cmd", "lua print(1)"]);
        assert_eq!(nvim_args(true, &[]), vec!["--clean", "--cmd", hide]);
        assert_eq!(nvim_args(false, &[]), vec!["--cmd", hide]);
    }
}
