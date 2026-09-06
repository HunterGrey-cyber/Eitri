use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{DrawingArea, Overlay, Paned};

/// Horizontal split, carried over from `shell_chrome::build_content_area`'s `GtkPaned` setup, with
/// the two placeholder panes replaced by the real editor/agent widgets. `agent` is deliberately
/// *not* attached to `paned` directly -- see `install_webview_resize_throttle`'s doc for why -- so
/// the wrapper widget that helper returns becomes the actual end child instead. Returns the
/// wrapping widget plus the `Paned` itself: `shell`'s current caller (`main.rs`) has no use for it
/// yet and binds it as `_paned`, but exposing the handle costs nothing and leaves it available for
/// a future caller that needs to read or drive the divider position directly (e.g. persisting/
/// restoring the split ratio, or a future command that programmatically resizes a pane) instead of
/// only reacting to `notify::position` from outside.
pub(crate) fn build_content_area(editor: &gtk4::Widget, agent: &gtk4::Widget) -> (gtk4::Widget, Paned) {
    let paned = Paned::new(gtk4::Orientation::Horizontal);
    paned.add_css_class("content-area");
    paned.set_vexpand(true);
    paned.set_hexpand(true);
    paned.set_wide_handle(true);

    let agent_host = install_webview_resize_throttle(agent);

    paned.set_start_child(Some(editor));
    paned.set_end_child(Some(&agent_host));
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_position(760);

    let widget = paned.clone().upcast();
    (widget, paned)
}

/// P7 quick-win: `WebKitWebProcess` was measured pegging ~108% CPU (over one full core) during a
/// resize sweep, because every single `notify::position` change on the paned reallocates -- and
/// so reflows -- the WebView in real time. There's no public GtkPaned/WebKitGTK API to throttle
/// that reflow rate directly.
///
/// **2026-09-06 rewrite.** The first version of this fix called `agent.set_visible(false)` for the
/// duration of a drag burst and `set_visible(true)` ~70ms after the last position change (a hidden
/// widget isn't allocated/painted, so WebKit doesn't reflow it). That traded the CPU problem for a
/// real, separate visual bug: with only one of `Paned`'s two children visible, `GtkPaned` gives
/// that lone child the *entire* paned width and draws no handle at all -- this is standard
/// `GtkPaned` behavior for a single-visible-child paned, not something this crate's code caused.
/// Any real drag (mouse or synthesized) produces `notify::position` events well under 70ms apart,
/// so the WebView stayed hidden -- and the editor pane filled the whole window, divider and
/// WebView gone -- for the entire held-and-moving duration of a drag, snapping back only once
/// motion paused or the button was released. (`Paned::position()` itself tracked the real cursor
/// throughout the hidden period, which is exactly why it always self-corrected instead of getting
/// stuck.) See `MANUAL_VERIFICATION.md` and `docs/neovibe_feasibility_status.md`'s P7 section for
/// the full repro/root-cause writeup.
///
/// This version never touches `agent`'s visibility, and never lets `agent` itself be a real,
/// currently-allocated `Paned` child at all -- so `Paned`'s own two-visible-children width math
/// can't collapse, regardless of how fast position changes arrive:
/// - `agent` becomes a manually-placed *overlay child* of a `gtk4::Overlay` (`halign`/`valign`
///   left at `Start`, so `Overlay` does not auto-stretch it to the overlay's own size -- see
///   `gtk_overlay_add_overlay()`'s docs).
/// - An empty `gtk4::DrawingArea` "sensor" (drawn to only in the sense of never having a draw
///   func set at all -- it exists purely for its cheap, standard `resize` signal) is the
///   `Overlay`'s *main* child, and it's the `Overlay` -- not `agent` -- that `build_content_area`
///   gives to `Paned` as the real end child. `Overlay` always gives its main child the overlay's
///   full, real-time allocation no matter what any overlay child is doing, so from `Paned`'s point
///   of view its end child is an ordinary, always-correctly-sized visible widget throughout any
///   drag or plain window resize.
/// - The sensor's `resize` signal -- emitted synchronously by GTK every time `Paned` (via
///   `Overlay`) gives it a new size -- is what actually drives `agent`'s size, via
///   `set_size_request`, debounced by the same `DEBOUNCE_MS` as before: at most once per burst,
///   ~70ms after the last resize in it. In between, `agent` simply keeps its last real size --
///   still visible, still painting, just not asked to reflow in real time -- which is what
///   preserves the original CPU fix. The very first resize (app startup) is applied immediately
///   rather than debounced, so the WebView is never briefly stuck at a stale/zero size before the
///   first drag ever happens. This throttle is what makes the WebView pane different from the
///   editor pane: `editor` is `paned`'s *other* child, attached directly with no such debounce, so
///   `NeovideEditorPane` (in the separate, frozen `neovide-editor` crate) receives every
///   intermediate resize during a drag immediately and in real time, with no equivalent sensor or
///   debounce logic anywhere in `shell` standing in front of it.
/// - `set_clip_overlay` guards the one edge case this design creates: mid-*shrink*-drag, `agent`
///   can briefly still be sized larger than the shrinking `Overlay` slot until the debounce catches
///   up; without clipping, that would visibly spill over into the editor pane.
///
/// Returns the `Overlay` (upcast to `Widget`) the caller must use as `Paned`'s end child -- handing
/// `agent` itself to `Paned` would bypass this whole mechanism and reintroduce the original bug.
pub(crate) fn install_webview_resize_throttle(agent: &gtk4::Widget) -> gtk4::Widget {
    const DEBOUNCE_MS: u32 = 70;

    agent.set_halign(gtk4::Align::Start);
    agent.set_valign(gtk4::Align::Start);

    let sensor = DrawingArea::new();
    sensor.set_hexpand(true);
    sensor.set_vexpand(true);

    let overlay = Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.set_child(Some(&sensor));
    overlay.add_overlay(agent);
    overlay.set_clip_overlay(agent, true);

    let agent = agent.clone();
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let primed: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    sensor.connect_resize(move |_sensor, width, height| {
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        if !primed.get() {
            // First-ever layout: size it now, not debounced, so the WebView doesn't sit at a
            // stale/zero size before the user has dragged (or resized) anything.
            primed.set(true);
            agent.set_size_request(width, height);
            return;
        }
        let agent2 = agent.clone();
        let pending2 = pending.clone();
        let id = glib::source::timeout_add_local(Duration::from_millis(DEBOUNCE_MS as u64), move || {
            agent2.set_size_request(width, height);
            *pending2.borrow_mut() = None;
            glib::ControlFlow::Break
        });
        *pending.borrow_mut() = Some(id);
    });

    overlay.upcast()
}
