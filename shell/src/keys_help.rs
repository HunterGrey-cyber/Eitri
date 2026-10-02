//! The `?` overlay's window and prefix sections, generated from the keymap and sent to the agent
//! panel.
//!
//! The panel table (the merge of the user's panel keys and what nvim reports) and the new-tab chord
//! travel in the same envelope, and the panel table is recomputed -- and only re-sent when it
//! actually changed -- every time nvim's own keys report changes.

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::keymap::{Action, HelpRow, Keymap, PanelKeymap, TabAction};
use eitri_core::layout::ModuleKeys;
use eitri_core::nvim_keys::NvimReport;

use crate::agent_panel::AgentPanelHandle;

pub(crate) struct KeysHelp {
    /// `.0` is the last nvim report seen (`None` until the feed's first line, or forever if the feed
    /// could not start); `.1` is the last `PanelKeymap` actually sent, so a report that leaves the
    /// merged table unchanged costs no dispatch and no repeated log line.
    latest: RefCell<(Option<NvimReport>, Option<PanelKeymap>)>,
    keymap: Rc<Keymap>,
    module_keys: Rc<ModuleKeys>,
    panel: AgentPanelHandle,
    /// The chord for `Action::Tab(TabAction::New)`: the prefix as a person reads it, then the first
    /// key bound to the action.
    new_tab_chord: String,
    skipped: Vec<HelpRow>,
}

impl KeysHelp {
    pub(crate) fn new(
        keymap: Rc<Keymap>,
        module_keys: Rc<ModuleKeys>,
        panel: AgentPanelHandle,
        tmux_skipped: Vec<HelpRow>,
    ) -> Rc<Self> {
        let new_tab_chord = keymap
            .keys_for(&Action::Tab(TabAction::New))
            .first()
            .map(|key| format!("{} {}", keymap.prefix().human(), key.human()))
            .unwrap_or_default();
        Rc::new(Self {
            latest: RefCell::new((None, None)),
            keymap,
            module_keys,
            panel,
            new_tab_chord,
            skipped: tmux_skipped,
        })
    }

    /// Recomputes the panel table from the last nvim report and sends the help when it changed.
    pub(crate) fn send(&self) {
        let mut latest = self.latest.borrow_mut();
        let report = latest.0.clone();
        let (panel_keymap, log) = eitri_core::keymap::panel::effective(self.keymap.panel_user(), report.as_ref());
        for line in log {
            println!("{line}");
        }
        if latest.1.as_ref() != Some(&panel_keymap) {
            self.panel
                .set_keymap_help(eitri_core::agent_bridge::serialize_keymap_for_js(
                    &self.keymap.prefix().human(),
                    &eitri_core::keymap::root::help_rows(),
                    &self.keymap.help(&self.module_keys),
                    &panel_keymap,
                    &self.new_tab_chord,
                    &self.skipped,
                ));
            latest.1 = Some(panel_keymap);
        }
    }

    /// nvim's own keys changed: remember the report and re-send if the merged table moved.
    pub(crate) fn nvim_report(&self, report: NvimReport) {
        self.latest.borrow_mut().0 = Some(report);
        self.send();
    }
}
