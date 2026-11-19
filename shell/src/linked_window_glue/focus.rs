use std::cell::RefCell;
use std::rc::Rc;

use crate::companion;
use eitri_core::wm::{HostEvent, Monitor, MonitorId, WinId};
use gtk4::prelude::*;
use gtk4::{gdk, gio, ApplicationWindow};

/// Brings a window of the pair forward after its first show, when the other window had the keys, and any window a
/// file was handed to. GNOME lets a window take the keys from another only with an activation token naming that other
/// window's surface, which the other window sends. sway, Hyprland and niri turn a token activation into an urgency
/// hint, so there the window also has the compositor's own tool focus it, on a worker thread (the adapter is started
/// on first use).
#[derive(Clone)]
pub(crate) struct PairRaise {
    wm: eitri_core::compositor::Wm,
    adapter: Rc<std::cell::OnceCell<Rc<companion::wm_runner::WmRunner>>>,
    /// The next show leaves the keys where they are: a window 2 started again at a launch, whose first show must
    /// not take them from window 1. Shared by every copy, so the one the host raises with sees it.
    keep_keys_once: Rc<std::cell::Cell<bool>>,
    before_map: Rc<RefCell<Option<std::sync::mpsc::Receiver<Option<eitri_core::compositor::WindowRef>>>>>,
}

impl PairRaise {
    pub(crate) fn new(choice: eitri_core::compositor::WmChoice) -> PairRaise {
        PairRaise::with_wm(eitri_core::compositor::resolve(choice, &|name| std::env::var_os(name)))
    }

    fn with_wm(wm: eitri_core::compositor::Wm) -> PairRaise {
        PairRaise {
            wm,
            adapter: Rc::default(),
            keep_keys_once: Rc::default(),
            before_map: Rc::default(),
        }
    }

    /// A raise whose adapter is `runner`, for a test of what it runs and where.
    #[cfg(test)]
    fn with_runner(wm: eitri_core::compositor::Wm, runner: Rc<companion::wm_runner::WmRunner>) -> PairRaise {
        let raise = PairRaise::with_wm(wm);
        raise.adapter.set(runner).ok();
        raise
    }

    /// The next show only presents the window: no compositor tool focuses it.
    pub(crate) fn keep_keys_at_next_show(&self) {
        self.keep_keys_once.set(true);
    }

    /// Readies the raise for this window's first show, and says whether the host presents through it. A window of a
    /// pair always does. One outside a pair does when it takes files from outside: a file handed to it while another
    /// application has the keys must bring it forward. Nothing asks such a window forward at its first show, so that
    /// show stays a plain present, as a restored window's does.
    pub(crate) fn used_for_window(&self, in_pair: bool, restored: bool, takes_opens: bool) -> bool {
        if restored || !in_pair {
            self.keep_keys_at_next_show();
        }
        in_pair || takes_opens
    }

    /// Whether this show has the compositor's own tool focus the window: on sway, Hyprland and niri, unless the
    /// show is the one that keeps the keys where they are.
    fn focuses(&self) -> bool {
        use eitri_core::compositor::Wm;
        let kept = self.keep_keys_once.replace(false);
        !kept && matches!(self.wm, Wm::Sway | Wm::Hyprland | Wm::Niri)
    }

    /// Starts the snapshot before the first present. Only module windows use it; a pair keeps its own placement.
    pub(crate) fn prepare_module(&self) {
        *self.before_map.borrow_mut() = Some(self.runner().before_map());
    }

    pub(crate) fn raise(&self, window: &impl IsA<gtk4::Window>, token: Option<&str>) {
        if let Some(receive) = self.before_map.borrow_mut().take() {
            let (raise, window, token) = (
                self.clone(),
                window.clone().upcast::<gtk4::Window>().downgrade(),
                token.map(str::to_owned),
            );
            let until = std::time::Instant::now() + std::time::Duration::from_millis(500);
            gtk4::glib::timeout_add_local(std::time::Duration::from_millis(5), move || {
                let before = match receive.try_recv() {
                    Ok(before) => before,
                    Err(std::sync::mpsc::TryRecvError::Empty) if std::time::Instant::now() < until => {
                        return gtk4::glib::ControlFlow::Continue
                    }
                    Err(_) => None,
                };
                raise.runner().module_mapped_from(before);
                if let Some(window) = window.upgrade() {
                    raise.raise(&window, token.as_deref());
                }
                gtk4::glib::ControlFlow::Break
            });
            return;
        }
        present_with_token(window, token);
        if self.focuses() {
            self.runner().raise_pid(std::process::id());
        }
    }

