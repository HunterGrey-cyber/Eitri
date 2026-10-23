//! Window identities and native window descriptions shared with the host, and the controller's own record of
//! each window.

use super::chrome::ChromeModel;
use super::focus::FocusState;
use super::grid::{floats_need_restack, shown_floats};
use super::host::{Batch, FocusTo, HostOp, RestackMove, Scheduler, TimerToken};
use super::keys::KeyState;
use super::module::{Lifecycle, ModuleState};
use super::persist::{Fired, Saver};
use crate::layout::{Layout, ModuleId, Rect as LayoutRect, Size, Workspaces};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// A stable identity for one application window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WinId(pub u32);

/// A stable identity for a linked process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub u32);

impl WinId {
    /// The initial main window.
    pub const MAIN: Self = Self(1);
}

/// Which end of a linked pair a window occupies.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkSide {
    /// The first end.
    First,
    /// The second end.
    Second,
}

/// The purpose of a native window.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowRole {
    /// The main workspace.
    Main,
    /// A companion chat window.
    Companion,
    /// One end of a linked pair.
    Linked { side: LinkSide },
}

/// How the window reaches the editor.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorPresence {
    /// Owns the editor process.
    Owned,
    /// Displays an editor owned elsewhere.
    Proxy,
    /// Has no editor.
    None,
}

/// The initial description of a window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    /// The window role.
    pub role: WindowRole,
    /// The editor relationship.
    pub presence: EditorPresence,
    /// The native title.
    pub title: String,
}

/// What the shell hands the controller when it opens a window: the live layout its grid shares, every workspace
/// beside it, and the saver of its state file (`None` for a window that keeps no layout file).
pub struct WindowParts {
    /// The current workspace's layout, the one on screen.
    pub layout: Rc<RefCell<Layout>>,
    /// Every workspace of the window; the current one's layout is `layout`.
    pub workspaces: Rc<RefCell<Workspaces>>,
    /// Writes the window's layouts to the project's state file.
    pub saver: Option<Saver>,
}

/// Where a window's timers come from: the controller's scheduler, and the counter every window's tokens are
/// taken from, so no two timers of the process share one.
#[derive(Clone)]
pub struct Timers {
    pub(super) scheduler: Rc<dyn Scheduler>,
    pub(super) next: Rc<Cell<u64>>,
}

impl Timers {
    fn token(&self) -> TimerToken {
        let token = TimerToken(self.next.get());
        self.next.set(token.0 + 1);
        token
    }
}

/// The controller's record of one window. The shell's remaining arms still write the layout and the workspaces,
/// so the controller reads them with `try_borrow` and never waits for a shell closure that holds them.
pub struct Window {
    pub(super) spec: WindowSpec,
    pub(super) lifecycle: Lifecycle,
    pub(super) layout: Rc<RefCell<Layout>>,
    pub(super) workspaces: Rc<RefCell<Workspaces>>,
    pub(super) saver: Option<Saver>,
    /// The save's debounce timer, while one is armed.
    pub(super) save_timer: Option<TimerToken>,
    timers: Timers,
    /// The hosted modules the grid shows, as the last layout batch (or the shell's last change) left them.
    pub(super) mapped: BTreeSet<ModuleId>,
    /// The floats the grid's float band holds, as its last restack put them.
    pub(super) float_band: BTreeSet<ModuleId>,
    /// The size of the window's last allocation; `None` before its first.
    pub(super) allocated: Option<Size>,
    /// A shell verb the runner handed over and whose re-read has not come yet: whether the selection is to be
    /// put back on the module with the keys once its arm has run.
    pub(super) after_verb: Option<bool>,
    /// Who holds the keys as the host reports it, and what follows from it.
    pub(super) focus: FocusState,
    /// The window's prefix and the timer of its digit wait.
    pub(super) keys: KeyState,
    /// What the window's chrome shows, as the controller decided it.
    pub(super) chrome: ChromeModel,
    /// Bumped by every change of `chrome`, so the shell can tell an announced model from one it drew.
    pub(super) chrome_rev: u64,
    #[cfg(test)]
    notes: Vec<String>,
}

impl Window {
    pub fn new(spec: WindowSpec, parts: WindowParts, timers: Timers) -> Window {
        Window {
            lifecycle: Lifecycle::default(),
            spec,
            layout: parts.layout,
            workspaces: parts.workspaces,
            saver: parts.saver,
            save_timer: None,
            timers,
            mapped: BTreeSet::new(),
            float_band: BTreeSet::new(),
            allocated: None,
            after_verb: None,
            focus: FocusState::default(),
            keys: KeyState::default(),
            chrome: ChromeModel::default(),
            chrome_rev: 0,
            #[cfg(test)]
            notes: vec![],
        }
    }

