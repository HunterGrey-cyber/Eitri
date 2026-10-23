//! The window's services behind the controller's effect ops: what a terminal's start, a plugin page's
//! build, reload, visibility and stop, and a chat view's chooser call in the shell. The controller's
//! module lifecycle decides when each happens, from the layout after a change; this only carries the
//! op out on the module's own service, so no change hook in `build_ui` decides any of them.
//!
//! The keys' effects come here too: every module's "has the keys" signal (the editor's and each
//! terminal's cursor, each chat page's mode block, a plugin page's `focus` event), derived by the
//! controller from where GTK reports the keys; the literal key `send-prefix` and `send-keys` hand to
//! the module with the keys; the terminal's copy mode; and the navigation key a chat page claimed.
//!
//! A module's kill and a new instance end and begin here as well: a terminal's shell hung up, its directory read for
//! a split, an idle pane's next start directory; a chat view's tabs closed, its first tab opened or broken out of the
//! base view, and the views the workspaces still hold told to the panel.
//!
//! Given to the GTK host once, before the startup layout's first batch (`GtkHost::set_services`).
//! An op this shell does not carry out yet is answered with the host's refusal.
//!
//! [`LuaKernel`] is what the host's `Lua` ops run on: the window's `init.lua`, loaded at the start's
//! kernel phase, and the events the start emits.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use eitri_core::layout::ModuleId;
use eitri_core::tabs::ViewId;
use eitri_core::wm::{ChatOp, EditorCall, HostEvent, HostSink, LuaOp, PluginOp, TermEvent, TermOp, WinId};
use gtk4::prelude::*;
use neovide_editor::NeovideEditorPane;

use crate::agent_panel::AgentPanelHandle;
use crate::gtk_host::instance_pool::InstancePool;
use crate::gtk_host::{LuaRunner, ServiceSet, NOT_CARRIED_OUT};
use crate::lua::LuaEngine;
use crate::plugin_panels::PluginPanels;
use crate::terminal::registry::Pane;
use crate::terminal::TerminalPane;
use crate::webview_crash_guard::WebViewCrashGuard;

/// Every Lua panel's module, its view as `init.lua` registered it, its URL and its crash guard: what
/// a reload loads again and what a kill ends. A plugin page built after startup has its own view in
/// `PluginPanels`, which is asked first.
pub(crate) type LuaPages = Rc<Vec<(ModuleId, gtk4::Widget, String, Rc<RefCell<WebViewCrashGuard>>)>>;

/// What a terminal's start needs of a pane, so the decision can be tested without a display.
pub(crate) trait ShellPane: Pane + Clone {
    /// A shell is running in the pane.
    fn running(&self) -> bool;
    /// Wants a shell: spawned now if the pane has been laid out, else at its first allocation.
    fn start(&self);
}

impl ShellPane for TerminalPane {
    fn running(&self) -> bool {
        self.shell_pid().is_some()
    }
    fn start(&self) {
        TerminalPane::start(self);
    }
}

/// Starts `module`'s pane unless a shell already runs in it; whether it started one. The registry is
/// released before the start, which may spawn and report from inside.
pub(crate) fn start_pane<P: ShellPane>(
    terminals: &RefCell<InstancePool<P>>,
    module: &ModuleId,
) -> Result<bool, String> {
    let pane = match terminals.try_borrow() {
        Ok(terminals) => terminals.get(module).cloned(),
        Err(_) => {
            return Err(format!(
                "{module}: the terminal registry was borrowed; no shell was started"
            ))
        }
    };
    let Some(pane) = pane else {
        return Err(format!("{module} has no live pane"));
    };
    if pane.running() {
        return Ok(false);
    }
    pane.start();
    Ok(true)
}

/// Takes `module`'s idle pane back live, its next shell to start in `dir`: a new instance under a number a kill left
/// idle asks no create, so this is where its directory comes from. A live pane only gets the directory.
pub(crate) fn revive_pane(
    terminals: &RefCell<InstancePool<TerminalPane>>,
    module: &ModuleId,
    dir: PathBuf,
) -> Result<(), String> {
    let pane = match terminals.try_borrow_mut() {
        Ok(mut terminals) => terminals.revive(module),
        Err(_) => return Err(format!("{module}: the terminal registry was borrowed")),
    };
    let pane = pane.ok_or_else(|| format!("{module} has no pane to take back"))?;
    pane.set_start_dir(dir);
    Ok(())
}

/// The chat views whose removal is their caller's own while their tabs close or open: the panel's word that such a
/// view holds no tab is then not a request to remove it.
pub(crate) type ChatBusy = Rc<RefCell<BTreeSet<ViewId>>>;