    /// Brings `window` forward for a request from another process: presented with `token`, and on sway, Hyprland
    /// and niri focused by this process's pid through the compositor's own tool, whatever the next show was to keep.
    pub(crate) fn present_and_focus(&self, window: &impl IsA<gtk4::Window>, token: Option<&str>) {
        use eitri_core::compositor::Wm;
        present_with_token(window, token);
        if matches!(self.wm, Wm::Sway | Wm::Hyprland | Wm::Niri) {
            self.runner().raise_pid(std::process::id());
        }
    }

    /// The window manager adapter every use of this window shares: the raise, the output moves and the focus moves
    /// past the window's edge. Started on first use.
    fn runner(&self) -> &Rc<companion::wm_runner::WmRunner> {
        let wm = self.wm;
        self.adapter.get_or_init(|| companion::wm_runner::WmRunner::new(wm))
    }
}

/// Presents `window` with the activation token another window gave, when there is one: GNOME lets a window take the
/// keys from another only with a token naming that other window's surface. Any process may present a window so; on a
/// tiling compositor the token becomes an urgency hint, and the compositor's own tool has to focus it as well.
pub(crate) fn present_with_token(window: &impl IsA<gtk4::Window>, token: Option<&str>) {
    if let Some(token) = token {
        window.set_startup_id(token);
    }
    window.present();
}

impl crate::gtk_host::PairRaise for PairRaise {
    fn raise(&self, window: &gtk4::Window, token: Option<&str>) {
        PairRaise::raise(self, window, token);
    }

    fn move_focus(&self, direction: eitri_core::layout::Direction, on_none: &dyn Fn(&str)) -> bool {
        self.runner().move_focus(direction, on_none)
    }
}

/// A window put back on its monitor uses the same adapter as its raise: one per window, started on first use.
impl crate::gtk_host::MonitorPlacer for PairRaise {
    fn wm(&self) -> eitri_core::compositor::Wm {
        self.wm
    }

    fn place(&self, how: crate::gtk_host::MonitorMove) {
        if self.wm == eitri_core::compositor::Wm::None {
            return;
        }
        self.runner().move_own(how);
    }
}

/// The hub's own snapshot. Peer ids identify this run's links; only stable window names reach disk.
pub(crate) struct CmSession {
    pub(crate) file: eitri_core::window_link::cm_file::CmFile,
    peers: std::collections::BTreeMap<eitri_core::wm::PeerId, String>,
    /// The outputs window 1 sees now, by connector; empty until its first report.
    outputs: Vec<String>,
    writer: eitri_core::window_link::cm_file::CmWriter,
    closing: bool,
}

impl CmSession {
    pub(crate) fn new(
        dir: &std::path::Path,
        root: &std::path::Path,
        file: eitri_core::window_link::cm_file::CmFile,
    ) -> std::io::Result<Self> {
        Ok(Self {
            file,
            peers: Default::default(),
            outputs: Vec::new(),
            writer: eitri_core::window_link::cm_file::CmWriter::new(dir.to_owned(), root.to_owned())?,
            closing: false,
        })
    }

    pub(crate) fn linked(&mut self, peer: eitri_core::wm::PeerId, name: &eitri_core::window_link::names::WindowName) {
        if self.closing {
            return;
        }
        let text = name.text();
        self.peers.insert(peer, text.clone());
        if !self.file.windows.iter().any(|w| w.name == text) {
            self.file.windows.push(eitri_core::window_link::cm_file::CmWindow {
                name: text,
                output: None,
                order: self.file.windows.len() as u32,
            });
        }
        self.writer.changed(self.file.clone());
    }

    /// A window's link ended, `linked` saying the window had got as far as a link. Its line stays, and with it its output
    /// and its place at the next start, until [`CmSession::window_closed`] says the user closed it. One that never linked
    /// has no place of its own to keep: a start that keeps failing must not come back every launch.
    pub(crate) fn link_ended(
        &mut self,
        peer: eitri_core::wm::PeerId,
        name: &eitri_core::window_link::names::WindowName,
        linked: bool,
    ) {
        if self.closing {
            return;
        }
        self.peers.remove(&peer);
        if !linked {
            self.window_closed(name);
        }
    }

