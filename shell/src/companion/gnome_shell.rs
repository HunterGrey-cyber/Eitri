//! The GNOME half of companion focus: calls to the Eitri GNOME Shell extension's D-Bus service.
//!
//! A client cannot list or focus windows under GNOME, so the extension does it, and it decides
//! who may ask by the caller's own pid: the calls therefore have to come from this process over
//! its own bus connection. A `gdbus`/`busctl` child would show up as a different pid and be refused,
//! so none is ever run.
//!
//! Every call is an async gio call under `glib::spawn_future_local` on the GTK main context, so
//! nothing here waits. A call reports through a callback; a caller that needs nothing back passes a
//! closure that does nothing.
//!
//! This file imports nothing from the rest of the shell crate, so a test binary can include it
//! alone and run it against a private bus.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use gtk4::gio::{self, DBusCallFlags};
use gtk4::glib::{self, Variant, VariantTy};
use gtk4::prelude::ToVariant;

use eitri_core::layout::Direction;

pub(crate) const BUS_NAME: &str = "cn.huntergrey.Eitri.Shell1";
pub(crate) const OBJECT_PATH: &str = "/cn/huntergrey/Eitri/Shell1";
pub(crate) const INTERFACE: &str = "cn.huntergrey.Eitri.Shell1";
/// A call that has not been answered by now has failed. The keys that cause calls are typed by a
/// person, so a Shell that stalls must not stall a call for long, and the extension itself answers
/// in a few milliseconds.
pub(crate) const CALL_TIMEOUT_MS: i32 = 1000;

/// What a call reports: the method's boolean answer, or `None` when the call itself failed (no
/// service, a timeout, a reply of an unexpected shape). `Some(false)` is the extension saying
/// there was nothing to do, and is not a failure.
pub(crate) type Done = Box<dyn FnOnce(Option<bool>)>;

/// The extension's methods, as the window-manager runner uses them; implemented by [`GnomeShell`],
/// and by a recording fake where the routing is tested without a bus.
pub(crate) trait GnomeCalls {
    /// Asks the extension for its version and logs what came back. Also settles [`present`].
    ///
    /// [`present`]: GnomeCalls::present
    fn probe(&self);
    /// `Some(true)` while the extension's name has an owner, `Some(false)` once the bus said nobody
    /// has owned it since this process started (the extension is not installed or not enabled), and
    /// `None` while that is not known, or after an owner that was there went away (a screen lock
    /// disables the extension and unlocking enables it again, so that is not an absence).
    fn present(&self) -> Option<bool>;
    /// `hook` runs each time the extension's name gets an owner: the first time one appears, and
    /// again after every time it went away. A freshly enabled extension knows nothing of what the
    /// panel told the one before it, so this is where that is told again.
    fn on_appeared(&self, hook: Box<dyn Fn()>);
    fn focus_direction(&self, dir: Direction, done: Done);
    fn focus_self_if_neighbour(&self, dir: Direction, done: Done);
    fn set_partner(&self, pids: Vec<u32>, done: Done);
    fn activate_own(&self, done: Done);
    fn activate_partner(&self, done: Done);
}

type ConnectFuture = Pin<Box<dyn Future<Output = Result<gio::DBusConnection, glib::Error>>>>;
type Connect = Box<dyn Fn() -> ConnectFuture>;

pub(crate) struct GnomeShell {
    inner: Rc<Inner>,
}

struct Inner {
    connect: Connect,
    /// The first connection that was made; every later call reuses it.
    connection: RefCell<Option<gio::DBusConnection>>,
    present: Cell<Option<bool>>,
    /// Whether the name has ever had an owner. A name that had one and lost it is coming back, not
    /// missing; a name that never had one is the "install the extension" case.
    ever_owned: Cell<bool>,
    /// Whether the version was read from the current owner: a new owner is read again.
    version_logged: Cell<bool>,
    version_in_flight: Cell<bool>,
    appeared_hook: RefCell<Option<Rc<dyn Fn()>>>,
    /// Ends the watch on the name.
    unwatch: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(unwatch) = self.unwatch.get_mut().take() {
            unwatch();
        }
    }
}

