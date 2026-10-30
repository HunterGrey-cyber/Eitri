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
use eitri_panel::agent_panel::{AgentPanelHandle, HintInbound, PanelInit, ViewId};
use eitri_panel::keys_help::{HelpScope, KeysHelp};
use eitri_panel::panel_document::PanelDocument;
use eitri_panel::panel_page::{PanelHost, PanelPage};
use eitri_panel::window_config::{self, WindowConfig};

use crate::editor::{Editor, EditorEvent};
use crate::keys::{neighbour, Keys, Pane};

/// What the window says to an action that has no meaning in it (a split, a Lua panel, a HINT). The
/// companion window's own text names the wrong window.
pub const NOT_IN_THIS_WINDOW: &str = "not in this window";

/// What the window says when the keys cannot go to the panel because it is not on screen: the window is too
/// narrow to show it beside the editor.
pub const PANEL_NOT_SHOWN: &str = "the window is too narrow for the panel; widen it to use the panel";

/// What the window says when the panel is on screen but the keys did not move to it (AppKit refused its view
/// the keys, or the window had none yet).
pub const PANEL_TOOK_NO_KEYS: &str = "the panel could not take the keys; they stay where they are";

/// What only the AppKit half can do.
pub trait HostActions {
    /// Move the keys within the window. `true` when `pane` has them afterwards: AppKit may refuse its view the
    /// keys, and a panel whose view is hidden never takes them.
    fn focus(&self, pane: Pane) -> bool;
    fn toast(&self, text: &str);
    /// Bring the window forward and give the editor the keys: something happened there the user must see
    /// (a draft to edit, a file the review opened).
    fn show_editor(&self);
    /// The prefix's zoom: the panel is hidden (`true`) or shown again. The host relayouts and, when it
    /// hides the panel, moves the keys to the editor.
    fn set_panel_hidden(&self, hidden: bool);
    /// Whether the panel is on screen now: not hidden by the zoom, and the window wide enough to show it beside
    /// the editor. A panel that is not on screen must never get the keys: a plain `a` or `y` would answer a
    /// permission card nobody can see.
    fn panel_visible(&self) -> bool;
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

/// What the panel has been told about the keys, so it is told only of a change: whether it has keyboard focus
/// (it drops a pending bypass question when it loses it), and whether the editor has the keys (the typing
/// cadence holds the panel's stream back only while keys go to the editor, and losing them ends the typing
/// window at once). Either needs the window to be the key window: no key reaches a pane of a window that is not.
#[derive(Debug, Default)]
pub struct KeysReport {
    told: Option<(bool, bool)>,
}

/// One change [`KeysReport::update`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeysChange {
    PanelFocused(bool),
    EditorHasKeys(bool),
}

impl KeysReport {
    /// The changes to tell the panel now that `pane` has the keys and the window is key or not; all of them
    /// the first time.
    pub fn update(&mut self, pane: Pane, window_key: bool) -> Vec<KeysChange> {
        let now = (pane == Pane::Panel && window_key, pane == Pane::Editor && window_key);
        let before = self.told.replace(now);
        let mut changes = Vec::new();
        if before.map(|b| b.0) != Some(now.0) {
            changes.push(KeysChange::PanelFocused(now.0));
        }
        if before.map(|b| b.1) != Some(now.1) {
            changes.push(KeysChange::EditorHasKeys(now.1));
        }
        changes
    }
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
    /// The band the editor link last reported: `(state, text)`.
    editor_link: RefCell<Option<(String, String)>>,
    keys_report: RefCell<KeysReport>,
}

impl Shared {
    fn panel_font_size_px(&self) -> f32 {
        self.base_font_px * self.panel_scale.get()
    }