    /// A window is closed, and leaves the record.
    pub(crate) fn window_closed(&mut self, name: &eitri_core::window_link::names::WindowName) {
        if self.closing {
            return;
        }
        self.file.windows.retain(|w| w.name != name.text());
        self.reorder();
        self.writer.changed(self.file.clone());
    }

    fn reorder(&mut self) {
        self.file.windows.sort_by_key(|w| w.order);
        for (order, window) in self.file.windows.iter_mut().enumerate() {
            window.order = order as u32;
        }
    }

    /// The outputs that exist now, from window 1's monitor watch.
    pub(crate) fn outputs_changed(&mut self, monitors: &[Monitor]) {
        self.outputs = monitors.iter().map(|monitor| monitor.id.0.clone()).collect();
    }

    pub(crate) fn event(&mut self, event: &eitri_core::wm::PeerEvent) {
        use eitri_core::wm::PeerEvent;
        if self.closing {
            return;
        }
        let Some(name) = self.peers.get(&event.peer()).cloned() else {
            return;
        };
        match event {
            PeerEvent::Monitor { connector, .. } if eitri_core::compositor::valid_output_name(connector) => {
                let outputs = &self.outputs;
                if let Some(window) = self.file.windows.iter_mut().find(|w| w.name == name) {
                    // Where a window opens without its output says nothing about where the user wants it, so the
                    // record keeps that output until it returns.
                    let gone = |output: &str| !outputs.is_empty() && !outputs.iter().any(|known| known == output);
                    if !window.output.as_deref().is_some_and(gone) {
                        window.output = Some(connector.clone());
                    }
                }
            }
            PeerEvent::WindowFocused { .. } => {
                // The most recently focused window goes last, so a restart opens it last and it keeps the focus.
                let before = self.file.windows.clone();
                self.reorder();
                if let Some(last) = self.file.windows.iter().position(|w| w.name == name) {
                    let window = self.file.windows.remove(last);
                    self.file.windows.push(window);
                }
                for (order, window) in self.file.windows.iter_mut().enumerate() {
                    window.order = order as u32;
                }
                if self.file.windows == before {
                    return;
                }
            }
            _ => return,
        }
        self.writer.changed(self.file.clone());
    }

    #[cfg(test)]
    pub(crate) fn terminal(&mut self, layout: &eitri_core::layout::Layout) {
        self.record_terminal(eitri_core::window_link::cm_file::CmTerminal::of(layout));
    }

    /// The bottom terminal as the record keeps it, which the window's controller decides.
    pub(crate) fn record_terminal(&mut self, terminal: eitri_core::window_link::cm_file::CmTerminal) {
        if self.closing {
            return;
        }
        if self.file.terminal != terminal {
            self.file.terminal = terminal;
            self.writer.changed(self.file.clone());
        }
    }

    /// The application stays alive for the worker's final write without holding up GTK's close operations.
    pub(crate) fn finish(&mut self, app: &gtk4::Application) {
        if self.closing {
            return;
        }
        self.closing = true;
        let answer = self.writer.finish(self.file.clone());
        let until = std::time::Instant::now() + eitri_core::wm::persist::CLOSE_FLUSH_BOUND;
        let mut hold = Some(app.hold());
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(10), move || {
            match answer.try_recv() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => eprintln!("[layout] compositor layout: {err}"),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    eprintln!("[layout] compositor layout writer ended")
                }
                Err(std::sync::mpsc::TryRecvError::Empty) if std::time::Instant::now() < until => {
                    return gtk4::glib::ControlFlow::Continue
                }
                Err(_) => eprintln!("[layout] compositor layout was still being written after 5 seconds"),
            }
            hold.take();
            gtk4::glib::ControlFlow::Break
        });
    }
}

fn monitors(list: &gio::ListModel) -> Vec<Monitor> {
    (0..list.n_items())
        .filter_map(|i| {
            let monitor = list.item(i)?.downcast::<gdk::Monitor>().ok()?;
            let connector = monitor.connector()?.to_string();
            let geometry = monitor.geometry();
            Some(Monitor {
                id: MonitorId(connector.clone()),
                name: monitor.description().map(|s| s.to_string()).unwrap_or(connector),
                rect: eitri_core::layout::Rect {
                    x: geometry.x(),
                    y: geometry.y(),
                    w: geometry.width(),
                    h: geometry.height(),
                },
                scale: monitor.scale(),
            })
        })
        .collect()
}

