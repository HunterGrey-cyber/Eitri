use std::cell::RefCell;
use std::rc::Rc;

use crate::companion;
use eitri_core::wm::{HostEvent, Monitor, MonitorId, WinId};
use gtk4::prelude::*;
use gtk4::{gdk, gio, ApplicationWindow};

/// Brings a window of the pair forward after its first show, when the other window had the keys. GNOME lets a
/// window take the keys from another only with an activation token naming that other window's surface, which the
/// other window sends. sway, Hyprland and niri turn a token activation into an urgency hint, so there the window also
/// has the compositor's own tool focus it, on a worker thread (the adapter is started on first use).
#[derive(Clone)]
pub(crate) struct PairRaise {
    wm: eitri_core::compositor::Wm,
    adapter: Rc<std::cell::OnceCell<Rc<companion::wm_runner::WmRunner>>>,
    /// The next show leaves the keys where they are: a window 2 started again at a launch, whose first show must
    /// not take them from window 1. Shared by every copy, so the one the host raises with sees it.
    keep_keys_once: Rc<std::cell::Cell<bool>>,
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
        }
    }

    /// The next show only presents the window: no compositor tool focuses it.
    pub(crate) fn keep_keys_at_next_show(&self) {
        self.keep_keys_once.set(true);
    }

    /// Whether this show has the compositor's own tool focus the window: on sway, Hyprland and niri, unless the
    /// show is the one that keeps the keys where they are.
    fn focuses(&self) -> bool {
        use eitri_core::compositor::Wm;
        let kept = self.keep_keys_once.replace(false);
        !kept && matches!(self.wm, Wm::Sway | Wm::Hyprland | Wm::Niri)
    }

    pub(crate) fn raise(&self, window: &impl IsA<gtk4::Window>, token: Option<&str>) {
        if let Some(token) = token {
            window.set_startup_id(token);
        }
        window.present();
        if self.focuses() {
            let wm = self.wm;
            self.adapter
                .get_or_init(|| companion::wm_runner::WmRunner::new(wm))
                .raise_pid(std::process::id());
        }
    }
}

impl crate::gtk_host::PairRaise for PairRaise {
    fn raise(&self, window: &gtk4::Window, token: Option<&str>) {
        PairRaise::raise(self, window, token);
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
        let wm = self.wm;
        self.adapter
            .get_or_init(|| companion::wm_runner::WmRunner::new(wm))
            .move_own(how);
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

/// Reports output facts only; navigation and crossing between windows are the controller's decisions.
pub(crate) fn watch(window: &ApplicationWindow, win: WinId, post: Rc<dyn Fn(HostEvent)>) {
    let display = WidgetExt::display(window);
    let list = display.monitors();
    post(HostEvent::MonitorsChanged {
        monitors: monitors(&list),
    });
    let changed = post.clone();
    let signal = RefCell::new(Some(list.connect_items_changed(move |list, _, _, _| {
        changed(HostEvent::MonitorsChanged {
            monitors: monitors(list),
        });
    })));
    window.connect_destroy(move |_| {
        if let Some(signal) = signal.borrow_mut().take() {
            list.disconnect(signal);
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
}
