//! A GLib unix-fd watch for a `!Send` closure on the default main context.
//!
//! glib 0.22.9 (which gtk4 0.11.4 pulls in) has no `unix_fd_add_local`, so this calls
//! `g_unix_fd_add_full` itself. That call is the only `unsafe` here, and [`add_local`] is the only
//! way to reach it.
//!
//! **Contract:** the caller removes the returned source (`SourceId::remove`) before it closes the
//! fd. A watch on a closed fd reports `G_IO_NVAL`, and on a reused fd number would watch somebody
//! else's file.
//!
//! **Threads:** the closure is wrapped in a [`ThreadGuard`], as glib-rs's own `*_local` sources
//! do. `add_local` only checks ownership at registration: the thread could later release the
//! context to another thread that iterates it, or send the (`Send`) `SourceId` to a thread that
//! removes it. Either would run or drop the `!Send` closure off its thread; the guard turns that
//! into a panic (an abort, from a C callback) rather than undefined behaviour.

use std::os::fd::RawFd;

use gtk4::glib::{self, ffi, thread_guard::ThreadGuard, translate::*};

type Callback = ThreadGuard<Box<dyn FnMut(glib::IOCondition) -> glib::ControlFlow>>;

/// The priority the pane's event-loop watch runs at (`TickDriver::watch_fd`), and the one
/// `tests/fd_watch_priority.rs` holds: `G_PRIORITY_DEFAULT_IDLE` (200), below GDK's paint
/// priority (120), so a flood of redraw batches never starves a paint (plan review F1).
pub const EVENT_LOOP_WATCH_PRIORITY: std::ffi::c_int = ffi::G_PRIORITY_DEFAULT_IDLE;

/// `add_local` was called from a thread that does not own the default main context, where the
/// closure (which is not `Send`) would be dispatched on the wrong thread.
#[derive(Debug, PartialEq, Eq)]
pub struct NotOwner;

unsafe extern "C" {
    fn g_unix_fd_add_full(
        priority: std::ffi::c_int,
        fd: std::ffi::c_int,
        condition: ffi::GIOCondition,
        function: Option<unsafe extern "C" fn(std::ffi::c_int, ffi::GIOCondition, ffi::gpointer) -> ffi::gboolean>,
        user_data: ffi::gpointer,
        notify: ffi::GDestroyNotify,
    ) -> std::ffi::c_uint;
}

unsafe extern "C" fn trampoline(
    _fd: std::ffi::c_int,
    condition: ffi::GIOCondition,
    data: ffi::gpointer,
) -> ffi::gboolean {
    // SAFETY: `data` is the `Box<Callback>` `add_local` leaked, alive until `destroy`.
    let callback = unsafe { &mut *(data as *mut Callback) };
    let condition: glib::IOCondition = unsafe { from_glib(condition) };
    // Panics off the registering thread (see the module doc).
    (callback.get_mut())(condition).into_glib()
}

unsafe extern "C" fn destroy(data: ffi::gpointer) {
    // SAFETY: called once by GLib when the source is destroyed. The guard's own drop panics off
    // the registering thread (see the module doc).
    drop(unsafe { Box::from_raw(data as *mut Callback) });
}

/// Watches `fd` on the default main context at `priority` (a `glib::ffi::G_PRIORITY_*` value).
/// `Err(NotOwner)` unless this thread owns that context, which is the thread the closure runs on.
pub fn add_local(
    fd: RawFd,
    condition: glib::IOCondition,
    priority: std::ffi::c_int,
    func: impl FnMut(glib::IOCondition) -> glib::ControlFlow + 'static,
) -> Result<glib::SourceId, NotOwner> {
    if !glib::MainContext::default().is_owner() {
        return Err(NotOwner);
    }
    let callback: Box<Callback> = Box::new(ThreadGuard::new(Box::new(func)));
    // SAFETY: a valid trampoline/destroy pair for the leaked box. The `!Send` closure is only
    // touched on this thread: the guard enforces it for dispatch and destruction alike.
    Ok(unsafe {
        from_glib(g_unix_fd_add_full(
            priority,
            fd,
            condition.into_glib(),
            Some(trampoline),
            Box::into_raw(callback) as ffi::gpointer,
            Some(destroy),
        ))
    })
}