    /// The keys go to the panel. A panel hidden by the zoom is shown first: tmux's `select-pane` unzooms a
    /// zoomed window, and a tab verb that takes the keys means "show the chat". A panel the window is too narrow
    /// to show is refused with a notice, and the keys stay where they are: `false`, and the caller does nothing
    /// that needs them. The zoom is undone even then, as the request asked; the panel appears once the window is
    /// wide enough. A move the host could not make is refused the same way: what is decided is where the keys
    /// went, not where they were sent.
    fn give_panel_keys(&self) -> bool {
        if self.panel_hidden.replace(false) {
            self.actions.set_panel_hidden(false);
        }
        if !self.actions.panel_visible() {
            crate::log_line!("[mac] the panel is not on screen; the keys stay where they are");
            self.actions.toast(PANEL_NOT_SHOWN);
            return false;
        }
        if !self.actions.focus(Pane::Panel) {
            crate::log_line!("[mac] the panel did not take the keys; they stay where they are");
            self.actions.toast(PANEL_TOOK_NO_KEYS);
            return false;
        }
        true
    }

    /// The prefix's tab verbs, as the companion window runs them: the chat takes the keys first for the
    /// verbs that read them, then the verb goes to the panel.
    fn run_tab(&self, action: TabAction, pane: Pane) {
        let keys_in = match pane {
            Pane::Editor => ModuleKind::Editor,
            Pane::Panel => ModuleKind::Agent,
        };
        // No terminal exists here, so the count is zero: `n`/`p` step the chat's tabs.
        let plan = plan(action, Some(keys_in), 0, false);
        // A verb that reads the keys is not run in a panel that cannot have them: a close question would wait,
        // unseen, for the `y` meant for the editor.
        if plan.takes_the_keys && !self.give_panel_keys() {
            return;
        }
        let panel = &self.panel;
        let switched = match plan.verb {
            TabVerb::New => {
                panel.new_tab();
                panel.enter_input(ViewId::BASE);
                true
            }
            TabVerb::Step(delta) => panel.step(delta),
            TabVerb::Last => panel.select_last(),
            TabVerb::Select(n) => panel.select_number(n).is_some(),
            TabVerb::Rename => {
                panel.begin_rename(ViewId::BASE);
                true
            }
            TabVerb::Close => {
                panel.confirm_close(ViewId::BASE);
                true
            }
            TabVerb::CloseOthers => {
                panel.confirm_close_others();
                true
            }
            TabVerb::Choose => {
                panel.open_chooser(ViewId::BASE);
                true
            }
            TabVerb::Info => {
                panel.open_detail(ViewId::BASE);
                true
            }
            TabVerb::Flash => false,
            TabVerb::Nothing | TabVerb::Sibling(_) | TabVerb::Terminal(_) => return,
        };
        if !switched {
            // tmux: "can't find window".
            self.actions.toast("no such tab");
        }
    }