/// The window's services, as the host's ops reach them.
pub(crate) struct Services {
    /// `None` in a window with no editor, where an editor op has nothing to reach.
    pub(crate) editor: Option<Rc<NeovideEditorPane>>,
    pub(crate) terminals: Rc<RefCell<InstancePool<TerminalPane>>>,
    pub(crate) plugin_panels: Rc<PluginPanels>,
    pub(crate) agent: AgentPanelHandle,
    pub(crate) lua_pages: LuaPages,
    pub(crate) chat_busy: ChatBusy,
    /// Where a terminal's shell starts when its directory cannot be read.
    pub(crate) project_dir: PathBuf,
    /// Where a directory read off the toolkit's thread is reported; weak, as every host's sink is.
    pub(crate) sink: Weak<dyn HostSink>,
}

impl Services {
    /// `module`'s live pane, with the registry released before anything is asked of it.
    fn pane(&self, module: &ModuleId) -> Result<TerminalPane, String> {
        let pane = match self.terminals.try_borrow() {
            Ok(terminals) => terminals.get(module).cloned(),
            Err(_) => return Err(format!("{module}: the terminal registry was borrowed")),
        };
        pane.ok_or_else(|| format!("{module} has no live pane"))
    }

    /// Reads the directory `module`'s shell is in now, on a worker thread (a slow or hung filesystem must never
    /// stall the window), and reports it as [`TermEvent::Cwd`]: the project root, with one log line, where it cannot
    /// be read or no shell runs; a refusal when the terminal was killed while it was read, since its number may
    /// name a new terminal by then.
    fn read_cwd(&self, win: WinId, module: &ModuleId, read: u64) -> Result<(), String> {
        let pane = self.pane(module)?;
        let captured = crate::ShellStart::of(pane.generation(), pane.shell_pid());
        let terminals = Rc::downgrade(&self.terminals);
        let root = self.project_dir.clone();
        let sink = self.sink.clone();
        let module = module.clone();
        gtk4::glib::spawn_future_local(async move {
            let found = match captured.pid {
                // The stat that tells a removed directory runs here too, off the GTK thread.
                Some(pid) => gtk4::gio::spawn_blocking(move || {
                    crate::terminal::usable_dir(std::fs::read_link(format!("/proc/{pid}/cwd")))
                })
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("the reading thread panicked"))),
                None => Err(std::io::Error::other("its shell is not running")),
            };
            let now = terminals.upgrade().and_then(|terminals| {
                terminals.try_borrow().ok().and_then(|terminals| {
                    terminals
                        .get(&module)
                        .map(|pane| crate::ShellStart::of(pane.generation(), pane.shell_pid()))
                })
            });
            let dir = if captured.still(now) {
                let (dir, note) = crate::terminal::start_dir(found, &root);
                if let Some(note) = note {
                    println!("[terminal] {note}");
                }
                Ok(dir)
            } else {
                Err(format!(
                    "{module} closed while its directory was read; no terminal was opened"
                ))
            };
            if let Some(sink) = sink.upgrade() {
                sink.post(HostEvent::Terminal(TermEvent::Cwd { win, module, read, dir }));
            }
        });
        Ok(())
    }

    /// Runs `f` on the panel with `view` marked busy, so the view's tabs closing or opening is not taken for the
    /// view holding none.
    fn busy<T>(&self, view: ViewId, f: impl FnOnce() -> T) -> T {
        self.chat_busy.borrow_mut().insert(view);
        let out = f();
        self.chat_busy.borrow_mut().remove(&view);
        out
    }
}

impl ServiceSet for Services {
    fn editor(&self, op: &EditorCall) -> Result<(), String> {
        match op {
            // Neovide draws the cursor, or none, and tells nvim `FocusGained`/`FocusLost`.
            EditorCall::SetFocused { focused, .. } => {
                if let Some(editor) = &self.editor {
                    editor.set_focused(*focused);
                }
                Ok(())
            }
            EditorCall::SendKeys { keys, .. } => {
                if let Some(editor) = &self.editor {
                    editor.send_keys(keys);
                }
                Ok(())
            }
            EditorCall::Quit { .. }
            | EditorCall::End { .. }
            | EditorCall::Shutdown { .. }
            | EditorCall::SetScale { .. }
            | EditorCall::Reveal { .. }
            | EditorCall::Restart { .. } => Err(NOT_CARRIED_OUT.into()),
        }
    }