/// GTK can list an output that returns before it knows the output's connector, which is the only name `monitors` can
/// give it, and the list does not change again when the name comes. So each unnamed monitor is watched for its name,
/// and the list is reported again then. Monitors that left the list are let go.
fn report_when_named(
    list: &gio::ListModel,
    unnamed: &Rc<RefCell<Vec<(gdk::Monitor, gtk4::glib::SignalHandlerId)>>>,
    post: &Rc<dyn Fn(HostEvent)>,
) {
    let listed: Vec<gdk::Monitor> = (0..list.n_items())
        .filter_map(|i| list.item(i)?.downcast::<gdk::Monitor>().ok())
        .collect();
    let mut unnamed = unnamed.borrow_mut();
    let (kept, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut *unnamed)
        .into_iter()
        .partition(|(monitor, _)| listed.contains(monitor));
    *unnamed = kept;
    for (monitor, signal) in gone {
        monitor.disconnect(signal);
    }
    for monitor in listed {
        if monitor.connector().is_some() || unnamed.iter().any(|(watched, _)| *watched == monitor) {
            continue;
        }
        let (list, post) = (list.downgrade(), post.clone());
        let signal = monitor.connect_connector_notify(move |_| {
            if let Some(list) = list.upgrade() {
                post(HostEvent::MonitorsChanged {
                    monitors: monitors(&list),
                });
            }
        });
        unnamed.push((monitor, signal));
    }
}