    fn run_prefix(&self, command: PrefixCommand, pane: Pane) {
        let action = match command {
            PrefixCommand::Number(digit) => {
                crate::log_line!("[mac] prefix: number {digit} refused");
                self.actions.toast(NOT_IN_THIS_WINDOW);
                return;
            }
            PrefixCommand::Workspace(pick) => {
                crate::log_line!("[mac] prefix: workspace {pick:?} refused");
                self.actions.toast(NOT_IN_THIS_WINDOW);
                return;
            }
            PrefixCommand::Run(action) => action,
            // A module key after a split key: there is nothing to place.
            PrefixCommand::Place { module, axis, cwd: _ } => {
                crate::log_line!("[mac] prefix: place {module} {axis:?} refused");
                self.actions.toast(NOT_IN_THIS_WINDOW);
                return;
            }
            // A module key after the hide key: there is no layout to hide it from.
            PrefixCommand::Hide { module } => {
                crate::log_line!("[mac] prefix: hide {module} refused");
                self.actions.toast(NOT_IN_THIS_WINDOW);
                return;
            }
        };
        if action == Action::Zoom {
            // The zoom is the user's choice and is kept whatever the width; asked to show a panel the window is too
            // narrow for, it says why nothing appears.
            let hidden = !self.panel_hidden.get();
            self.panel_hidden.set(hidden);
            self.actions.set_panel_hidden(hidden);
            if !hidden && !self.actions.panel_visible() {
                self.actions.toast(PANEL_NOT_SHOWN);
            }
            return;
        }
        match companion_prefix::classify(&action) {
            CompanionVerb::Tab(tab) => self.run_tab(tab, pane),
            CompanionVerb::Reload => self.panel.reload_document_by_hand(ViewId::BASE),
            CompanionVerb::Keymap => {
                if self.give_panel_keys() {
                    self.panel.open_keymap(ViewId::BASE, None, 0);
                }
            }
            CompanionVerb::CommandLine => {
                if self.give_panel_keys() {
                    self.panel.open_command_line(ViewId::BASE);
                }
            }
            CompanionVerb::Text(change) => {
                self.panel_scale.set(stepped(self.panel_scale.get(), change));
                self.panel.set_panel_font_size_px(self.panel_font_size_px());
            }
            CompanionVerb::Hint => self.actions.toast(NOT_IN_THIS_WINDOW),
            CompanionVerb::Literal => {
                if let Some(key) = literal_for(&action, &self.keymap) {
                    match pane {
                        Pane::Panel => self.panel.literal_key(ViewId::BASE, &key),
                        Pane::Editor => {
                            if let Err(why) = self.editor.send_keys(&key.to_vim()) {
                                crate::log_line!("[mac] prefix: the key did not reach the editor: {why}");
                                self.actions.toast(&why);
                            }
                        }
                    }
                }
            }
            CompanionVerb::Select(direction) => match neighbour(pane, direction) {
                Some(Pane::Panel) => {
                    self.give_panel_keys();
                }
                Some(Pane::Editor) => {
                    self.actions.focus(Pane::Editor);
                }
                None => crate::log_line!("[mac] prefix: nothing {direction:?} of the {pane:?}"),
            },
            CompanionVerb::Refuse => {
                crate::log_line!("[mac] prefix: {action:?} refused");
                self.actions.toast(NOT_IN_THIS_WINDOW);
            }
        }
    }

