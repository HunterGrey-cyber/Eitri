//! The link's GTK side: the link thread's wake descriptor watched on the main context, what arrived drained on
//! each wake and handed to the handler registered for its kind, and the end of the link reported once.
//!
//! The link thread does every read and write, so nothing here waits: a wake drains what is already queued, a send
//! only queues. A kind with no handler is logged and its descriptors closed, so a message no part of this window
//! takes cannot hold a descriptor open. The end is reported after every message the other window wrote before
//! it, as the link thread passes them: `bye{why}` first means a clean close, an end without one means the other
//! window died or the link broke.
//!
//! The move protocol's messages are the window controller's: [`LinkDriver::attach_controller`] hands them to it as
//! its peer events, posts one wake event after each drain (a confirmed send's answer comes on a wake with no message),
//! and detaches the peer when the link ends.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eitri_core::window_link::link::{Inbound, LinkHandle, LinkSender, PresenceFlag};
use eitri_core::window_link::msg::KINDS;
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::{LinkError, Msg, TurnEdge};
use eitri_core::wm::{Controller, Driver, PeerId, WinId};
use gtk4::glib;

use super::LinkEnd;
use crate::gtk_host::peer::{self, LinkPort, LinkSend};

/// How long a window started here has to finish the handshake. Its `hello` comes once its toolkit is up, which a
/// loaded machine can take several seconds for; past this the start has failed, and a window that never says hello
/// must not leave the pair looking as if it had a second window.
pub const HANDSHAKE_WITHIN: Duration = Duration::from_secs(30);

/// The other window of the pair: its process and that process's start time, which together name it even after
/// its pid is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partner {
    pub pid: u32,
    pub start: u64,
}

type Handler = Rc<dyn Fn(&LinkDriver, Msg, Vec<OwnedFd>)>;
type EndHandler = Rc<dyn Fn(&LinkDriver, &LinkEnd)>;
type ClosingHandler = Rc<dyn Fn(&LinkDriver)>;
type DrainedHandler = Rc<dyn Fn(&LinkDriver)>;
type WakeHandler = Rc<dyn Fn(&LinkDriver)>;

struct Inner {
    handle: RefCell<Option<LinkHandle>>,
    inbound: Receiver<Inbound>,
    handlers: RefCell<BTreeMap<&'static str, Handler>>,
    on_end: RefCell<Vec<EndHandler>>,
    on_closing: RefCell<Vec<ClosingHandler>>,
    on_drained: RefCell<Vec<DrainedHandler>>,
    on_wake: RefCell<Vec<WakeHandler>>,
    partner: Cell<Option<Partner>>,
    /// The flag the link thread sets while the other window counts as another window for turn review: the one given
    /// to [`LinkHandle::start`], so what the thread sets is what this window's revert checks read.
    presence: RefCell<PresenceFlag>,
    /// The turns the other window said were running when it said hello or welcome, until they are taken.
    partner_running: RefCell<Vec<TurnEdge>>,
    /// The handshake is done: the other window's `hello` was checked and `welcome` sent (window 1), or `welcome`
    /// arrived (window 2).
    linked: Cell<bool>,
    /// The other window's `bye`, kept until the end it announces arrives.
    bye: RefCell<Option<String>>,
    ended: RefCell<Option<LinkEnd>>,
    /// This window closed the link itself: its end is not reported as the other window's.
    closed_here: Cell<bool>,
    /// When the handshake must be done by, for a link whose other window was started here.
    handshake_by: Cell<Option<Instant>>,
    watch: RefCell<Option<glib::SourceId>>,
}

/// One window's end of the link on the GTK thread. Cheap to clone: every clone is the same link.
#[derive(Clone)]
pub struct LinkDriver {
    inner: Rc<Inner>,
}

impl LinkDriver {
    /// Wraps a started link. Nothing is read until [`LinkDriver::watch`] (or a test's [`LinkDriver::pump`]).
    pub fn new(handle: LinkHandle, inbound: Receiver<Inbound>) -> LinkDriver {
        LinkDriver {
            inner: Rc::new(Inner {
                handle: RefCell::new(Some(handle)),
                inbound,
                handlers: RefCell::default(),
                on_end: RefCell::default(),
                on_closing: RefCell::default(),
                on_drained: RefCell::default(),
                on_wake: RefCell::default(),
                partner: Cell::new(None),
                presence: RefCell::new(PresenceFlag::default()),
                partner_running: RefCell::default(),
                linked: Cell::new(false),
                bye: RefCell::new(None),
                ended: RefCell::new(None),
                closed_here: Cell::new(false),
                handshake_by: Cell::new(None),
                watch: RefCell::new(None),
            }),
        }
    }