    /// Prints a verb's line on stdout, as the shell's arms print theirs.
    pub(super) fn say(&mut self, line: String) {
        println!("{line}");
        #[cfg(test)]
        self.notes.push(line);
    }

    /// Prints a refusal or a mistake on stderr.
    pub(super) fn warn(&mut self, line: String) {
        eprintln!("{line}");
        #[cfg(test)]
        self.notes.push(line);
    }

    /// The lines the window's verbs printed, for tests.
    #[cfg(test)]
    pub(super) fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The hosted modules the layout puts on screen now, tiles in tree order then floats bottom first: the
    /// hosts the grid makes visible. A module only declared, retired or away has no host here.
    fn hosted_visible(&self, layout: &Layout) -> Vec<ModuleId> {
        layout
            .visible_modules()
            .into_iter()
            .filter(|id| {
                self.lifecycle.state(id).is_some_and(|state| {
                    !matches!(state, ModuleState::Declared | ModuleState::Retired | ModuleState::Away)
                })
            })
            .collect()
    }

    /// The layout ops of a change, in the order that keeps the keys off a host leaving the screen: each hosted
    /// module the layout shows now and the grid does not is mapped (tree order), then `focus` gives the keys
    /// with its grab's tail, then what the grid shows and the layout no longer does is unmapped, the float band
    /// is restacked when its floats changed, and the grid is allocated. Both records follow what the ops do.
    pub fn layout_ops(&mut self, win: WinId, focus: Option<(ModuleId, Vec<HostOp>)>, batch: &mut Batch) {
        let layout = self.layout.clone();
        let Ok(layout) = layout.try_borrow() else {
            self.warn("[wm] BUG: the layout was borrowed when its change was laid out; nothing moved".into());
            return;
        };
        let visible = self.hosted_visible(&layout);
        for module in visible.iter().filter(|m| !self.mapped.contains(*m)) {
            batch.push_layout(HostOp::Map {
                win,
                module: module.clone(),
            });
        }
        if let Some((module, on_grab)) = focus {
            self.focus.ask(&module, batch);
            batch.push_layout(HostOp::Focus {
                win,
                to: FocusTo::Module(module),
                on_grab,
            });
        }
        for module in self.mapped.iter().filter(|m| !visible.contains(*m)) {
            batch.push_layout(HostOp::Unmap {
                win,
                module: module.clone(),
            });
        }
        let floats = shown_floats(&layout);
        if floats_need_restack(&self.float_band, &floats) {
            batch.push_layout(HostOp::Restack {
                win,
                moves: restack_order(&floats),
            });
            self.float_band = floats.into_iter().collect();
        }
        batch.push_layout(HostOp::Allocate { win });
        self.mapped = visible.into_iter().collect();
    }

    /// A change the controller made to the layout: its layout ops, then as effects the modules' lifecycle
    /// effects and the shell's remaining change hooks, and the save's debounce armed when the arrangement
    /// differs from what was last written.
    pub fn changed_by_controller(&mut self, win: WinId, batch: &mut Batch) {
        self.changed_with_focus(win, None, batch);
    }

    /// [`Window::changed_by_controller`] for a change that also gives the keys: `focus` gets them, with its
    /// grab's tail, between the maps and the unmaps. The change hooks run once however many changes the batch
    /// holds, since they read the layout as the whole batch leaves it.
    pub fn changed_with_focus(&mut self, win: WinId, focus: Option<(ModuleId, Vec<HostOp>)>, batch: &mut Batch) {
        self.layout_ops(win, focus, batch);
        let layout = self.layout.clone();
        match layout.try_borrow() {
            Ok(layout) => self.lifecycle.after_change(win, &layout, batch),
            Err(_) => self.warn("[wm] BUG: the layout was borrowed after a change; no lifecycle effect".into()),
        }
        let hooks = HostOp::ChromeHooks { win };
        if !batch.ops().any(|op| *op == hooks) {
            batch.push_effect(hooks);
        }
        self.saver_changed();
    }

    /// The float band put in the layout's z-order now, as one restack.
    pub(super) fn restack(&mut self, win: WinId, batch: &mut Batch) {
        let Ok(floats) = self.layout.try_borrow().map(|layout| shown_floats(&layout)) else {
            self.warn("[wm] BUG: the layout was borrowed when the floats were restacked".into());
            return;
        };
        batch.push_layout(HostOp::Restack {
            win,
            moves: restack_order(&floats),
        });
        self.float_band = floats.into_iter().collect();
    }

    /// Arms an idle below painting on the window's scheduler, with a token no other timer of the process has.
    pub(super) fn arm_idle(&mut self) -> TimerToken {
        let token = self.timers.token();
        self.timers.scheduler.idle(token);
        token
    }