impl GnomeShell {
    /// Over the session bus. gio shares one session connection per process, which is also the one
    /// the rest of the shell uses.
    pub(crate) fn new() -> Rc<GnomeShell> {
        Self::with_connector(Box::new(|| Box::pin(gio::bus_get_future(gio::BusType::Session))))
    }

    /// Over whatever `connect` makes. For a test that must not touch the session bus.
    pub(crate) fn with_connector(connect: Connect) -> Rc<GnomeShell> {
        Rc::new(GnomeShell {
            inner: Rc::new(Inner {
                connect,
                connection: RefCell::new(None),
                present: Cell::new(None),
                ever_owned: Cell::new(false),
                version_logged: Cell::new(false),
                version_in_flight: Cell::new(false),
                appeared_hook: RefCell::new(None),
                unwatch: RefCell::new(None),
            }),
        })
    }
}

/// The word the extension's `direction` argument takes.
pub(crate) fn direction_name(dir: Direction) -> &'static str {
    match dir {
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

/// Whether `err` is the bus saying the extension's name has no owner, as opposed to a call that
/// failed for any other reason. Both names are what a bus answers for a name nobody holds,
/// depending on whether it tried to start one.
fn is_absent(err: &glib::Error) -> bool {
    err.matches(gio::DBusError::ServiceUnknown) || err.matches(gio::DBusError::NameHasNoOwner)
}

impl Inner {
    async fn connection(self: &Rc<Self>) -> Result<gio::DBusConnection, glib::Error> {
        if let Some(connection) = self.connection.borrow().clone() {
            return Ok(connection);
        }
        // Two first calls in flight each connect; gio hands the session bus's one shared connection
        // to both, and a test connector is only ever used one call at a time.
        let connection = (self.connect)().await?;
        if self.connection.borrow().is_some() {
            // The other first call got there while this one was connecting: one watch is enough.
            return Ok(connection);
        }
        *self.connection.borrow_mut() = Some(connection.clone());
        self.watch_name(&connection);
        Ok(connection)
    }

    /// Follows who owns the extension's name for as long as this lives. gio reports the owner as it
    /// stands when the watch starts, and every change after, on the main context this runs on.
    fn watch_name(self: &Rc<Self>, connection: &gio::DBusConnection) {
        let (on_appeared, on_vanished) = (Rc::downgrade(self), Rc::downgrade(self));
        let id = gio::bus_watch_name_on_connection(
            connection,
            BUS_NAME,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, _| {
                if let Some(inner) = on_appeared.upgrade() {
                    inner.appeared();
                }
            },
            move |_, _| {
                if let Some(inner) = on_vanished.upgrade() {
                    inner.vanished();
                }
            },
        );
        *self.unwatch.borrow_mut() = Some(Box::new(move || gio::bus_unwatch_name(id)));
    }

    fn appeared(self: &Rc<Self>) {
        self.ever_owned.set(true);
        self.present.set(Some(true));
        // Each owner is asked its version once; a first probe still waiting for its answer is that.
        self.log_version();
        let hook = self.appeared_hook.borrow().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn vanished(&self) {
        if self.ever_owned.get() {
            // Disabled, as a screen lock does, and not gone for good: no notice, and the next owner
            // is read again.
            if self.present.replace(None) != None {
                println!("[companion] GNOME Shell extension went away; waiting for it to come back");
            }
            self.version_logged.set(false);
        } else {
            self.mark_never_owned();
        }
    }

    /// The bus says nobody owns the name, and nobody did.
    fn mark_never_owned(&self) {
        if self.present.replace(Some(false)) != Some(false) {
            println!("[companion] GNOME Shell extension is not installed or not enabled");
        }
    }

    /// An error that says the name has no owner. Whether that is "not installed" or "gone for now"
    /// depends on whether it ever had one.
    fn mark_absent(&self) {
        if self.ever_owned.get() {
            self.present.set(None);
        } else {
            self.mark_never_owned();
        }
    }

    /// Reads and logs the owner's version, unless that was done for this owner or is being done.
    fn log_version(self: &Rc<Self>) {
        if self.version_logged.get() || self.version_in_flight.replace(true) {
            return;
        }
        let inner = self.clone();
        glib::spawn_future_local(async move {
            let result = inner.call("Version", None, "(u)").await;
            inner.version_in_flight.set(false);
            match result {
                Ok(reply) => match reply.get::<(u32,)>() {
                    Some((version,)) => {
                        inner.version_logged.set(true);
                        println!("[companion] GNOME Shell extension version {version}")
                    }
                    None => println!("[companion] GNOME Shell extension: Version answered {reply}, not a number"),
                },
                // `call` already said so when it learnt the extension is absent.
                Err(err) if is_absent(&err) => {}
                Err(err) => println!("[companion] GNOME Shell extension: Version failed: {err}"),
            }
        });
    }

    async fn call(self: &Rc<Self>, method: &str, args: Option<Variant>, reply: &str) -> Result<Variant, glib::Error> {
        let connection = self.connection().await?;
        let reply_type = VariantTy::new(reply).expect("a reply type written in this file");
        let result = connection
            .call_future(
                Some(BUS_NAME),
                OBJECT_PATH,
                INTERFACE,
                method,
                args.as_ref(),
                Some(reply_type),
                DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
            )
            .await;
        match &result {
            Ok(_) => {
                self.ever_owned.set(true);
                self.present.set(Some(true));
            }
            Err(err) if is_absent(err) => self.mark_absent(),
            Err(_) => {}
        }
        result
    }
}

/// Runs the call on the main context and reports its boolean answer through `done`.
fn call_bool(inner: &Rc<Inner>, method: &'static str, args: Option<Variant>, done: Done) {
    let inner = inner.clone();
    glib::spawn_future_local(async move {
        let answer = match inner.call(method, args, "(b)").await {
            Ok(reply) => match reply.get::<(bool,)>() {
                Some((answer,)) => Some(answer),
                None => {
                    println!("[companion] GNOME Shell extension: {method} answered {reply}, not a boolean");
                    None
                }
            },
            Err(err) => {
                // A name nobody owns was already said once, when the extension was found absent.
                if !is_absent(&err) {
                    println!("[companion] GNOME Shell extension: {method} failed: {err}");
                }
                None
            }
        };
        done(answer);
    });
}

impl GnomeCalls for GnomeShell {
    fn probe(&self) {
        self.inner.log_version();
    }

    fn present(&self) -> Option<bool> {
        self.inner.present.get()
    }

    fn on_appeared(&self, hook: Box<dyn Fn()>) {
        *self.inner.appeared_hook.borrow_mut() = Some(Rc::from(hook));
    }

    fn focus_direction(&self, dir: Direction, done: Done) {
        call_bool(
            &self.inner,
            "FocusDirection",
            Some((direction_name(dir),).to_variant()),
            done,
        );
    }

    fn focus_self_if_neighbour(&self, dir: Direction, done: Done) {
        call_bool(
            &self.inner,
            "FocusSelfIfNeighbour",
            Some((direction_name(dir),).to_variant()),
            done,
        );
    }

    fn set_partner(&self, pids: Vec<u32>, done: Done) {
        call_bool(&self.inner, "SetPartner", Some((pids,).to_variant()), done);
    }

    fn activate_own(&self, done: Done) {
        call_bool(&self.inner, "ActivateOwn", None, done);
    }

    fn activate_partner(&self, done: Done) {
        call_bool(&self.inner, "ActivatePartner", None, done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_are_the_four_words_the_extension_takes() {
        assert_eq!(direction_name(Direction::Left), "left");
        assert_eq!(direction_name(Direction::Right), "right");
        assert_eq!(direction_name(Direction::Up), "up");
        assert_eq!(direction_name(Direction::Down), "down");
    }

    #[test]
    fn only_a_nameless_bus_error_counts_as_an_absent_extension() {
        assert!(is_absent(&glib::Error::new(gio::DBusError::ServiceUnknown, "x")));
        assert!(is_absent(&glib::Error::new(gio::DBusError::NameHasNoOwner, "x")));
        assert!(!is_absent(&glib::Error::new(gio::DBusError::NoReply, "x")));
        assert!(!is_absent(&glib::Error::new(gio::IOErrorEnum::TimedOut, "x")));
    }
}
