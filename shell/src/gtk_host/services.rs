//! The window's services behind the controller's effect ops: what a terminal's start, a plugin page's
//! build, reload, visibility and stop, and a chat view's chooser call in the shell. The controller's
//! module lifecycle decides when each happens, from the layout after a change; this only carries the
//! op out on the module's own service, so no change hook in `build_ui` decides any of them.
//!
//! Given to the GTK host once, before the startup layout's first batch (`GtkHost::set_services`).
//! An op this shell does not carry out yet is answered with the host's refusal.
//!
//! [`LuaKernel`] is what the host's `Lua` ops run on: the window's `init.lua`, loaded at the start's
//! kernel phase, and the events the start emits.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use eitri_core::layout::ModuleId;
use eitri_core::wm::{ChatOp, LuaOp, PluginOp, TermOp};
use gtk4::prelude::*;

use crate::agent_panel::AgentPanelHandle;
use crate::gtk_host::{LuaRunner, ServiceSet, NOT_CARRIED_OUT};
use crate::lua::LuaEngine;
use crate::plugin_panels::PluginPanels;
use crate::terminal::registry::{Pane, Terminals};
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
pub(crate) fn start_pane<P: ShellPane>(terminals: &RefCell<Terminals<P>>, module: &ModuleId) -> Result<bool, String> {
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

/// The window's services, as the host's ops reach them.
pub(crate) struct Services {
    pub(crate) terminals: Rc<RefCell<Terminals<TerminalPane>>>,
    pub(crate) plugin_panels: Rc<PluginPanels>,
    pub(crate) agent: AgentPanelHandle,
    pub(crate) lua_pages: LuaPages,
}

impl ServiceSet for Services {
    fn terminal(&self, op: &TermOp) -> Result<(), String> {
        match op {
            TermOp::Start { module, .. } => start_pane(&self.terminals, module).map(drop),
            TermOp::SetFocused { .. }
            | TermOp::HangUp { .. }
            | TermOp::CopyMode { .. }
            | TermOp::SendKeys { .. }
            | TermOp::Detach { .. }
            | TermOp::ShutdownAll { .. } => Err(NOT_CARRIED_OUT.into()),
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
            // Into a chat view: the oldest card among that view's tabs (the base view switches to the tab holding it
            // first), else the view's own arrival.
            ChatOp::Arrive { view, .. } => {
                if !self.agent.focus_oldest_card(*view) {
                    self.agent.arrive(*view);
                }
                Ok(())
            }
            ChatOp::PaneFocused { .. }
            | ChatOp::PageShown { .. }
            | ChatOp::NavKey { .. }
            | ChatOp::CloseTabs { .. }
            | ChatOp::BreakOut { .. }
            | ChatOp::JoinBack { .. }
            | ChatOp::OpenKeymap { .. }
            | ChatOp::CommandLine { .. }
            | ChatOp::Reload { .. }
            | ChatOp::SetCoverage { .. }
            | ChatOp::TellUncovered { .. }
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
        let terminals = RefCell::new(Terminals::new());
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