    /// Watches the link's wake descriptor on the default main context; each wake pumps. The watch ends with
    /// the link. `false` when it could not be added (not on the main context's thread).
    pub fn watch(&self) -> bool {
        let fd = match self.inner.handle.borrow().as_ref() {
            Some(handle) => handle.wake_fd().as_raw_fd(),
            None => return false,
        };
        let me = Rc::downgrade(&self.inner);
        let watched =
            neovide_editor::fd_watch::add_local(fd, glib::IOCondition::IN, glib::ffi::G_PRIORITY_DEFAULT, move |_| {
                let Some(inner) = me.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let driver = LinkDriver { inner };
                driver.pump();
                if driver.ended().is_some() || driver.inner.handle.borrow().is_none() {
                    // GLib removes the source once this returns `Break`, so its id is forgotten here.
                    driver.inner.watch.borrow_mut().take();
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        let Ok(id) = watched else {
            eprintln!("[link] BUG: the link is watched off the main context's thread; nothing arrives");
            return false;
        };
        *self.inner.watch.borrow_mut() = Some(id);
        true
    }

    /// Registers what `kind`'s messages go to; one handler per kind, a later one replacing it. A kind the
    /// link does not know is a mistake in this window's code.
    pub fn on(&self, kind: &'static str, handler: impl Fn(&LinkDriver, Msg, Vec<OwnedFd>) + 'static) {
        assert!(KINDS.contains(&kind), "the link has no message kind {kind:?}");
        self.inner.handlers.borrow_mut().insert(kind, Rc::new(handler));
    }

    /// Registers what runs as this window closes the link, before its `bye` is queued.
    pub fn on_closing(&self, handler: impl Fn(&LinkDriver) + 'static) {
        self.inner.on_closing.borrow_mut().push(Rc::new(handler));
    }

    /// Registers what runs after each drain that handed any message over, before the link's end is reported: for a
    /// handler that keeps what arrived together and acts on it as one.
    pub fn on_drained(&self, handler: impl Fn(&LinkDriver) + 'static) {
        self.inner.on_drained.borrow_mut().push(Rc::new(handler));
    }

    /// Registers what runs once after each drain, whether or not anything arrived, before the link's end is
    /// reported: a confirmed send's answer wakes the link without a message.
    pub fn on_wake(&self, handler: impl Fn(&LinkDriver) + 'static) {
        self.inner.on_wake.borrow_mut().push(Rc::new(handler));
    }

    /// The flag the link thread was started with ([`LinkHandle::start`]); see [`LinkDriver::presence_flag`].
    pub fn set_presence(&self, flag: PresenceFlag) {
        *self.inner.presence.borrow_mut() = flag;
    }

    /// Whether the other window counts as another window for turn review, as the link thread keeps it: set from
    /// the handshake when the other window had not confirmed its presence, and from each `presence{held}` it
    /// sends, before the acknowledgement leaves.
    pub fn presence_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.inner.presence.borrow().0.clone()
    }

    /// The turns the other window's `hello` or `welcome` said were running.
    pub fn set_partner_running(&self, running: Vec<TurnEdge>) {
        *self.inner.partner_running.borrow_mut() = running;
    }

    pub fn take_partner_running(&self) -> Vec<TurnEdge> {
        std::mem::take(&mut *self.inner.partner_running.borrow_mut())
    }

    /// Registers what runs once when the link ends without this window having closed it.
    pub fn on_end(&self, handler: impl Fn(&LinkDriver, &LinkEnd) + 'static) {
        self.inner.on_end.borrow_mut().push(Rc::new(handler));
    }

    /// Drains what arrived and hands each message to its handler, the end last. Handlers run with nothing of
    /// the driver borrowed, so one may send, close or register.
    pub fn pump(&self) {
        if let Some(handle) = self.inner.handle.borrow().as_ref() {
            handle.drain_wake();
        }
        let mut handed = false;
        loop {
            match self.inner.inbound.try_recv() {
                Ok(Inbound::Msg { msg, fds }) => {
                    handed = true;
                    self.dispatch(msg, fds);
                }
                Ok(Inbound::Refused(err)) => eprintln!("[link] a packet from the other window was refused: {err}"),
                Ok(Inbound::Closed) | Err(TryRecvError::Disconnected) => {
                    if handed {
                        self.drained();
                    }
                    self.woke();
                    self.end_of_link();
                    return;
                }
                Err(TryRecvError::Empty) => {
                    if handed {
                        self.drained();
                    }
                    self.woke();
                    return;
                }
            }
        }
    }

    fn drained(&self) {
        let handlers: Vec<DrainedHandler> = self.inner.on_drained.borrow().clone();
        for handler in handlers {
            handler(self);
        }
    }

    fn woke(&self) {
        let handlers: Vec<WakeHandler> = self.inner.on_wake.borrow().clone();
        for handler in handlers {
            handler(self);
        }
    }

    fn dispatch(&self, msg: Msg, fds: Vec<OwnedFd>) {
        if let Msg::Bye { why } = &msg {
            println!("[link] the other window says bye: {why}");
            *self.inner.bye.borrow_mut() = Some(why.clone());
        }
        let kind = msg.kind();
        let handler = self.inner.handlers.borrow().get(kind).cloned();
        match handler {
            Some(handler) => handler(self, msg, fds),
            None if kind == "bye" => {}
            // The descriptors close as `fds` drops.
            None => eprintln!("[link] nothing in this window takes `{kind}` yet; dropped"),
        }
    }

    fn end_of_link(&self) {
        if self.inner.ended.borrow().is_some() {
            return;
        }
        let end = match self.inner.bye.borrow_mut().take() {
            Some(why) => LinkEnd::Bye { why },
            None => LinkEnd::Eof,
        };
        *self.inner.ended.borrow_mut() = Some(end.clone());
        self.inner.linked.set(false);
        // The thread has ended: the handle has nothing more to give, and dropping it joins the thread.
        let handle = self.inner.handle.borrow_mut().take();
        drop(handle);
        if self.inner.closed_here.get() {
            return;
        }
        println!("[link] the link to the other window ended ({end:?})");
        let handlers: Vec<EndHandler> = self.inner.on_end.borrow().clone();
        for handler in handlers {
            handler(self, &end);
        }
    }

    /// Queues `msg` with `fds` (closed once written); refused at once when the message breaks its kind's rules.
    /// After the link ended the message is dropped, as on a link the other window closed.
    pub fn send(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<(), LinkError> {
        match self.inner.handle.borrow().as_ref() {
            Some(handle) => handle.send(msg, fds),
            None => Ok(()),
        }
    }

    /// As [`LinkDriver::send`], and the receiver answers once the packet is on the socket. After the link ended it
    /// answers at once that it never will be.
    pub fn send_confirmed(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<Receiver<io::Result<()>>, LinkError> {
        match self.inner.handle.borrow().as_ref() {
            Some(handle) => handle.send_confirmed(msg, fds),
            None => {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the link ended before this was written",
                )));
                Ok(rx)
            }
        }
    }

    /// A sender for a thread other than this one, while the link has a handle.
    pub fn sender(&self) -> Option<LinkSender> {
        self.inner.handle.borrow().as_ref().map(LinkHandle::sender)
    }

    /// Makes the window controller's `win` the link's end toward `peer`, the window called `name`: the move
    /// protocol's messages reach it as peer events, each drain as one wake event, and the link's end detaches the
    /// peer. The controller sends on the link through a [`LinkPort`]. An offer this window cannot read is refused on
    /// the link at once, never posted. The controller is held weakly: the link must not keep the window's controller
    /// alive.
    pub fn attach_controller(&self, win: WinId, peer: PeerId, name: WindowName, controller: &Rc<Driver<Controller>>) {
        let weak = Rc::downgrade(controller);
        for kind in peer::MOVE_KINDS {
            let weak = weak.clone();
            self.on(kind, move |link, msg, fds| {
                if let Some(refusal) = peer::unreadable_offer(&msg) {
                    if let Err(err) = link.send(refusal, Vec::new()) {
                        eprintln!("[link] the refusal of an offer could not be sent: {err}");
                    }
                    return;
                }
                let event = peer::inbound_event(peer, Inbound::Msg { msg, fds });
                if let (Some(event), Some(controller)) = (event, weak.upgrade()) {
                    controller.post(event);
                }
            });
        }
        {
            let weak = weak.clone();
            self.on_wake(move |_| {
                if let Some(controller) = weak.upgrade() {
                    controller.post(peer::woke(peer));
                }
            });
        }
        self.on_end(move |_, _| {
            if let Some(controller) = weak.upgrade() {
                controller.detach_peer(win, peer);
            }
        });
        controller.attach_peer(win, peer, name, Rc::new(LinkPort::new(self.clone())));
    }

    /// Says `bye{why}` and closes the link; the other window sees a clean close. This window's own handlers for
    /// the end do not run: it asked for it.
    pub fn close(&self, why: &str) {
        if let Some(handle) = self.closing() {
            handle.close(why);
        }
    }

    /// [`LinkDriver::close`] with the wait for a full socket on a thread of its own: for one close of many, each of which
    /// could otherwise hold the GTK thread for its whole bound.
    pub fn close_detached(&self, why: &str) {
        if let Some(handle) = self.closing() {
            handle.close_detached(why);
        }
    }

    /// Says this window is closing the link, and gives up its handle to do it with.
    fn closing(&self) -> Option<LinkHandle> {
        if !self.inner.closed_here.get() && self.is_up() {
            let handlers: Vec<ClosingHandler> = self.inner.on_closing.borrow().clone();
            for handler in handlers {
                handler(self);
            }
        }
        self.inner.closed_here.set(true);
        self.inner.linked.set(false);
        // The watch goes first: closing the link closes the descriptor it watches.
        if let Some(id) = self.inner.watch.borrow_mut().take() {
            id.remove();
        }
        self.inner.handle.borrow_mut().take()
    }

    /// Whether `other` is this same link.
    pub fn same(&self, other: &LinkDriver) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn partner(&self) -> Option<Partner> {
        self.inner.partner.get()
    }

    pub fn set_partner(&self, partner: Partner) {
        self.inner.partner.set(Some(partner));
    }

    /// Whether the handshake is done and the link has not ended.
    pub fn linked(&self) -> bool {
        self.inner.linked.get() && self.is_up()
    }

    pub fn set_linked(&self) {
        if self.is_up() {
            self.inner.linked.set(true);
        }
    }

    /// The handshake must be done by `at`; [`LinkDriver::handshake_overdue`] ends the link after it.
    pub fn set_handshake_deadline(&self, at: Instant) {
        self.inner.handshake_by.set(Some(at));
    }

    /// At `now`, past the handshake's deadline with no handshake done: the link is closed and `true` is returned,
    /// for the caller to end the process at its other end and the pair to start another. What has already arrived
    /// is handed over first, so a `hello` that came just in time still links. A linked link, one that has ended,
    /// or one with no deadline is left alone, and `false` returned: its window is never ended for it.
    pub fn handshake_overdue(&self, now: Instant) -> bool {
        let Some(by) = self.inner.handshake_by.get() else {
            return false;
        };
        if now < by || self.linked() || !self.is_up() {
            return false;
        }
        self.pump();
        if self.linked() || !self.is_up() {
            return false;
        }
        eprintln!("[link] the second window did not finish the handshake in time; the link is closed");
        self.close("the handshake was not finished in time");
        true
    }

    /// Whether the link still runs, handshake or not.
    pub fn is_up(&self) -> bool {
        self.inner.ended.borrow().is_none() && self.inner.handle.borrow().is_some()
    }

    /// How the link ended, once it has.
    pub fn ended(&self) -> Option<LinkEnd> {
        self.inner.ended.borrow().clone()
    }
}

impl LinkSend for LinkDriver {
    fn send(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<(), LinkError> {
        LinkDriver::send(self, msg, fds)
    }

    fn send_confirmed(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<Receiver<io::Result<()>>, LinkError> {
        LinkDriver::send_confirmed(self, msg, fds)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use eitri_core::window_link::fdpass;
    use eitri_core::window_link::link::PresenceFlag;
    use std::time::{Duration, Instant};

    /// Two linked ends: a driver on one, the bare handle of the other window on the other.
    pub(crate) fn pair() -> (LinkDriver, LinkHandle, Receiver<Inbound>) {
        let (a, b) = fdpass::seqpacket_pair().unwrap();
        let (mine, inbound) = LinkHandle::start(a, PresenceFlag::default()).unwrap();
        let (theirs, their_inbound) = LinkHandle::start(b, PresenceFlag::default()).unwrap();
        (LinkDriver::new(mine, inbound), theirs, their_inbound)
    }

    /// Pumps until `done` holds, for at most five seconds: the link thread delivers on its own schedule.
    pub(crate) fn pump_until(driver: &LinkDriver, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            driver.pump();
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn messages_reach_the_handler_of_their_kind_and_unclaimed_ones_are_dropped() {
        let (driver, theirs, _their_inbound) = pair();
        let seen: Rc<RefCell<Vec<Msg>>> = Rc::default();
        {
            let seen = seen.clone();
            driver.on("away", move |_, msg, fds| {
                assert!(fds.is_empty());
                seen.borrow_mut().push(msg);
            });
        }
        theirs.send(Msg::Presence { held: true }, Vec::new()).unwrap();
        theirs
            .send(
                Msg::Away {
                    ids: vec!["terminal".into()],
                },
                Vec::new(),
            )
            .unwrap();
        assert!(pump_until(&driver, || !seen.borrow().is_empty()));
        assert_eq!(
            *seen.borrow(),
            vec![Msg::Away {
                ids: vec!["terminal".into()]
            }]
        );
        assert!(driver.is_up());
    }

    #[test]
    fn a_link_end_without_bye_leaves_window_2_unlinked() {
        let (driver, theirs, _their_inbound) = pair();
        driver.set_linked();
        assert!(driver.linked());
        let ends: Rc<RefCell<Vec<LinkEnd>>> = Rc::default();
        {
            let ends = ends.clone();
            driver.on_end(move |_, end| ends.borrow_mut().push(end.clone()));
        }
        // The other window dies: its end of the socket closes with no `bye`.
        drop(theirs);
        assert!(pump_until(&driver, || !ends.borrow().is_empty()));
        assert_eq!(*ends.borrow(), vec![LinkEnd::Eof]);
        assert!(!driver.linked() && !driver.is_up());
        assert_eq!(driver.ended(), Some(LinkEnd::Eof));
        driver.pump();
        assert_eq!(ends.borrow().len(), 1, "the end is reported once");
        assert!(
            driver.send(Msg::Presence { held: true }, Vec::new()).is_ok(),
            "a send after the end is dropped quietly"
        );
    }

    #[test]
    fn window_1s_clean_close_with_bye_also_leaves_window_2_unlinked() {
        let (driver, theirs, _their_inbound) = pair();
        driver.set_linked();
        let ends: Rc<RefCell<Vec<LinkEnd>>> = Rc::default();
        {
            let ends = ends.clone();
            driver.on_end(move |_, end| ends.borrow_mut().push(end.clone()));
        }
        theirs.close("window 1 closed");
        assert!(pump_until(&driver, || !ends.borrow().is_empty()));
        assert_eq!(
            *ends.borrow(),
            vec![LinkEnd::Bye {
                why: "window 1 closed".into()
            }],
            "a clean close ends the link for window 2 as a crash does, and says so"
        );
        assert!(!driver.linked());
    }

    #[test]
    fn closing_the_link_here_says_bye_and_reports_no_end_to_this_window() {
        let (driver, _theirs, their_inbound) = pair();
        let ends: Rc<Cell<u32>> = Rc::default();
        {
            let ends = ends.clone();
            driver.on_end(move |_, _| ends.set(ends.get() + 1));
        }
        driver.close("window 2 closed");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            match their_inbound.recv_timeout(Duration::from_millis(50)) {
                Ok(Inbound::Msg { msg, .. }) => got.push(msg),
                Ok(Inbound::Closed) => break,
                _ => {}
            }
        }
        assert_eq!(
            got,
            vec![Msg::Bye {
                why: "window 2 closed".into()
            }]
        );
        driver.pump();
        assert_eq!(ends.get(), 0, "the window that closed the link is not told it ended");
        assert!(!driver.is_up());
    }

    #[test]
    fn a_handshake_not_done_by_its_deadline_drops_the_link_so_another_window_can_start() {
        use crate::linked_window::{WindowNew, WindowRole};
        // A clock of the test's own: instants it makes up, never the time it takes to run.
        let start = Instant::now();
        let (driver, _theirs, _their_inbound) = pair();
        driver.set_handshake_deadline(start + HANDSHAKE_WITHIN);
        let role = WindowRole::Main {
            link: Some(driver.clone()),
        };
        assert!(!driver.handshake_overdue(start + HANDSHAKE_WITHIN - Duration::from_millis(1)));
        assert!(driver.is_up(), "before the deadline the start is still waited for");
        assert_eq!(role.window_new(), WindowNew::Raise);

        assert!(driver.handshake_overdue(start + HANDSHAKE_WITHIN));
        assert!(!driver.is_up(), "past the deadline the link is gone");
        assert_eq!(
            role.window_new(),
            WindowNew::Spawn,
            "and window.new starts another window"
        );
        assert!(
            !driver.handshake_overdue(start + HANDSHAKE_WITHIN * 2),
            "it is dropped once"
        );

        let (linked, _theirs, _inbound) = pair();
        linked.set_handshake_deadline(start + HANDSHAKE_WITHIN);
        linked.set_linked();
        assert!(!linked.handshake_overdue(start + HANDSHAKE_WITHIN * 2));
        assert!(linked.linked(), "a link whose handshake is done keeps no deadline");

        let (no_deadline, _theirs, _inbound) = pair();
        assert!(!no_deadline.handshake_overdue(start + HANDSHAKE_WITHIN * 2));
        assert!(no_deadline.is_up());
    }

    /// Waits, for at most five seconds, until the link thread has queued something for `driver`, without handing it
    /// over: the wake descriptor turns readable only after the message is in the queue.
    fn wait_queued(driver: &LinkDriver) -> bool {
        let handle = driver.inner.handle.borrow();
        let mut pfd = libc::pollfd {
            fd: handle.as_ref().expect("the link is up").wake_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one `pollfd` that lives for the call.
        unsafe { libc::poll(&mut pfd, 1, 5000) == 1 }
    }

    #[test]
    fn a_hello_already_waiting_when_the_deadline_passes_still_links() {
        let start = Instant::now();
        let (driver, theirs, _their_inbound) = pair();
        driver.set_handshake_deadline(start + HANDSHAKE_WITHIN);
        driver.on("hello", |link, _, _| link.set_linked());
        theirs
            .send(
                crate::linked_window::spawn::hello("0.4.0+abc".into(), &WindowName::Second, 4242, 99),
                Vec::new(),
            )
            .unwrap();
        assert!(wait_queued(&driver), "the hello reached this side's queue");
        assert!(
            !driver.handshake_overdue(start + HANDSHAKE_WITHIN),
            "a hello that arrived in time is handed over before the deadline is judged"
        );
        assert!(driver.linked(), "and it links instead of being dropped");
    }

    #[test]
    fn a_wake_with_no_message_still_runs_the_wake_handlers() {
        let (driver, _theirs, _their_inbound) = pair();
        let (wakes, drains): (Rc<Cell<u32>>, Rc<Cell<u32>>) = Default::default();
        {
            let wakes = wakes.clone();
            driver.on_wake(move |_| wakes.set(wakes.get() + 1));
            let drains = drains.clone();
            driver.on_drained(move |_| drains.set(drains.get() + 1));
        }
        // A message the other end's link does not answer: `presence` draws an acknowledgement, which arrives here
        // as a message whenever it beats the pump.
        let answer = driver.send_confirmed(Msg::MoveDone { mv: 1 }, Vec::new()).unwrap();
        assert!(
            answer.recv_timeout(Duration::from_secs(5)).unwrap().is_ok(),
            "the packet was written"
        );
        assert!(wait_queued(&driver), "the answer woke the link");
        driver.pump();
        assert_eq!(
            (wakes.get(), drains.get()),
            (1, 0),
            "the wake is reported though nothing arrived"
        );
    }

    #[test]
    fn a_confirmed_send_after_the_end_answers_that_it_was_never_written() {
        let (driver, theirs, _their_inbound) = pair();
        drop(theirs);
        assert!(pump_until(&driver, || !driver.is_up()));
        let answer = driver.send_confirmed(Msg::MoveDone { mv: 1 }, Vec::new()).unwrap();
        assert!(answer.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
    }

    /// A window controller holding one window, on the fake host, for the link to attach to.
    pub(crate) fn controller() -> (Rc<Driver<Controller>>, Rc<eitri_core::wm::fake::FakeHost>) {
        controller_in(eitri_core::wm::window::Tiling::Eitri)
    }

    /// [`controller`], its window tiled as `tiling`.
    pub(crate) fn controller_in(
        tiling: eitri_core::wm::window::Tiling,
    ) -> (Rc<Driver<Controller>>, Rc<eitri_core::wm::fake::FakeHost>) {
        use eitri_core::layout::{Layout, ModuleDecl, ModuleId, ModuleKeys, Placement, Workspaces};
        use eitri_core::wm::fake::{FakeHost, FakeScheduler};
        use eitri_core::wm::start::StartStep;
        use eitri_core::wm::{EditorPresence, HostSink, WindowParts, WindowRole, WindowSpec};
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let driver = Driver::new(Controller::new(scheduler.clone()), host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        let keymap = Rc::new(eitri_core::keymap::Keymap::defaults());
        let module_keys = Rc::new(ModuleKeys::build(&[], &keymap).unwrap());
        for step in [
            StartStep::Feeds {
                spawn: eitri_core::wm::EditorSpawn {
                    env: vec![],
                    args: vec![],
                    cwd: None,
                    clean: true,
                    background: None,
                    open: vec![],
                },
            },
            StartStep::Window {
                spec: WindowSpec {
                    role: WindowRole::Main,
                    presence: EditorPresence::Owned,
                    title: "project".into(),
                },
            },
            StartStep::Editor,
            StartStep::Kernel,
            StartStep::Config { keymap, module_keys },
            StartStep::Layout {
                own_window: None,
                tiling,
                parts: WindowParts::new(
                    Layout::initial(&[ModuleDecl {
                        id: ModuleId::terminal(),
                        placement: Placement::BelowRoot,
                    }])
                    .unwrap(),
                    Workspaces::single(),
                    None,
                ),
            },
        ] {
            driver.start(WinId::MAIN, step);
        }
        (driver, host)
    }

    fn raises(host: &eitri_core::wm::fake::FakeHost) -> usize {
        host.log()
            .iter()
            .filter(|logged| {
                matches!(
                    logged,
                    eitri_core::wm::fake::Logged::Op(eitri_core::wm::HostOp::RaisePeer { .. }, _)
                )
            })
            .count()
    }

    #[test]
    fn the_link_is_the_controllers_peer_until_it_ends() {
        use eitri_core::keymap::Action;
        use eitri_core::wm::Origin;
        let (wm, host) = controller();
        let (driver, theirs, _their_inbound) = pair();
        driver.attach_controller(WinId::MAIN, PeerId(2), WindowName::Second, &wm);
        wm.run(WinId::MAIN, Action::WindowOther, Origin::Key { keycode: 0 });
        assert_eq!(
            raises(&host),
            1,
            "with the link attached, window.other raises the other window"
        );

        drop(theirs);
        assert!(pump_until(&driver, || !driver.is_up()));
        wm.run(WinId::MAIN, Action::WindowOther, Origin::Key { keycode: 0 });
        assert_eq!(
            raises(&host),
            1,
            "once the link has ended there is no other window to raise"
        );
    }

    #[test]
    fn an_offer_this_window_cannot_read_is_refused_on_the_link() {
        let (wm, _host) = controller();
        let (driver, theirs, their_inbound) = pair();
        driver.attach_controller(WinId::MAIN, PeerId(2), WindowName::Second, &wm);
        theirs
            .send(
                Msg::MoveOffer {
                    mv: 4,
                    module: "terminal#2".into(),
                    kind: "spreadsheet".into(),
                    shape: eitri_core::window_link::Shape::Tiled,
                    size: (10, 10),
                    session: None,
                },
                Vec::new(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut refused = None;
        while refused.is_none() && Instant::now() < deadline {
            driver.pump();
            // The attach sends this window's counts at once; only the refusal is the point here.
            if let Ok(Inbound::Msg { msg, .. }) = their_inbound.recv_timeout(Duration::from_millis(20)) {
                if !matches!(msg, Msg::Summary(_)) {
                    refused = Some(msg);
                }
            }
        }
        assert!(
            matches!(refused, Some(Msg::MoveRefused { mv: 4, .. })),
            "the other window hears the refusal: {refused:?}"
        );
    }

    #[test]
    #[should_panic(expected = "no message kind")]
    fn a_handler_for_a_kind_the_link_does_not_know_is_refused() {
        let (driver, _theirs, _their_inbound) = pair();
        driver.on("helo", |_, _, _| {});
    }
}
