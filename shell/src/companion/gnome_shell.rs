//! The GNOME half of companion focus: calls to the Eitri GNOME Shell extension's D-Bus service.
//!
//! A client cannot list or focus windows under GNOME, so the extension does it, and it decides
//! who may ask by the caller's own pid: the calls therefore have to come from this process over
//! its own bus connection. A `gdbus`/`busctl` child would show up as a different pid and be refused,
//! so none is ever run.
//!
//! The extension's name is a well-known name, and the first process of this user to claim it would
//! own it: such a process would learn the editor's process chain from `SetPartner` and could answer
//! the calls. The extension runs inside gnome-shell, so an owner counts only when the bus says it is
//! the same process as the owner of `org.gnome.Shell`, and every call then goes to that owner's
//! unique name, which no other connection can ever take. A new owner is checked again.
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
/// The name gnome-shell itself owns. The extension's name counts only when the same process owns it.
pub(crate) const SHELL_NAME: &str = "org.gnome.Shell";
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

/// Why a call did not reach the extension.
enum CallError {
    /// The bus or the call itself failed.
    Bus(glib::Error),
    /// The extension's name is owned by a process that could not be shown to be gnome-shell, so
    /// nothing is sent to it. Already logged, once per owner.
    NotTheShell,
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
    /// The unique name of the extension name's owner, once it was found to be gnome-shell. Calls go
    /// there and nowhere else. Forgotten whenever the owner changes.
    verified: RefCell<Option<String>>,
    /// Counts the owner changes the watch reported, so a check that was under way across one does
    /// not record its answer for the new owner.
    owner_changes: Cell<u64>,
    /// The owner that was last refused, so its refusal is logged once.
    refused: RefCell<Option<String>>,
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
                verified: RefCell::new(None),
                owner_changes: Cell::new(0),
                refused: RefCell::new(None),
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

    /// The owner changed: whatever was checked about the previous one no longer holds.
    fn owner_changed(&self) {
        self.owner_changes.set(self.owner_changes.get() + 1);
        *self.verified.borrow_mut() = None;
    }

