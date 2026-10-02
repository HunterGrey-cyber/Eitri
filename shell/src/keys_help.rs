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
use crate::companion::prefix::CompanionVerb;

/// Which window the `?` overlay is drawn for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelpScope {
    /// The full window: modules, layout, the root chords.
    Window,
    /// A companion window: one agent panel and nothing to lay out. Only the tab and panel actions its
    /// prefix runs are listed (`companion::prefix::classify`), and the root chords are one row, since
    /// the window manager decides what leaving the panel does.
    Companion,
}

/// The two window-level tables of the overlay, "Anywhere in the window" then "After <prefix>", for
/// `scope`.
fn help_tables(scope: HelpScope, keymap: &Keymap, module_keys: &ModuleKeys) -> (Vec<HelpRow>, Vec<HelpRow>) {
    match scope {
        HelpScope::Window => (eitri_core::keymap::root::help_rows(), keymap.help(module_keys)),
        HelpScope::Companion => (
            vec![HelpRow {
                keys: "Ctrl+h / Ctrl+l".into(),
                what: "leave the panel (your window manager moves focus)".into(),
            }],
            keymap.help_where(module_keys, |action| {
                // A move toward the next window is the window manager's, not a module move, so its
                // row ("move the keys to the module that way") would say what does not happen here.
                !matches!(action, Action::Select(_))
                    && !matches!(crate::companion::prefix::classify(action), CompanionVerb::Refuse)
            }),
        ),
    }
}

pub(crate) struct KeysHelp {
    /// `.0` is the last nvim report seen (`None` until the feed's first line, or forever if the feed
    /// could not start); `.1` is the last `PanelKeymap` actually sent, so a report that leaves the
    /// merged table unchanged costs no dispatch and no repeated log line.
    latest: RefCell<(Option<NvimReport>, Option<PanelKeymap>)>,
    keymap: Rc<Keymap>,
    module_keys: Rc<ModuleKeys>,
    panel: AgentPanelHandle,
    scope: HelpScope,
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
        scope: HelpScope,
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
            scope,
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
            let (root_rows, prefix_rows) = help_tables(self.scope, &self.keymap, &self.module_keys);
            self.panel
                .set_keymap_help(eitri_core::agent_bridge::serialize_keymap_for_js(
                    &self.keymap.prefix().human(),
                    &root_rows,
                    &prefix_rows,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_scope_keeps_only_tab_and_panel_rows() {
        let keymap = Keymap::defaults();
        let keys = ModuleKeys::built_in();
        let (root, prefix) = help_tables(HelpScope::Companion, &keymap, &keys);
        assert_eq!(
            root,
            vec![HelpRow {
                keys: "Ctrl+h / Ctrl+l".into(),
                what: "leave the panel (your window manager moves focus)".into(),
            }]
        );
        let whats: Vec<&str> = prefix.iter().map(|row| row.what.as_str()).collect();
        // Tab verbs and the panel's own actions stay.
        assert!(whats.iter().any(|w| w.contains("New session tab")), "{whats:?}");
        assert!(whats.iter().any(|w| w.contains("Next session tab")), "{whats:?}");
        // Nothing about modules, splits, zoom, resizing, swapping or moving between modules.
        for gone in [
            "module",
            "Zoom",
            "divider",
            "Swap",
            "Even",
            "Every module",
            "Hide",
            "Close this module",
        ] {
            assert!(!whats.iter().any(|w| w.contains(gone)), "{gone:?} is listed: {whats:?}");
        }
        assert!(!prefix.is_empty());
    }

    #[test]
    fn window_scope_is_the_whole_table() {
        let keymap = Keymap::defaults();
        let keys = ModuleKeys::built_in();
        let (root, prefix) = help_tables(HelpScope::Window, &keymap, &keys);
        assert_eq!(root, eitri_core::keymap::root::help_rows());
        assert_eq!(prefix, keymap.help(&keys));
    }
}
