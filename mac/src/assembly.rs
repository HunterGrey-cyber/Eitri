//! Everything the companion window does to build the agent panel and wire it to its editor, without a
//! toolkit: the Lua kernel and `init.lua`, the window's configuration, the panel handle and its hooks, the
//! `?` overlay's rows, and the prefix's command dispatch. What only the AppKit half can do (move the keys,
//! show a toast, lay the panes out) is the [`HostActions`] it supplies.
//!
//! Neither the page nor the first document is started here: those follow the web view's own start-up
//! messages, which the AppKit half owns.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use eitri_core::agent_backend::BackendKind;
use eitri_core::keymap::companion::{self as companion_prefix, CompanionVerb};
use eitri_core::keymap::prefix::{literal_for, PrefixCommand};
use eitri_core::keymap::{Action, Keymap, TabAction, TextChange};
use eitri_core::layout::{ModuleKeys, ModuleKind};
use eitri_core::lua::kernel::{refuse_panels, Kernel};
use eitri_core::tabs::{plan, TabVerb};
use eitri_core::theme::ThemeTokens;
use eitri_panel::agent_panel::{AgentPanelHandle, HintInbound, PanelInit};
use eitri_panel::keys_help::{HelpScope, KeysHelp};
use eitri_panel::panel_document::PanelDocument;
use eitri_panel::panel_page::{PanelHost, PanelPage};
use eitri_panel::window_config::{self, WindowConfig};

use crate::editor::{Editor, EditorEvent};
use crate::keys::{neighbour, Keys, Pane};

/// What the window says to an action that has no meaning in it (a split, a Lua panel, a HINT). The
/// companion window's own text names the wrong window.
pub const NOT_IN_THIS_WINDOW: &str = "not in this window";

/// What only the AppKit half can do.
pub trait HostActions {
    /// Move the keys within the window.
    fn focus(&self, pane: Pane);
    fn toast(&self, text: &str);
    /// Bring the window forward and give the editor the keys: something happened there the user must see
    /// (a draft to edit, a file the review opened).
    fn show_editor(&self);
    /// The prefix's zoom: the panel is hidden (`true`) or shown again. The host relayouts and, when it
    /// hides the panel, moves the keys to the editor.
    fn set_panel_hidden(&self, hidden: bool);
}

/// Text size steps, as the one-pane companion case of the full window's rule: the panel alone is scaled
/// (Neovide owns the editor's size), by a tenth per step, within one half to three, rounded to two decimals.
fn stepped(scale: f32, change: TextChange) -> f32 {
    let next = match change {
        TextChange::Larger => scale + 0.1,
        TextChange::Smaller => scale - 0.1,
        TextChange::Reset => 1.0,
    };
    ((next.clamp(0.5, 3.0)) * 100.0).round() / 100.0
}

/// The state the panel's hooks and the prefix's commands share. Hooks keep it alive through the panel
/// handle (the handle holds them), as the companion window's closures do: it lives for the process.
struct Shared {
    panel: AgentPanelHandle,
    editor: Rc<Editor>,
    actions: Rc<dyn HostActions>,
    keymap: Rc<Keymap>,
    keys_help: Rc<KeysHelp>,
    base_font_px: f32,
    panel_hidden: Cell<bool>,
    panel_scale: Cell<f32>,
}

impl Shared {
    fn panel_font_size_px(&self) -> f32 {
        self.base_font_px * self.panel_scale.get()
    }

    /// The keys go to the panel. A hidden panel is shown first: tmux's `select-pane` unzooms a zoomed
    /// window, and a tab verb that takes the keys means "show the chat".
    fn give_panel_keys(&self) {
        if self.panel_hidden.replace(false) {
            self.actions.set_panel_hidden(false);
        }
        self.actions.focus(Pane::Panel);
    }

    /// The prefix's tab verbs, as the companion window runs them: the chat takes the keys first for the
    /// verbs that read them, then the verb goes to the panel.
    fn run_tab(&self, action: TabAction, pane: Pane) {
        let keys_in = match pane {
            Pane::Editor => ModuleKind::Editor,
            Pane::Panel => ModuleKind::Agent,
        };
        // No terminal exists here, so the count is zero: `n`/`p` step the chat's tabs.
        let plan = plan(action, Some(keys_in), 0);
        if plan.takes_the_keys {
            self.give_panel_keys();
        }
        let panel = &self.panel;
        let switched = match plan.verb {
            TabVerb::New => {
                panel.new_tab();
                panel.enter_input();
                true
            }
            TabVerb::Step(delta) => panel.step(delta),
            TabVerb::Last => panel.select_last(),
            TabVerb::Select(n) => panel.select_number(n),
            TabVerb::Rename => {
                panel.begin_rename();
                true
            }
            TabVerb::Close => {
                panel.confirm_close();
                true
            }
            TabVerb::CloseOthers => {
                panel.confirm_close_others();
                true
            }
            TabVerb::Choose => {
                panel.open_chooser();
                true
            }
            TabVerb::Info => {
                panel.open_detail();
                true
            }
            TabVerb::Flash => false,
            TabVerb::Nothing => return,
        };
        if !switched {
            // tmux: "can't find window".
            self.actions.toast("no such tab");
        }
    }