    fn handle(&self, event: EditorEvent) {
        match event {
            EditorEvent::PaneSwitch(direction) => match neighbour(Pane::Editor, direction) {
                Some(_) => {
                    self.give_panel_keys();
                }
                None => crate::log_line!("[mac] pane switch {direction:?} from the editor leads nowhere"),
            },
            EditorEvent::Theme(payload) => {
                crate::log_line!(
                    "[theme] following nvim colorscheme {:?} (background={})",
                    payload.options.colors_name,
                    payload.options.background
                );
                let mut tokens = ThemeTokens::derive(&payload);
                tokens.font_size_px = self.panel_font_size_px();
                self.panel.set_theme(&tokens);
            }
            EditorEvent::Keys(report) => self.keys_help.nvim_report(report),
            EditorEvent::LinkChanged(band) => {
                crate::log_line!("[mac] link: {} {}", band.state, band.text);
                *self.editor_link.borrow_mut() = Some((band.state.to_owned(), band.text.clone()));
                self.panel.set_editor_link(band);
            }
            EditorEvent::ContextChanged => self.panel.wake(),
            EditorEvent::CancelDrafts => {
                let cut = self.panel.editor_detached();
                if cut > 0 {
                    crate::log_line!("[mac] ended {cut} draft edit(s) out in the editor");
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
        crate::log_line!("[mac] init.lua registers {commands} command(s); this window runs none");
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
        tab_role: eitri_core::window_link::pair::Role::Main,
        theme: ThemeTokens::fallback(),
    };
    let panel = AgentPanelHandle::new(init, page.is_some(), host);
    if let Some(page) = page {
        panel.attach_page(ViewId::BASE, page);
    }
    config.apply_to_panel(&panel);
    // This window is never one of a pair: it offers its own saved tabs, once `agent.restore` is known.
    panel.set_tab_files(Default::default());
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
        editor_link: RefCell::new(None),
        keys_report: RefCell::new(KeysReport::default()),
    });
    install_hooks(&shared);
    // The chat page keeps a `Ctrl+j`/`Ctrl+k` for its composer when its mode says so; a kept one is handed to it.
    let intercept: Rc<dyn Fn(eitri_core::layout::Direction) -> bool> = {
        let panel = panel.clone();
        Rc::new(move |direction| {
            let kept = panel.nav_claims(ViewId::BASE, direction);
            if kept {
                panel.nav_key(ViewId::BASE, direction);
            }
            kept
        })
    };
    let keys = Keys::new(config.keymap.clone(), module_keys, intercept);
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
        panel.on_tab_verb(move |_view, action| shared.run_tab(action, Pane::Panel));
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
        panel.on_nav_fallthrough(move |_view, direction| {
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
        panel.on_editor_done(move |_view| {
            // The arrival is where the keys land in the panel; without them it has nowhere to land.
            if shared.give_panel_keys() && !shared.panel.focus_oldest_card(ViewId::BASE) {
                shared.panel.arrive(ViewId::BASE);
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

    /// Tells the panel where the keys are: `pane` has them, and the window is the key window or not. Call it
    /// whenever either may have changed (a move between the panes, the window gaining or losing the keys, the
    /// panel hidden); only a change reaches the panel.
    pub fn report_keys(&self, pane: Pane, window_key: bool) {
        // Bound first: the panel's calls below must find no borrow held.
        let changes = self.shared.keys_report.borrow_mut().update(pane, window_key);
        for change in changes {
            match change {
                KeysChange::PanelFocused(focused) => self.panel.set_pane_focused(ViewId::BASE, focused),
                KeysChange::EditorHasKeys(has) => self.panel.set_editor_has_keys(has),
            }
        }
    }

    /// The panel's text size now: `agent.font_size` times the window's own scale.
    pub fn panel_font_size_px(&self) -> f32 {
        self.shared.panel_font_size_px()
    }

    /// The band the editor link last reported, `(state, text)`; `None` before the first report.
    pub fn editor_link(&self) -> Option<(String, String)> {
        self.shared.editor_link.borrow().clone()
    }

    /// Shows a band about the editor link the host itself knows (while it waits for nvim), as the link's
    /// own reports are shown.
    pub fn report_editor_link(&self, state: &'static str, text: &str) {
        self.shared
            .handle(EditorEvent::LinkChanged(eitri_core::companion::attach::BandLink {
                state,
                text: text.to_owned(),
            }));
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
    fn the_editor_has_the_keys_only_while_its_pane_has_them_in_the_key_window() {
        use KeysChange::{EditorHasKeys, PanelFocused};
        let mut report = KeysReport::default();
        // The first report says both.
        assert_eq!(
            report.update(Pane::Editor, true),
            [PanelFocused(false), EditorHasKeys(true)]
        );
        assert_eq!(report.update(Pane::Editor, true), [], "no change, nothing told");
        // A move to the panel: the typing window ends.
        assert_eq!(
            report.update(Pane::Panel, true),
            [PanelFocused(true), EditorHasKeys(false)]
        );
        // Back to the editor (`Ctrl+h`, or the panel hidden by the zoom or a narrow window).
        assert_eq!(
            report.update(Pane::Editor, true),
            [PanelFocused(false), EditorHasKeys(true)]
        );
        // The window lost the keys (another app, the quit question's sheet) and got them back.
        assert_eq!(report.update(Pane::Editor, false), [EditorHasKeys(false)]);
        assert_eq!(report.update(Pane::Editor, true), [EditorHasKeys(true)]);
        // The panel's focus follows the window too, and the editor's stays off meanwhile.
        report.update(Pane::Panel, true);
        assert_eq!(report.update(Pane::Panel, false), [PanelFocused(false)]);
        assert_eq!(report.update(Pane::Editor, false), []);
    }

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