    /// Arms a timer after `delay` on the window's scheduler, with a token no other timer of the process has.
    pub(super) fn arm_after(&mut self, delay: Duration) -> TimerToken {
        let token = self.timers.token();
        self.timers.scheduler.after(delay, token);
        token
    }

    /// Cancels a timer this window armed.
    pub(super) fn cancel_timer(&self, token: TimerToken) {
        self.timers.scheduler.cancel(token);
    }

    /// The scheduler's clock.
    pub(super) fn now(&self) -> Instant {
        self.timers.scheduler.now()
    }

    /// The chrome changed: its revision bumped and announced with the new revision.
    pub(super) fn chrome_changed(&mut self, win: WinId, batch: &mut Batch) {
        self.chrome_rev += 1;
        batch.push_effect(HostOp::Chrome {
            win,
            rev: self.chrome_rev,
        });
    }

    /// What the shell's own apply left on screen, taken as the grid's state: the visible hosted modules and the
    /// floats it shows. A shell arm moves hosts without telling the controller which.
    pub(super) fn reset_mapped(&mut self) {
        let layout = self.layout.clone();
        let Ok(layout) = layout.try_borrow() else {
            return;
        };
        self.mapped = self.hosted_visible(&layout).into_iter().collect();
        self.float_band = shown_floats(&layout).into_iter().collect();
    }

    /// Puts the selection back on the module with the keys; `true` if a container was selected. A layout
    /// borrowed right now leaves the selection as it is.
    pub(super) fn clear_selection(&mut self) -> bool {
        self.layout
            .try_borrow_mut()
            .is_ok_and(|mut layout| layout.clear_selection())
    }

    /// The layout or the workspaces changed: the saver says whether the save's debounce is to be armed.
    pub(super) fn saver_changed(&mut self) {
        let now = self.timers.scheduler.now();
        let Some(saver) = self.saver.as_mut() else {
            return;
        };
        saver.collect(self.workspaces.try_borrow_mut().ok().as_deref_mut());
        let arm = match (self.layout.try_borrow(), self.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => saver.changed(now, &layout, &workspaces),
            _ => {
                saver.changed_unread(now);
                None
            }
        };
        if let Some(delay) = arm {
            self.arm_save(delay);
        }
    }

    /// The save's debounce fired: armed again for what is left, or written.
    pub(super) fn save_timer_fired(&mut self) {
        self.save_timer = None;
        let now = self.timers.scheduler.now();
        let Some(saver) = self.saver.as_mut() else {
            return;
        };
        saver.collect(self.workspaces.try_borrow_mut().ok().as_deref_mut());
        let rearm = match (self.layout.try_borrow(), self.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => match saver.timer_fired(now, &layout, &workspaces) {
                Fired::Rearm(left) => Some(left),
                Fired::Saved | Fired::NothingToSave => None,
            },
            _ => saver.timer_fired_unread(now),
        };
        if let Some(left) = rearm {
            self.arm_save(left);
        }
    }

    fn arm_save(&mut self, delay: Duration) {
        let token = self.timers.token();
        if let Some(old) = self.save_timer.replace(token) {
            self.timers.scheduler.cancel(old);
        }
        self.timers.scheduler.after(delay, token);
    }

    pub fn role(&self) -> &WindowRole {
        &self.spec.role
    }

    pub fn presence(&self) -> &EditorPresence {
        &self.spec.presence
    }

    /// How the shell described the window when it opened it.
    pub fn spec(&self) -> &WindowSpec {
        &self.spec
    }
}

/// The float band's order as restack moves: each shown float after the one below it, the lowest first. Floats
/// only: the editor never floats, and a move naming it would reorder the editor's widget among the grid's.
fn restack_order(floats: &[ModuleId]) -> Vec<RestackMove> {
    let mut after = None;
    floats
        .iter()
        .map(|module| {
            debug_assert!(*module != ModuleId::editor(), "the editor never floats");
            let step = RestackMove {
                module: module.clone(),
                after: after.clone(),
            };
            after = Some(module.clone());
            step
        })
        .collect()
}

/// The independently controlled window presentation flags.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowMode {
    /// Whether surrounding chrome is suppressed.
    pub immersive: bool,
    /// Whether the window fills its output.
    pub fullscreen: bool,
    /// Whether the top bar is temporarily exposed.
    pub top_bar_revealed: bool,
}

/// An opaque output identity supplied by the host.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorId(pub String);

/// One output in desktop layout coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// The stable output identity.
    pub id: MonitorId,
    /// The desktop rectangle.
    pub rect: LayoutRect,
    /// The output scale.
    pub scale: f64,
    /// The displayed output name.
    pub name: String,
}

/// The current content area and scale of a window.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMetrics {
    /// The available content size.
    pub size: Size,
    /// The native scale.
    pub scale: f64,
}

/// An opaque native token authorizing presentation.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivationToken(pub String);