    /// The name has a new owner. It counts as the extension only once it was checked to be
    /// gnome-shell; until then nothing is told to it and the hook does not run.
    fn appeared(self: &Rc<Self>) {
        self.owner_changed();
        let inner = self.clone();
        glib::spawn_future_local(async move {
            let Some(connection) = inner.connection.borrow().clone() else {
                return;
            };
            match inner.destination(&connection).await {
                Ok(_) => {
                    inner.ever_owned.set(true);
                    inner.present.set(Some(true));
                    // Each owner is asked its version once; a first probe still waiting for its
                    // answer is that.
                    inner.log_version();
                    let hook = inner.appeared_hook.borrow().clone();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                // Logged where it was found; the owner may still be replaced by the real one. The name
                // has an owner, so this is not "not installed" either.
                Err(CallError::NotTheShell) => inner.present.set(None),
                // Gone again before it could be checked: the watch says so.
                Err(CallError::Bus(err)) if is_absent(&err) => {}
                Err(CallError::Bus(err)) => {
                    println!("[companion] GNOME Shell extension: could not check its owner: {err}")
                }
            }
        });
    }

    fn vanished(&self) {
        self.owner_changed();
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
                Err(CallError::NotTheShell) => {}
                Err(CallError::Bus(err)) if is_absent(&err) => {}
                Err(CallError::Bus(err)) => println!("[companion] GNOME Shell extension: Version failed: {err}"),
                Ok(reply) => match reply.get::<(u32,)>() {
                    Some((version,)) => {
                        inner.version_logged.set(true);
                        println!("[companion] GNOME Shell extension version {version}")
                    }
                    None => println!("[companion] GNOME Shell extension: Version answered {reply}, not a number"),
                },
            }
        });
    }

    /// The unique name to send the extension's calls to: the current owner of its name, when the
    /// bus says that owner is the same process as the owner of [`SHELL_NAME`]. Asked of the bus
    /// once per owner. A name with no owner is the bus's own error, which says the extension is
    /// absent; anything that keeps the check from proving the owner is gnome-shell -- another
    /// process, no owner of `org.gnome.Shell`, a pid the bus would not give -- is
    /// [`CallError::NotTheShell`], logged once per owner.
    async fn destination(self: &Rc<Self>, connection: &gio::DBusConnection) -> Result<String, CallError> {
        if let Some(verified) = self.verified.borrow().clone() {
            return Ok(verified);
        }
        let changes = self.owner_changes.get();
        let owner = bus_call(connection, "GetNameOwner", BUS_NAME, "(s)")
            .await
            .map_err(CallError::Bus)?
            .get::<(String,)>()
            .map(|(owner,)| owner)
            .ok_or_else(|| CallError::Bus(glib::Error::new(gio::DBusError::InvalidArgs, "GetNameOwner: no name")))?;
        let owner_pid = connection_pid(connection, &owner).await;
        let shell_pid = connection_pid(connection, SHELL_NAME).await;
        match (owner_pid, shell_pid) {
            (Some(owner_pid), Some(shell_pid)) if owner_pid == shell_pid => {
                if changes == self.owner_changes.get() {
                    *self.verified.borrow_mut() = Some(owner.clone());
                }
                Ok(owner)
            }
            (owner_pid, shell_pid) => {
                if self.refused.borrow().as_deref() != Some(owner.as_str()) {
                    let shown = |pid: Option<u32>| pid.map_or("unknown".to_string(), |pid| pid.to_string());
                    println!(
                        "[companion] the GNOME Shell extension's bus name is held by pid {}, not by GNOME Shell (pid {}); not calling it",
                        shown(owner_pid),
                        shown(shell_pid)
                    );
                    *self.refused.borrow_mut() = Some(owner);
                }
                Err(CallError::NotTheShell)
            }
        }
    }

    async fn call(self: &Rc<Self>, method: &str, args: Option<Variant>, reply: &str) -> Result<Variant, CallError> {
        let connection = self.connection().await.map_err(CallError::Bus)?;
        let destination = match self.destination(&connection).await {
            Ok(destination) => destination,
            Err(CallError::Bus(err)) => {
                if is_absent(&err) {
                    self.mark_absent();
                }
                return Err(CallError::Bus(err));
            }
            Err(CallError::NotTheShell) => {
                // Something owns the name, so it is not "not installed"; and it is not the extension.
                self.present.set(None);
                return Err(CallError::NotTheShell);
            }
        };
        let reply_type = VariantTy::new(reply).expect("a reply type written in this file");
        let result = connection
            .call_future(
                Some(&destination),
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
            Err(err) if is_absent(err) => {
                // The verified owner's connection is gone; whoever owns the name next is checked anew.
                if self.verified.borrow().as_deref() == Some(destination.as_str()) {
                    *self.verified.borrow_mut() = None;
                }
                self.mark_absent();
            }
            Err(_) => {}
        }
        result.map_err(CallError::Bus)
    }
}

/// One call to the bus itself, with a single string argument.
async fn bus_call(
    connection: &gio::DBusConnection,
    method: &str,
    name: &str,
    reply: &str,
) -> Result<Variant, glib::Error> {
    let reply_type = VariantTy::new(reply).expect("a reply type written in this file");
    connection
        .call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            method,
            Some(&(name,).to_variant()),
            Some(reply_type),
            DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
        )
        .await
}

/// The pid of the process whose connection owns `name`, as the bus reports it; `None` when the name
/// has no owner or the bus would not say.
async fn connection_pid(connection: &gio::DBusConnection, name: &str) -> Option<u32> {
    bus_call(connection, "GetConnectionUnixProcessID", name, "(u)")
        .await
        .ok()?
        .get::<(u32,)>()
        .map(|(pid,)| pid)
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
            // A name nobody owns was already said once, when the extension was found absent, and an
            // owner that is not gnome-shell when it was refused.
            Err(CallError::NotTheShell) => None,
            Err(CallError::Bus(err)) => {
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