    fn terminal(&self, op: &TermOp) -> Result<(), String> {
        match op {
            TermOp::Start { module, .. } => start_pane(&self.terminals, module).map(drop),
            TermOp::SetFocused { module, focused, .. } => {
                self.pane(module)?.set_focused(*focused);
                Ok(())
            }
            TermOp::CopyMode { module, up, .. } => {
                self.pane(module)?.enter_copy_mode(*up);
                Ok(())
            }
            // The terminal's engine encodes the key, or the key has no encoding there: said, and nothing sent.
            TermOp::LiteralKey { module, key, .. } => {
                let pane = self.pane(module)?;
                match crate::terminal::keys::literal(key) {
                    Some(inputs) => inputs.into_iter().for_each(|input| pane.send(input)),
                    None => println!("[prefix] {key}: the terminal takes C-<letter> or one character"),
                }
                Ok(())
            }
            // The shell is hung up and the pane forgets it, so its next start is a fresh shell on a blank pane.
            TermOp::HangUp { module, .. } => {
                self.pane(module)?.close();
                Ok(())
            }
            TermOp::ReadCwd { win, module, read } => self.read_cwd(*win, module, *read),
            TermOp::SetStartDir { module, dir, .. } => revive_pane(&self.terminals, module, dir.clone()),
            TermOp::SendKeys { .. } | TermOp::Detach { .. } | TermOp::ShutdownAll { .. } => Err(NOT_CARRIED_OUT.into()),
        }
    }

    fn plugin(&self, op: &PluginOp) -> Result<(), String> {
        match op {
            // Only a page that waits for its first show is built here; `build_page` builds once, so a page the
            // startup or a grab already built is left as it is.
            PluginOp::Build { module, .. } => {
                if self.plugin_panels.starts_hidden(module) {
                    self.plugin_panels.build_page(module, &self.plugin_panels.theme());
                }
                Ok(())
            }
            PluginOp::Reload {
                module, reset_guard, ..
            } => {
                let Some((_, widget, url, crash_guard)) = self.lua_pages.iter().find(|(m, _, _, _)| m == module) else {
                    return Err(format!("{module} is not a Lua panel"));
                };
                println!("[modules] {module}: shown again after a kill, starting it fresh");
                let webview = self
                    .plugin_panels
                    .webview(module)
                    .or_else(|| widget.downcast_ref::<webkit6::WebView>().cloned());
                if let Some(webview) = webview {
                    use webkit6::prelude::*;
                    webview.load_uri(url);
                }
                // The manual recovery a give-up message points the user at (`prefix x`, then this panel's own
                // key): the guard is armed again, so a later, unrelated crash is caught fresh rather than
                // dropped as already given up.
                if *reset_guard {
                    crash_guard.borrow_mut().reset();
                }
                Ok(())
            }
            PluginOp::OnScreen { module, on, .. } => {
                self.plugin_panels.set_on_screen(module, *on);
                Ok(())
            }
            // A no-op for a Lua panel that is not a plugin.
            PluginOp::Terminate { module, .. } => {
                self.plugin_panels.stop(module);
                Ok(())
            }
            // A page hears this as its `focus` event; a Lua page that is no plugin's has no page to tell.
            PluginOp::HasKeys { module, has_keys, .. } => {
                self.plugin_panels.set_has_keys(module, *has_keys);
                Ok(())
            }
        }
    }

