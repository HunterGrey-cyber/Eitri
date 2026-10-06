//! The `?` overlay's window and prefix sections, generated from the keymap and sent to the agent
//! panel.
//!
//! The panel table (the merge of the user's panel keys and what nvim reports) and the new-tab chord
//! travel in the same envelope, and the panel table is recomputed -- and only re-sent when it
//! actually changed -- every time nvim's own keys report changes.

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::keymap::{Action, HelpRow, Keymap, PanelKeymap, TabAction};
use eitri_core::layout::{Direction, ModuleKeys};
use eitri_core::nvim_keys::NvimReport;

use eitri_core::keymap::companion::CompanionVerb;

use crate::agent_panel::AgentPanelHandle;

/// Which window the `?` overlay is drawn for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpScope {
    /// The full window: modules, layout, the root chords.
    Window,
    /// A companion window: one agent panel and nothing to lay out. Only the tab and panel actions its
    /// prefix runs are listed (`eitri_core::keymap::companion::classify`), and the root chords are one row, since
    /// the window manager decides what leaving the panel does.
    Companion,
    /// The panel and the editor in one window that hides the panel for the zoom: the companion's rows
    /// without HINT (this window has none), plus the zoom and the moves between the editor and the panel,
    /// and a root row that says where the keys go (the window, not a window manager, decides).
    PanelBesideEditor,
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
                    && !matches!(eitri_core::keymap::companion::classify(action), CompanionVerb::Refuse)
            }),
        ),
        HelpScope::PanelBesideEditor => (
            vec![HelpRow {
                keys: "Ctrl+h".into(),
                what: "leave the panel for the editor".into(),
            }],
            panel_beside_editor_rows(keymap, module_keys),
        ),
    }
}

/// The prefix rows of the window that holds the editor and the panel side by side.
fn panel_beside_editor_rows(keymap: &Keymap, module_keys: &ModuleKeys) -> Vec<HelpRow> {
    let keep = |action: &Action| match action {
        // The zoom is this window's own (it hides the panel), and `classify` refuses it for the companion
        // window, which has no panel to hide.
        Action::Zoom => true,
        // Here the window itself moves the keys between its two panes, the editor on the left and the panel
        // on the right; there is nothing above or below either of them.
        Action::Select(direction) => matches!(direction, Direction::Left | Direction::Right),
        action => !matches!(
            eitri_core::keymap::companion::classify(action),
            // This window has no global HINT: the prefix answers it with a notice, so it is not listed.
            CompanionVerb::Refuse | CompanionVerb::Hint
        ),
    };
    // The keymap's own wording speaks of modules, which this window does not have.
    let prefix = keymap.prefix().human();
    let module_move = Action::Select(Direction::Left).describe(&prefix, "");
    let module_zoom = Action::Zoom.describe(&prefix, "");
    keymap
        .help_where(module_keys, keep)
        .into_iter()
        .map(|row| {
            if let Some(rest) = row.what.strip_prefix(module_move.as_str()) {
                HelpRow {
                    what: format!("Move the keys between the editor (left) and the panel (right){rest}"),
                    ..row
                }
            } else if let Some(rest) = row.what.strip_prefix(module_zoom.as_str()) {
                // Here the zoom is the editor filling the window: it hides the panel, and shows it again.
                HelpRow {
                    what: format!("Hide the panel so the editor fills the window, or show it again{rest}"),
                    ..row
                }
            } else {
                row
            }
        })
        .collect()
}

pub struct KeysHelp {
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
    pub fn new(
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
    pub fn send(&self) {
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
    pub fn nvim_report(&self, report: NvimReport) {
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
    fn the_panel_beside_the_editor_lists_the_zoom_and_the_companion_rows() {
        let keymap = Keymap::defaults();
        let keys = ModuleKeys::built_in();
        let (_, companion) = help_tables(HelpScope::Companion, &keymap, &keys);
        let (root, beside) = help_tables(HelpScope::PanelBesideEditor, &keymap, &keys);
        assert_eq!(root.len(), 1);
        let extra: Vec<&HelpRow> = beside.iter().filter(|row| !companion.contains(row)).collect();
        assert_eq!(extra.len(), 2, "the zoom and the pane moves are added: {extra:?}");
        let zoom: Vec<&&HelpRow> = extra.iter().filter(|row| row.keys.ends_with(" z")).collect();
        assert_eq!(zoom.len(), 1, "{extra:?}");
        assert_eq!(
            zoom[0].what, "Hide the panel so the editor fills the window, or show it again",
            "the zoom hides the panel here, it does not zoom a module"
        );
        assert!(!zoom[0].what.contains("module"), "{zoom:?}");
        // The companion window and the full window keep the keymap's own wording.
        assert!(
            !companion.iter().any(|row| row.what.contains("Hide the panel")),
            "{companion:?}"
        );
        let (_, full) = help_tables(HelpScope::Window, &keymap, &keys);
        assert!(
            full.iter().any(|row| row.what == "Zoom this module, or restore"),
            "{full:?}"
        );
        let moves: Vec<&&HelpRow> = extra.iter().filter(|row| row.what.contains("(left)")).collect();
        assert_eq!(moves.len(), 1, "{extra:?}");
        assert_eq!(
            moves[0].keys, "Ctrl+b Left / Right",
            "nothing is above or below either pane"
        );
        assert!(!moves[0].what.contains("module"), "{moves:?}");
        // HINT is the one companion row this window answers with a notice.
        let missing: Vec<&HelpRow> = companion.iter().filter(|row| !beside.contains(row)).collect();
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(missing[0].what.contains("HINT"), "{missing:?}");
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