/// Reports output facts only; navigation and crossing between windows are the controller's decisions.
pub(crate) fn watch(window: &ApplicationWindow, win: WinId, post: Rc<dyn Fn(HostEvent)>) {
    let display = WidgetExt::display(window);
    let list = display.monitors();
    post(HostEvent::MonitorsChanged {
        monitors: monitors(&list),
    });
    let unnamed: Rc<RefCell<Vec<(gdk::Monitor, gtk4::glib::SignalHandlerId)>>> = Rc::default();
    report_when_named(&list, &unnamed, &post);
    let changed = post.clone();
    let waiting = unnamed.clone();
    let signal = RefCell::new(Some(list.connect_items_changed(move |list, _, _, _| {
        changed(HostEvent::MonitorsChanged {
            monitors: monitors(list),
        });
        report_when_named(list, &waiting, &changed);
    })));
    window.connect_destroy(move |_| {
        if let Some(signal) = signal.borrow_mut().take() {
            list.disconnect(signal);
        }
        for (monitor, signal) in unnamed.borrow_mut().drain(..) {
            monitor.disconnect(signal);
        }
    });
    let attach = move |window: &ApplicationWindow| {
        let Some(surface) = window.surface() else { return };
        let entered: Rc<RefCell<Vec<gdk::Monitor>>> = Rc::default();
        let report = {
            let entered = entered.clone();
            let post = post.clone();
            Rc::new(move || {
                let monitor = entered
                    .borrow()
                    .last()
                    .and_then(|m| m.connector())
                    .map(|c| MonitorId(c.to_string()));
                post(HostEvent::WindowMonitor { win, monitor });
            })
        };
        report();
        let on_enter = report.clone();
        let held = entered.clone();
        surface.connect_enter_monitor(move |_, monitor| {
            let mut held = held.borrow_mut();
            held.retain(|m| m != monitor);
            held.push(monitor.clone());
            drop(held);
            on_enter();
        });
        surface.connect_leave_monitor(move |_, monitor| {
            entered.borrow_mut().retain(|m| m != monitor);
            report();
        });
    };
    if window.is_realized() {
        attach(window);
    }
    window.connect_realize(attach);
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::compositor::Wm;

    #[test]
    fn the_hub_snapshot_tracks_links_outputs_and_terminal_changes_and_freezes_for_close() {
        use eitri_core::window_link::cm_file::{read_cm, CmFile, CmTerminal, ReadCm};
        use eitri_core::window_link::names::WindowName;
        use eitri_core::wm::{PeerEvent, PeerId};
        let base = std::env::temp_dir().join(format!("eitri-hub-snapshot-{}", uuid::Uuid::new_v4()));
        let dir = base.join("layout");
        let root = std::path::Path::new("/project");
        let layout = eitri_core::layout::hub_tree().layout;
        let mut session = CmSession::new(&dir, root, CmFile::new(&layout, &[WindowName::Chat(1)])).unwrap();
        session.linked(PeerId(16), &WindowName::Chat(1));
        session.linked(PeerId(17), &WindowName::Page("markdown".into()));
        session.event(&PeerEvent::Monitor {
            peer: PeerId(17),
            connector: "DP-2".into(),
        });
        session.event(&PeerEvent::Monitor {
            peer: PeerId(17),
            connector: "a;b".into(),
        });
        session.event(&PeerEvent::WindowFocused { peer: PeerId(17) });
        session.link_ended(PeerId(16), &WindowName::Chat(1), true);
        session.window_closed(&WindowName::Chat(1));
        let terminal = CmTerminal {
            shown: false,
            px: Some(220),
        };
        session.terminal(&terminal.layout(terminal.shown));
        assert_eq!(session.file.terminal, terminal);
        assert_eq!(session.file.windows.len(), 1);
        assert_eq!(session.file.windows[0].order, 0);
        assert_eq!(session.file.windows[0].output.as_deref(), Some("DP-2"));
        let final_file = session.file.clone();
        session.closing = true;
        session.link_ended(PeerId(17), &WindowName::Page("markdown".into()), true);
        session.window_closed(&WindowName::Page("markdown".into()));
        session.linked(PeerId(18), &WindowName::Chat(2));
        session.terminal(&layout);
        assert_eq!(session.file, final_file);
        session
            .writer
            .finish(final_file.clone())
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(read_cm(&dir, root), ReadCm::Read(final_file, vec![]));
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A window that opens elsewhere because its output is gone leaves the output on record for the next start. One
    /// that moves between outputs that exist, or whose output has returned, is recorded where it is.
    #[test]
    fn a_recorded_output_that_is_gone_stays_on_record_until_it_returns() {
        use eitri_core::window_link::cm_file::{CmFile, CmWindow};
        use eitri_core::window_link::names::WindowName;
        use eitri_core::wm::{PeerEvent, PeerId};
        let base = std::env::temp_dir().join(format!("eitri-hub-output-{}", uuid::Uuid::new_v4()));
        let layout = eitri_core::layout::hub_tree().layout;
        let mut file = CmFile::new(&layout, &[]);
        file.windows.push(CmWindow {
            name: "c1".into(),
            output: Some("DP-2".into()),
            order: 0,
        });
        let mut session = CmSession::new(&base.join("layout"), std::path::Path::new("/project"), file).unwrap();
        session.linked(PeerId(16), &WindowName::Chat(1));
        let on = |session: &mut CmSession, connector: &str| {
            session.event(&PeerEvent::Monitor {
                peer: PeerId(16),
                connector: connector.into(),
            });
            session.file.output(&WindowName::Chat(1)).map(str::to_owned)
        };
        let outputs = |names: &[&str]| -> Vec<Monitor> {
            names
                .iter()
                .map(|name| Monitor {
                    id: MonitorId((*name).into()),
                    name: (*name).into(),
                    rect: eitri_core::layout::Rect { x: 0, y: 0, w: 1, h: 1 },
                    scale: 1.0,
                })
                .collect()
        };

        session.outputs_changed(&outputs(&["DP-1"]));
        assert_eq!(on(&mut session, "DP-1").as_deref(), Some("DP-2"), "DP-2 is gone");
        session.outputs_changed(&outputs(&["DP-1", "DP-2"]));
        assert_eq!(on(&mut session, "DP-2").as_deref(), Some("DP-2"));
        assert_eq!(on(&mut session, "DP-1").as_deref(), Some("DP-1"), "the user moved it");
        session
            .writer
            .finish(session.file.clone())
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Only a window the user closed leaves the record: a crashed one keeps its line and its output, and the window that
    /// takes that name again is the same line.
    #[test]
    fn a_window_that_went_without_the_user_keeps_its_line() {
        use eitri_core::window_link::cm_file::CmFile;
        use eitri_core::window_link::names::WindowName;
        use eitri_core::wm::{PeerEvent, PeerId};
        let base = std::env::temp_dir().join(format!("eitri-hub-crash-{}", uuid::Uuid::new_v4()));
        let dir = base.join("layout");
        let layout = eitri_core::layout::hub_tree().layout;
        let root = std::path::Path::new("/project");
        let mut session = CmSession::new(&dir, root, CmFile::new(&layout, &[])).unwrap();
        let page = WindowName::Page("markdown".into());
        session.linked(PeerId(16), &WindowName::Chat(1));
        session.linked(PeerId(17), &WindowName::Chat(2));
        session.linked(PeerId(18), &page);
        session.event(&PeerEvent::Monitor {
            peer: PeerId(17),
            connector: "DP-2".into(),
        });
        let names = |session: &CmSession| session.file.ordered_windows();

        session.link_ended(PeerId(17), &WindowName::Chat(2), true);
        session.link_ended(PeerId(18), &page, true);
        assert_eq!(
            names(&session),
            [WindowName::Chat(1), WindowName::Chat(2), page.clone()]
        );
        assert_eq!(session.file.output(&WindowName::Chat(2)), Some("DP-2"));

        // The ended link's reports are not the window's any more, and a respawn does not repeat the line.
        session.event(&PeerEvent::Monitor {
            peer: PeerId(17),
            connector: "DP-3".into(),
        });
        assert_eq!(session.file.output(&WindowName::Chat(2)), Some("DP-2"));
        session.linked(PeerId(19), &WindowName::Chat(2));
        assert_eq!(names(&session).len(), 3);

        session.link_ended(PeerId(19), &WindowName::Chat(2), true);
        assert_eq!(
            names(&session).len(),
            3,
            "a close leaves the line for the close's burst to judge"
        );
        session.window_closed(&WindowName::Chat(2));
        assert_eq!(names(&session), [WindowName::Chat(1), page]);

        session.link_ended(PeerId(16), &WindowName::Chat(1), false);
        assert_eq!(names(&session).len(), 1, "a window that never linked keeps no place");

        let final_file = session.file.clone();
        session
            .writer
            .finish(final_file)
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn the_focused_window_moves_to_the_end_of_the_recorded_order() {
        use eitri_core::window_link::cm_file::{read_cm, CmFile, ReadCm};
        use eitri_core::window_link::names::WindowName;
        use eitri_core::wm::{PeerEvent, PeerId};
        let base = std::env::temp_dir().join(format!("eitri-hub-focus-{}", uuid::Uuid::new_v4()));
        let dir = base.join("layout");
        let root = std::path::Path::new("/project");
        let layout = eitri_core::layout::hub_tree().layout;
        let mut session = CmSession::new(&dir, root, CmFile::new(&layout, &[])).unwrap();
        session.linked(PeerId(1), &WindowName::Chat(1));
        session.linked(PeerId(2), &WindowName::Chat(2));
        session.linked(PeerId(3), &WindowName::Chat(3));
        session.event(&PeerEvent::WindowFocused { peer: PeerId(1) });
        assert_eq!(
            session.file.ordered_windows(),
            vec![WindowName::Chat(2), WindowName::Chat(3), WindowName::Chat(1)]
        );
        session.event(&PeerEvent::WindowFocused { peer: PeerId(1) });
        session.event(&PeerEvent::WindowFocused { peer: PeerId(99) });
        let orders: Vec<u32> = session.file.windows.iter().map(|w| w.order).collect();
        assert_eq!(orders.iter().copied().max(), Some(2));
        let final_file = session.file.clone();
        session
            .writer
            .finish(final_file.clone())
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        match read_cm(&dir, root) {
            ReadCm::Read(file, _) => assert_eq!(file.ordered_windows(), final_file.ordered_windows()),
            other => panic!("the order was not written: {other:?}"),
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_module_snapshots_the_focused_window_on_a_worker_before_mapping() {
        use std::sync::mpsc;
        let (sent, received) = mpsc::channel();
        let runner = companion::wm_runner::WmRunner::with_exec(Wm::Sway, move |_| {
            sent.send(std::thread::current().id()).unwrap();
            Ok(r#"{"type":"workspace","name":"1","nodes":[{"type":"con","pid":123,"focused":true}]}"#.into())
        });
        let raise = PairRaise::with_runner(Wm::Sway, runner);
        raise.prepare_module();
        assert_ne!(
            received.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
            std::thread::current().id()
        );
        let receive = raise.before_map.borrow_mut().take().unwrap();
        let before = receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(before.pid, 123);
        assert_eq!(before.workspace.as_deref(), Some("1"));
    }

    #[test]
    fn a_show_that_keeps_the_keys_is_not_focused_by_the_compositor() {
        for wm in [Wm::Sway, Wm::Hyprland, Wm::Niri] {
            let raise = PairRaise::with_wm(wm);
            let copy = raise.clone();
            raise.keep_keys_at_next_show();
            assert!(
                !copy.focuses(),
                "{wm:?}: the show that keeps the keys asks nothing of the compositor"
            );
            assert!(copy.focuses(), "{wm:?}: only that one show");
        }
        let gnome = PairRaise::with_wm(Wm::None);
        assert!(!gnome.focuses(), "without a compositor tool the token alone raises");
    }

    #[test]
    fn compositor_focus_runs_the_adapter_off_the_gtk_thread() {
        use crate::gtk_host::PairRaise as _;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};
        use std::time::Duration;
        let call_returned = Arc::new(AtomicBool::new(false));
        let (ran, heard) = mpsc::channel();
        let exec = {
            let call_returned = call_returned.clone();
            move |argv: Vec<String>| {
                // Held until the caller has returned: a command run on the caller's own thread never gets here.
                let waited = std::time::Instant::now();
                while !call_returned.load(Ordering::SeqCst) && waited.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                let _ = ran.send((std::thread::current().id(), call_returned.load(Ordering::SeqCst), argv));
                Ok(String::new())
            }
        };
        let runner = companion::wm_runner::WmRunner::with_exec(eitri_core::compositor::Wm::Sway, exec);
        let raise = PairRaise::with_runner(eitri_core::compositor::Wm::Sway, runner);
        let claimed = raise.move_focus(eitri_core::layout::Direction::Right, &|text| {
            panic!("no notice: {text}")
        });
        call_returned.store(true, Ordering::SeqCst);
        assert!(claimed, "the key is claimed at once");
        let (thread, after_return, argv) = heard.recv_timeout(Duration::from_secs(10)).expect("the adapter ran");
        assert_ne!(thread, std::thread::current().id(), "the command runs on a worker");
        assert!(after_return, "the call returned before the command ran");
        assert_eq!(argv.first().map(String::as_str), Some("swaymsg"));
    }

    #[test]
    fn a_new_module_window_raises_itself_once_with_its_spawn_token() {
        use crate::linked_window::spawn::{Pair, PairEnv};
        use eitri_core::window_link::names::WindowName;
        // The token the spawn carried is taken once, by the first show.
        let (_one, two) = eitri_core::window_link::fdpass::seqpacket_pair().unwrap();
        let pair = Pair::module(
            PairEnv::for_tests(),
            two,
            WindowName::Chat(2),
            std::path::Path::new("/p"),
        )
        .unwrap();
        pair.set_activation(Some("spawn-token".into()));
        assert_eq!(pair.take_activation().as_deref(), Some("spawn-token"));
        assert_eq!(pair.take_activation(), None, "no later show raises with it again");
        // That show only presents (no compositor tool): the focus follows once the window is mapped, in its placement,
        // and later shows raise as any other window's do.
        for wm in [Wm::Sway, Wm::Hyprland, Wm::Niri] {
            let raise = PairRaise::with_wm(wm);
            raise.keep_keys_at_next_show();
            assert!(!raise.focuses(), "{wm:?}: the first show only presents");
            assert!(raise.focuses(), "{wm:?}: a later raise focuses by pid");
        }
    }

    #[test]
    fn a_window_outside_a_pair_comes_forward_for_a_file_and_not_at_its_first_show() {
        for wm in [Wm::Sway, Wm::Hyprland, Wm::Niri] {
            let raise = PairRaise::with_wm(wm);
            assert!(
                raise.used_for_window(false, false, true),
                "{wm:?}: without the raise a file handed over stays behind whatever has the keys"
            );
            assert!(!raise.focuses(), "{wm:?}: the first show stays a plain present");
            assert!(raise.focuses(), "{wm:?}: the file's show is focused");
        }
    }

    #[test]
    fn a_window_outside_a_pair_that_takes_no_files_presents_for_itself() {
        assert!(!PairRaise::with_wm(Wm::Sway).used_for_window(false, false, false));
    }

    #[test]
    fn a_window_of_a_pair_is_focused_at_its_first_show_unless_it_was_restored() {
        let raise = PairRaise::with_wm(Wm::Sway);
        assert!(raise.used_for_window(true, false, false));
        assert!(raise.focuses());
        let restored = PairRaise::with_wm(Wm::Sway);
        assert!(restored.used_for_window(true, true, true));
        assert!(!restored.focuses());
    }
}