    fn run_prefix(&self, command: PrefixCommand, pane: Pane) {
        let action = match command {
            PrefixCommand::Run(action) => action,
            // A module key after a split key: there is nothing to place.
            PrefixCommand::Place { module, axis } => {
                println!("[mac] prefix: place {module} {axis:?} refused");
                self.actions.toast(NOT_IN_THIS_WINDOW);
                return;
            }
        };
        if action == Action::Zoom {
            let hidden = !self.panel_hidden.get();
            self.panel_hidden.set(hidden);
            self.actions.set_panel_hidden(hidden);
            return;
        }
        match companion_prefix::classify(&action) {
            CompanionVerb::Tab(tab) => self.run_tab(tab, pane),
            CompanionVerb::Reload => self.panel.reload_document_by_hand(),
            CompanionVerb::Keymap => {
                self.give_panel_keys();
                self.panel.open_keymap();
            }
            CompanionVerb::CommandLine => {
                self.give_panel_keys();
                self.panel.open_command_line();
            }
            CompanionVerb::Text(change) => {
                self.panel_scale.set(stepped(self.panel_scale.get(), change));
                self.panel.set_panel_font_size_px(self.panel_font_size_px());
            }
            CompanionVerb::Hint => self.actions.toast(NOT_IN_THIS_WINDOW),
            CompanionVerb::Literal => {
                if let Some(key) = literal_for(&action, &self.keymap) {
                    match pane {
                        Pane::Panel => self.panel.literal_key(&key),
                        Pane::Editor => {
                            if let Err(why) = self.editor.send_keys(&key.to_vim()) {
                                println!("[mac] prefix: the key did not reach the editor: {why}");
                                self.actions.toast(&why);
                            }
                        }
                    }
                }
            }
            CompanionVerb::Select(direction) => match neighbour(pane, direction) {
                Some(Pane::Panel) => self.give_panel_keys(),
                Some(Pane::Editor) => self.actions.focus(Pane::Editor),
                None => println!("[mac] prefix: nothing {direction:?} of the {pane:?}"),
            },
            CompanionVerb::Refuse => {
                println!("[mac] prefix: {action:?} refused");
                self.actions.toast(NOT_IN_THIS_WINDOW);
            }
        }
    }

    fn handle(&self, event: EditorEvent) {
        match event {
            EditorEvent::PaneSwitch(direction) => match neighbour(Pane::Editor, direction) {
                Some(_) => self.give_panel_keys(),
                None => println!("[mac] pane switch {direction:?} from the editor leads nowhere"),
            },
            EditorEvent::Theme(payload) => {
                println!(
                    "[theme] following nvim colorscheme {:?} (background={})",
                    payload.options.colors_name, payload.options.background
                );
                let mut tokens = ThemeTokens::derive(&payload);
                tokens.font_size_px = self.panel_font_size_px();
                self.panel.set_theme(&tokens);
            }
            EditorEvent::Keys(report) => self.keys_help.nvim_report(report),
            EditorEvent::LinkChanged(band) => {
                println!("[mac] link: {} {}", band.state, band.text);
                self.panel.set_editor_link(band);
            }
            EditorEvent::CancelDrafts => {
                let cut = self.panel.editor_detached();
                if cut > 0 {
                    println!("[mac] ended {cut} draft edit(s) out in the editor");
                }
                self.panel.review_editor_lost();
            }
        }
    }
}

pub struct Assembly {
    pub panel: AgentPanelHandle,
    /// Shared with the AppKit half, which calls `key_down` on it: bind the result with a `let` before
    /// acting on it, so the borrow is not held across a call back into the assembly.
    pub keys: RefCell<Keys>,
    pub editor: Rc<Editor>,
    pub config: WindowConfig,
    shared: Rc<Shared>,
}

