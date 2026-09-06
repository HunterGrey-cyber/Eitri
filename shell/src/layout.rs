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

/// Wraps the WebView in an overlay to debounce resize reflows. Reallocating it on every
/// `notify::position` change caused high CPU use during divider drags.
///
/// When `agent` was a direct `GtkPaned` child, hiding it during a drag left only one visible child.
/// That child filled the pane and the handle disappeared until the WebView was shown again. Keeping both
/// pane children visible preserves divider geometry throughout the drag.
///
/// The returned `gtk4::Overlay` is the real end child of `Paned`. Its main child is an empty
/// `gtk4::DrawingArea` sensor that receives the slot's full allocation and emits `resize`.
/// `agent` is an overlay child with `halign` and `valign` set to `Start`, so it is placed
/// manually rather than stretched to the slot's size.
///
/// The sensor's `resize` signal drives `agent.set_size_request`, debounced by `DEBOUNCE_MS`:
/// the size is applied about 70ms after the last resize in a burst. Between updates, the
/// WebView keeps its last size and continues painting without reflowing on every motion. The
/// first resize is applied immediately so startup does not leave it at a stale or zero size.
/// The editor is attached directly as the other `Paned` child and receives intermediate resizes
/// immediately.
///
/// `set_clip_overlay` prevents the WebView from spilling into the editor while the slot shrinks
/// and the WebView still has its previous, larger size.
///
/// Returns the overlay upcast to `Widget`. The caller must use it as `Paned`'s end child;
/// passing `agent` directly would bypass the debounce.
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
