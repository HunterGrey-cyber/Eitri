use std::ffi::c_void;
use std::sync::OnceLock;

use gtk4::{gdk, glib, prelude::*};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::{protocol::wl_surface::WlSurface, Connection, Proxy};

#[link(name = "gtk-4")]
extern "C" {
    fn gdk_wayland_display_get_wl_display(display: *mut gdk::ffi::GdkDisplay) -> *mut c_void;
    fn gdk_wayland_surface_get_wl_surface(surface: *mut gdk::ffi::GdkSurface) -> *mut c_void;
}

type ForceNextCommit = unsafe extern "C" fn(*mut gdk::ffi::GdkSurface);

/// GDK's `gdk_wayland_surface_force_next_commit` (GTK 4.18), looked up at run time: the build floor's GTK (4.14) has
/// no such symbol. `auto` takes the subsurface path only on GTK 4.24 or later, but an explicit `subsurface` takes it on
/// any GTK, so the lookup can find nothing.
fn force_next_commit_fn() -> Option<ForceNextCommit> {
    static FOUND: OnceLock<Option<ForceNextCommit>> = OnceLock::new();
    *FOUND.get_or_init(|| {
        // SAFETY: a lookup in the symbols already loaded into the process, with a NUL-terminated name.
        let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"gdk_wayland_surface_force_next_commit".as_ptr()) };
        // SAFETY: GDK declares the symbol as `void (GdkSurface *)`, which is `ForceNextCommit`.
        (!symbol.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, ForceNextCommit>(symbol) })
    })
}

/// Makes GDK commit the surface at its next frame even when nothing GTK draws changed, so the state a sync child
/// surface cached is applied: a frame whose render nodes did not change is an empty frame, which GDK commits only
/// when it knows something of the surface is pending. Returns false when the surface is not a live Wayland one or the
/// function is missing; the caller still queues a draw.
pub(crate) fn force_next_commit(surface: &gdk::Surface) -> bool {
    let display = surface.display();
    if surface.is_destroyed() || display.is_closed() || !is_wayland_display_name(display.type_().name()) {
        return false;
    }
    let Some(force) = force_next_commit_fn() else {
        return false;
    };
    // SAFETY: the live surface belongs to a Wayland display (checked above) and is borrowed for the call; the
    // function only sets a flag on it.
    unsafe { force(surface.as_ptr()) };
    true
}

fn is_wayland_display_name(name: &str) -> bool {
    name == "GdkWaylandDisplay"
}

/// Borrows GDK's connection without taking ownership of its display or dispatching its events.
/// Callers keep the display alive and stop using the connection when GDK reports it closed.
pub(crate) fn connection_for(display: &gdk::Display) -> Option<Connection> {
    if !is_wayland_display_name(display.type_().name()) || display.is_closed() {
        return None;
    }
    let key = glib::Quark::from_str("eitri-subsurface-wayland-connection");
    // SAFETY: this private key holds only a Connection, installed below on the GTK thread.
    // The borrowed display keeps its qdata alive; no call here replaces it while it is borrowed.
    if let Some(cached) = unsafe { display.qdata::<Connection>(key) } {
        // SAFETY: the display owns this Connection and remains borrowed until it is cloned.
        return Some(unsafe { cached.as_ref() }.clone());
    }
    // SAFETY: the exact runtime type was checked above. GDK owns the returned wl_display;
    // the GdkDisplay outlives its panes and their connection use, which ends at Display::closed.
    let raw = unsafe { gdk_wayland_display_get_wl_display(display.as_ptr()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: GDK owns the live display above. Its panes keep GdkDisplay alive, and the
    // presentation thread's connection use ends at GdkDisplay::closed. Guest mode never disconnects it.
    let connection = Connection::from_backend(unsafe { Backend::from_foreign_display(raw.cast()) });
    // SAFETY: the private key is read only as Connection. GDK owns the cache and every caller
    // receives a clone of this same backend, so panes share one foreign connection per display.
    unsafe { display.set_qdata(key, connection.clone()) };
    display.connect_closed(move |display, _| {
        // SAFETY: this key holds only our Connection and all access is on the GTK thread.
        // Release the cache at close, while the presentation thread retains its own clone
        // until it has stopped; GDK's later finalization must not be what drops that backend.
        drop(unsafe { display.steal_qdata::<Connection>(key) });
    });
    Some(connection)
}

/// Wraps a live GDK surface without changing its owner, event queue or listener.
/// The proxy must not be used after GDK destroys the surface or closes its display.
pub(crate) fn toplevel_surface(conn: &Connection, surface: &gdk::Surface) -> Option<WlSurface> {
    let display = surface.display();
    if surface.is_destroyed() || display.is_closed() || !is_wayland_display_name(display.type_().name()) {
        return None;
    }
    // SAFETY: the checked Wayland display is borrowed and live for this call. GDK retains its
    // wl_display until closed; connection users stop then, including the presentation thread.
    let raw_display = unsafe { gdk_wayland_display_get_wl_display(display.as_ptr()) };
    if raw_display.is_null() || raw_display != conn.backend().display_ptr().cast() {
        return None;
    }
    // SAFETY: the live surface belongs to the checked Wayland display. Its borrowed GdkSurface
    // keeps the native wl_surface valid while it is wrapped; callers stop using it on destruction.
    let raw = unsafe { gdk_wayland_surface_get_wl_surface(surface.as_ptr()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: GDK returned a live wl_surface above, kept valid by the surface borrow. This creates
    // a foreign id only: GDK retains ownership and its listener, and no destroy request is sent.
    let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), raw.cast()) }.ok()?;
    WlSurface::from_id(conn, id).ok()
}

#[cfg(test)]
mod tests {
    use super::is_wayland_display_name;

    #[test]
    fn the_type_name_check_accepts_only_gdk_wayland_display() {
        assert!(is_wayland_display_name("GdkWaylandDisplay"));
        for name in [
            "",
            "GdkDisplay",
            "GdkX11Display",
            "WaylandDisplay",
            "GdkWaylandDisplayExtra",
        ] {
            assert!(!is_wayland_display_name(name), "accepted {name:?}");
        }
    }
}