/// Builds the panel the way the companion window does, in the same order: `init.lua` first (the
/// configuration's account and settings tiers must be in place before anything reads a transcript or
/// starts a sidecar), then the panel, then its hooks. Every `Err` is the text a start prints after
/// `eitri: `.
pub fn assemble(
    project_root: &Path,
    config_dir: &Path,
    page: Option<Rc<PanelPage>>,
    host: Rc<dyn PanelHost>,
    document: &'static PanelDocument,
    editor: Rc<Editor>,
    actions: Rc<dyn HostActions>,
) -> Result<Assembly, String> {
    let kernel = Kernel::new(config_dir.to_path_buf(), refuse_panels).map_err(|e| format!("the Lua kernel: {e}"))?;
    kernel.run_and_check_init_file(&config_dir.join("init.lua"))?;
    // This window has no place for a Lua command, and nothing runs `shell:ready`.
    let commands = kernel.commands.borrow().len();
    if commands > 0 {
        println!("[mac] init.lua registers {commands} command(s); this window runs none");
    }
    let config = window_config::load(&kernel, &[])?;
    let module_keys = Rc::new(ModuleKeys::build(&[], &config.keymap).map_err(|e| e.to_string())?);
    let backend_kind = BackendKind::from_env(false).map_err(|e| e.message)?;

    let init = PanelInit {
        project_dir: project_root.to_path_buf(),
        editor_context: editor.context(),
        scratch: editor.take_scratch(),
        backend_kind,
        document,
    };
    let panel = AgentPanelHandle::new(init, page, host);
    config.apply_to_panel(&panel);
    let mut tokens = ThemeTokens::fallback();
    tokens.font_size_px = config.panel_font_size;
    panel.set_theme(&tokens);

    let keys_help = KeysHelp::new(
        config.keymap.clone(),
        module_keys.clone(),
        panel.clone(),
        config.tmux_skipped.clone(),
        HelpScope::PanelBesideEditor,
    );
    keys_help.send();
    let shared = Rc::new(Shared {
        panel: panel.clone(),
        editor: editor.clone(),
        actions,
        keymap: config.keymap.clone(),
        keys_help,
        base_font_px: config.panel_font_size,
        panel_hidden: Cell::new(false),
        panel_scale: Cell::new(1.0),
    });
    install_hooks(&shared);
    let keys = Keys::new(config.keymap.clone(), module_keys, panel.nav_intercept());
    Ok(Assembly {
        panel,
        keys: RefCell::new(keys),
        editor,
        config,
        shared,
    })
}

fn install_hooks(shared: &Rc<Shared>) {
    let panel = &shared.panel;
    {
        let shared = shared.clone();
        panel.on_toast(move |text| shared.actions.toast(text));
    }
    {
        let shared = shared.clone();
        panel.on_tab_verb(move |action| shared.run_tab(action, Pane::Panel));
    }
    {
        let shared = shared.clone();
        panel.on_hint(move |message| {
            if matches!(message, HintInbound::Request) {
                shared.actions.toast(NOT_IN_THIS_WINDOW);
            }
        });
    }
    {
        // The page's stale-mirror recovery and its `pane_nav` ask for the move the chord would have made.
        let shared = shared.clone();
        panel.on_nav_fallthrough(move |direction| {
            if let Some(pane) = neighbour(Pane::Panel, direction) {
                shared.actions.focus(pane);
            }
        });
    }
    panel.set_editor_rpc(shared.editor.rpc());
    {
        let editor = shared.editor.clone();
        panel.set_review_owner(move || editor.review_owner());
    }
    {
        let shared = shared.clone();
        panel.on_review_open(move || shared.actions.show_editor());
    }
    {
        // A draft or a `gf` goes to the editor as one call, and the user is taken there.
        let shared = shared.clone();
        panel.on_editor_request(move |request| {
            shared.editor.scratch_call(request)?;
            shared.actions.show_editor();
            Ok(())
        });
    }
    {
        // An edit that came back: the keys return to the panel, on the oldest card if one waits.
        let shared = shared.clone();
        panel.on_editor_done(move || {
            shared.give_panel_keys();
            if !shared.panel.focus_oldest_card() {
                shared.panel.arrive();
            }
        });
    }
}

impl Assembly {
    /// What one [`Editor::poll`] found.
    pub fn handle(&self, event: EditorEvent) {
        self.shared.handle(event);
    }

    /// A command the prefix completed in `pane`: `Run(Zoom)` hides or shows the panel, every other action
    /// goes through the companion window's dispatch.
    pub fn run_prefix(&self, command: PrefixCommand, pane: Pane) {
        self.shared.run_prefix(command, pane);
    }

    /// The panel's text size now: `agent.font_size` times the window's own scale.
    pub fn panel_font_size_px(&self) -> f32 {
        self.shared.panel_font_size_px()
    }

    /// One call of the window's tick: the editor's feeds and link, each event handled.
    pub fn poll_editor(&self, now: Instant) {
        for event in self.editor.poll(now) {
            self.handle(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_text_steps_like_the_companion() {
        assert_eq!(stepped(1.0, TextChange::Larger), 1.1);
        assert_eq!(stepped(1.1, TextChange::Smaller), 1.0);
        assert_eq!(stepped(3.0, TextChange::Larger), 3.0);
        assert_eq!(stepped(0.5, TextChange::Smaller), 0.5);
        assert_eq!(stepped(2.4, TextChange::Reset), 1.0);
        // Repeated steps do not drift away from two decimals.
        let mut scale = 1.0;
        for _ in 0..7 {
            scale = stepped(scale, TextChange::Larger);
        }
        assert_eq!(scale, 1.7);
    }
}