    fn chat(&self, op: &ChatOp) -> Result<(), String> {
        match op {
            ChatOp::OpenChooser { view, .. } => {
                println!(
                    "[modules] {}: shown again after a kill, starting it fresh",
                    view.module()
                );
                self.agent.open_chooser(*view);
                Ok(())
            }
            // Into a chat view: when asked, the oldest card among that view's tabs (the base view switches to the tab
            // holding it first), else the view's own arrival.
            ChatOp::Arrive { view, card_first, .. } => {
                if !(*card_first && self.agent.focus_oldest_card(*view)) {
                    self.agent.arrive(*view);
                }
                Ok(())
            }
            // Each chat view's page is told for itself, so exactly one is told it has the keys.
            ChatOp::PaneFocused { view, focused, .. } => {
                self.agent.set_pane_focused(*view, *focused);
                Ok(())
            }
            ChatOp::NavKey { view, direction, .. } => {
                self.agent.nav_key(*view, *direction);
                Ok(())
            }
            ChatOp::LiteralKey { view, key, .. } => {
                self.agent.literal_key(*view, key);
                Ok(())
            }
            // The typing window that paces the panel's stream only counts while the editor holds the keys.
            ChatOp::EditorHasKeys { has_keys, .. } => {
                self.agent.set_editor_has_keys(*has_keys);
                Ok(())
            }
            // A float left these views. Each page is told it is on screen once the change that moved the float is
            // over, never inside it: the panel's attention hook reads the layout and the grid again, and checks then
            // that the page is mapped and clear, so a view that left the screen with its float sees nothing.
            ChatOp::TellUncovered { views, .. } => {
                let agent = self.agent.clone();
                let views = views.clone();
                gtk4::glib::idle_add_local_once(move || {
                    for view in views {
                        agent.page_shown(view);
                    }
                });
                Ok(())
            }
            // A kill's: the base view's own tabs, or an extra view's one tab. A tab shown in another view is that
            // view's.
            ChatOp::CloseTabs { view, .. } => {
                let closed = self.busy(*view, || self.agent.close_view_tabs(*view))?;
                println!("[modules] {}: {closed} session tab(s) closed", view.module());
                Ok(())
            }
            // A new chat view's first tab: a new empty tab of the window's default agent, or the base view's active
            // tab brought out of it.
            ChatOp::OpenTab { view, .. } => self.busy(*view, || self.agent.open_in(*view)).map(drop),
            ChatOp::BreakOut { view, .. } => self.busy(*view, || self.agent.break_out(*view)).map(drop),
            // A restore still to come must not bring a tab back into a view that has left: its page is idle, so the
            // tab would show nowhere.
            ChatOp::LayoutViews { views, .. } => {
                self.agent.set_layout_views(views.clone());
                Ok(())
            }
            ChatOp::PageShown { .. }
            | ChatOp::JoinBack { .. }
            | ChatOp::OpenKeymap { .. }
            | ChatOp::CommandLine { .. }
            | ChatOp::Reload { .. }
            | ChatOp::SetCoverage { .. }
            | ChatOp::ShutdownAll { .. } => Err(NOT_CARRIED_OUT.into()),
        }
    }
}

/// The window's Lua kernel, as the start's `Lua` ops reach it.
pub(crate) struct LuaKernel {
    pub(crate) engine: Rc<LuaEngine>,
    /// The `init.lua` the kernel phase loads.
    pub(crate) init_file: PathBuf,
}

impl LuaRunner for LuaKernel {
    fn run(&self, op: &LuaOp) -> Result<(), String> {
        match op {
            // A file that fails ends the process, naming it: `init.lua`'s refusals are startup failures.
            LuaOp::LoadInit => {
                self.engine.load_init_file(&self.init_file);
                Ok(())
            }
            LuaOp::Emit { event } => {
                self.engine.emit(event);
                Ok(())
            }
            LuaOp::Invoke { .. } => Err(NOT_CARRIED_OUT.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::ModuleKind;
    use std::cell::Cell;

    /// A pane whose shell runs from its first start on; counts its starts and closes.
    #[derive(Clone, Default)]
    struct FakePane {
        starts: Rc<Cell<u32>>,
        running: Rc<Cell<bool>>,
    }

    impl Pane for FakePane {
        fn close(&self) {
            self.running.set(false);
        }
    }

    impl ShellPane for FakePane {
        fn running(&self) -> bool {
            self.running.get()
        }
        fn start(&self) {
            self.starts.set(self.starts.get() + 1);
            self.running.set(true);
        }
    }

    #[test]
    fn a_start_op_starts_a_shown_pane_once() {
        let terminals = RefCell::new(InstancePool::new());
        let id = ModuleId::instance(ModuleKind::Terminal, 2).unwrap();
        let (pane, _) = terminals.borrow_mut().take_or_make(&id, FakePane::default);
        assert_eq!(start_pane(&terminals, &id), Ok(true));
        assert_eq!(
            start_pane(&terminals, &id),
            Ok(false),
            "a second start while its shell runs starts nothing"
        );
        assert_eq!(pane.starts.get(), 1);
        // A killed pane's shell is gone: the next start is a fresh one.
        pane.close();
        assert_eq!(start_pane(&terminals, &id), Ok(true));
        assert_eq!(pane.starts.get(), 2);
        // An idle pane, retired with its instance, is no one's to start.
        terminals.borrow_mut().retire(&id);
        assert!(start_pane(&terminals, &id).is_err());
        assert_eq!(pane.starts.get(), 2);
        // Nothing is started while something else holds the registry.
        let (other, _) = terminals
            .borrow_mut()
            .take_or_make(&ModuleId::terminal(), FakePane::default);
        let held = terminals.borrow_mut();
        assert!(start_pane(&terminals, &ModuleId::terminal()).is_err());
        drop(held);
        assert_eq!(other.starts.get(), 0);
    }
}
