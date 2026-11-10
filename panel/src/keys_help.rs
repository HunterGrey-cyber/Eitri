//! The `?` overlay's window and prefix sections, generated from the keymap and sent to the agent
//! panel.
//!
//! The panel table (the merge of the user's panel keys and what nvim reports) and the new-tab chord
//! travel in the same envelope. Help is recomputed when nvim reports its keys or a page joins,
//! and only re-sent when the panel table or module keys changed.

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::keymap::{Action, HelpRow, Keymap, PanelKeymap, TabAction};
use eitri_core::layout::{Direction, ModuleId, ModuleKeys};
use eitri_core::nvim_keys::NvimReport;

use eitri_core::keymap::companion::CompanionVerb;
use eitri_core::keymap::compositor::{classify_module, ModuleVerb};

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
    /// A chat window of compositor mode: only the keys that do something in it are listed
    /// (`eitri_core::keymap::compositor::classify_module`), and the root rows say the compositor moves the focus.
    ModuleWindow,
}

/// The two window-level tables of the overlay, "Anywhere in the window" then "After <prefix>", for
/// `scope`.
fn help_tables(scope: HelpScope, keymap: &Keymap, module_keys: &ModuleKeys) -> (Vec<HelpRow>, Vec<HelpRow>) {
    use eitri_core::keymap::root::user_help_rows as user_rows;
    match scope {
        HelpScope::Window => {
            let mut root = eitri_core::keymap::root::help_rows();
            root.extend(user_rows(keymap, |_| true));
            (root, keymap.help(module_keys))
        }
        HelpScope::Companion => {
            let mut root = vec![HelpRow {
                keys: "Ctrl+h / Ctrl+l".into(),
                what: "leave the panel (your window manager moves focus)".into(),
            }];
            root.extend(companion_root_rows(
                keymap,
                user_rows(keymap, eitri_core::keymap::companion::companion_root_keep),
            ));
            (root, keymap.help_where(module_keys, companion_keeps))
        }
        HelpScope::PanelBesideEditor => {
            let mut root = vec![HelpRow {
                keys: "Ctrl+h".into(),
                what: "leave the panel for the editor".into(),
            }];
            root.extend(user_rows(keymap, eitri_core::keymap::companion::beside_keep));
            (root, panel_beside_editor_rows(keymap, module_keys))
        }
        HelpScope::ModuleWindow => {
            let works_here = |action: &Action| {
                matches!(
                    classify_module(action),
                    ModuleVerb::AsToday | ModuleVerb::AskHub | ModuleVerb::TabBreak
                )
            };
            let root_works_here = |action: &Action| {
                works_here(action) || matches!(classify_module(action), ModuleVerb::EdgeToCompositor(_))
            };
            let mut root = eitri_core::keymap::root::compositor_help_rows();
            root.extend(companion_root_rows(keymap, user_rows(keymap, root_works_here)));
            (root, keymap.help_where(module_keys, works_here))
        }
    }
}

/// A root key that moves the keys toward a neighbour runs here as a move to the next window, which the window manager
/// makes; the keymap's own wording ("to the module that way") would say what does not happen.
fn companion_root_rows(keymap: &Keymap, rows: Vec<HelpRow>) -> Vec<HelpRow> {
    let module_move = Action::Select(Direction::Left).describe(&keymap.prefix().human(), "as after the prefix");
    rows.into_iter()
        .map(|row| match row.what.strip_prefix(module_move.as_str()) {
            Some(rest) => HelpRow {
                what: format!("Move the keys to the next window that way (your window manager moves focus){rest}"),
                ..row
            },
            None => row,
        })
        .collect()
}

/// The actions a companion window lists: its own tab and panel verbs.
fn companion_keeps(action: &Action) -> bool {
    // A move toward the next window is the window manager's, not a module move, so its
    // row ("move the keys to the module that way") would say what does not happen here.
    !matches!(action, Action::Select(_))
        && !matches!(eitri_core::keymap::companion::classify(action), CompanionVerb::Refuse)
}

/// The prefix rows of the window that holds the editor and the panel side by side.
fn panel_beside_editor_rows(keymap: &Keymap, module_keys: &ModuleKeys) -> Vec<HelpRow> {
    // The keymap's own wording speaks of modules, which this window does not have.
    let prefix = keymap.prefix().human();
    let module_move = Action::Select(Direction::Left).describe(&prefix, "");
    let module_zoom = Action::Zoom.describe(&prefix, "");
    keymap
        .help_where(module_keys, eitri_core::keymap::companion::beside_keep)
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
    /// merged table and module keys unchanged costs no dispatch and no repeated log line.
    latest: RefCell<(Option<NvimReport>, Option<PanelKeymap>)>,
    sent_modules: RefCell<Vec<(char, ModuleId)>>,
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
            sent_modules: RefCell::default(),
            keymap,
            module_keys,
            panel,
            scope,
            new_tab_chord,
            skipped: tmux_skipped,
        })
    }

    /// Recomputes help from the last nvim report and live module keys, sending only when either changed.
    pub fn send(&self) {
        let mut latest = self.latest.borrow_mut();
        let report = latest.0.clone();
        let (panel_keymap, log) = eitri_core::keymap::panel::effective(self.keymap.panel_user(), report.as_ref());
        for line in log {
            println!("{line}");
        }
        let modules = self.module_keys.entries().to_vec();
        if latest.1.as_ref() != Some(&panel_keymap) || *self.sent_modules.borrow() != modules {
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
            *self.sent_modules.borrow_mut() = modules;
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
    fn help_resends_when_only_module_keys_change() {
        #[derive(Default)]
        struct Page(RefCell<Vec<String>>);
        impl crate::panel_page::PageSurface for Page {
            fn send(&self, payload: &str) {
                self.0.borrow_mut().push(payload.to_string());
            }
            fn is_visible(&self) -> bool {
                true
            }
            fn load_document(&self, _: &str, _: &str) {}
            fn set_background(&self, _: [u8; 3]) {}
        }
        let page = Rc::new(Page::default());
        let keymap = Rc::new(Keymap::defaults());
        let keys = Rc::new(ModuleKeys::built_in());
        let help = KeysHelp::new(
            keymap.clone(),
            keys.clone(),
            AgentPanelHandle::for_host_test(page.clone()),
            vec![],
            HelpScope::Window,
        );
        help.send();
        page.0.borrow_mut().clear();
        let late = eitri_core::layout::ModuleId::lua("late");
        keys.add_default(&late, 'U', &keymap).unwrap();
        help.send();
        assert!(page
            .0
            .borrow()
            .iter()
            .any(|payload| payload.contains(r#""keys":"Ctrl+b U""#)));
        page.0.borrow_mut().clear();
        help.send();
        assert!(page.0.borrow().is_empty(), "unchanged keys send nothing");
        keys.remove(&late);
        help.send();
        assert!(!page.0.borrow().is_empty(), "removal refreshes help too");
        assert!(!page
            .0
            .borrow()
            .iter()
            .any(|payload| payload.contains(r#""keys":"Ctrl+b U""#)));
    }

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
    fn a_chat_window_of_compositor_mode_lists_no_key_that_is_off_there() {
        let keymap = Keymap::defaults();
        let keys = ModuleKeys::built_in();
        let (root, prefix) = help_tables(HelpScope::ModuleWindow, &keymap, &keys);
        assert_eq!(root, eitri_core::keymap::root::compositor_help_rows());
        assert!(
            root.iter().all(|row| !row.what.contains("top bar")),
            "at the edge a key goes to the compositor: {root:?}"
        );
        let split = Action::Split {
            axis: eitri_core::layout::Axis::Row,
            cwd: Default::default(),
        };
        let swap = Action::Swap(eitri_core::keymap::SwapTarget::Next);
        for action in [split, Action::Zoom, swap, Action::Even(eitri_core::layout::Axis::Row)] {
            let what = action.describe(&keymap.prefix().human(), "");
            assert!(
                prefix.iter().all(|row| !row.what.starts_with(&what)),
                "{what} is listed: {prefix:?}"
            );
        }
        let tab = Action::Tab(TabAction::New).describe(&keymap.prefix().human(), "");
        assert!(prefix.iter().any(|row| row.what.starts_with(&tab)), "{prefix:?}");
    }

    #[test]
    fn window_scope_is_the_whole_table() {
        let keymap = Keymap::defaults();
        let keys = ModuleKeys::built_in();
        let (root, prefix) = help_tables(HelpScope::Window, &keymap, &keys);
        assert_eq!(root, eitri_core::keymap::root::help_rows());
        assert_eq!(prefix, keymap.help(&keys));
    }

    fn with_root_keys() -> Keymap {
        let root = |key: &str, action: &str| eitri_core::keymap::KeymapOp::Set {
            table: "root".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        };
        Keymap::apply_user(&[root("M-y", "tab.next"), root("C-M-t", "zoom")], &[]).unwrap()
    }

    #[test]
    fn the_window_scopes_anywhere_rows_end_with_the_root_keys() {
        let keymap = with_root_keys();
        let keys = ModuleKeys::built_in();
        let (root, prefix) = help_tables(HelpScope::Window, &keymap, &keys);
        let built_in = eitri_core::keymap::root::help_rows();
        assert_eq!(root.len(), built_in.len() + 2);
        assert_eq!(&root[..built_in.len()], built_in.as_slice());
        assert_eq!(root[built_in.len()].keys, "Alt+y");
        assert!(root[built_in.len()].what.ends_with(" (init.lua)"), "{root:?}");
        assert_eq!(root[built_in.len() + 1].keys, "Ctrl+Alt+t");
        assert_eq!(prefix, keymap.help(&keys), "the prefix rows list no root key");
    }

    #[test]
    fn the_companion_and_beside_scopes_list_only_the_root_keys_they_run() {
        let keymap = with_root_keys();
        let keys = ModuleKeys::built_in();
        let (root, _) = help_tables(HelpScope::Companion, &keymap, &keys);
        assert_eq!(root.len(), 2, "{root:?}");
        assert_eq!(root[1].keys, "Alt+y", "the zoom is not a companion verb: {root:?}");
        let (root, _) = help_tables(HelpScope::PanelBesideEditor, &keymap, &keys);
        assert_eq!(root.len(), 3, "this window also has the zoom: {root:?}");
        assert_eq!(root[2].keys, "Ctrl+Alt+t");
    }

    /// A module window runs a focus move as the compositor's, so it words it that way; layout keys are off there.
    #[test]
    fn a_module_window_lists_the_root_keys_it_runs() {
        let root = |key: &str, action: &str| eitri_core::keymap::KeymapOp::Set {
            table: "root".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        };
        let keymap = Keymap::apply_user(&[root("M-y", "select.left"), root("M-z", "zoom")], &[]).unwrap();
        let keys = ModuleKeys::built_in();
        let (rows, _) = help_tables(HelpScope::ModuleWindow, &keymap, &keys);
        let built_in = eitri_core::keymap::root::compositor_help_rows();
        assert_eq!(rows.len(), built_in.len() + 1, "{rows:?}");
        let row = rows.last().unwrap();
        assert_eq!(row.keys, "Alt+y");
        assert!(row.what.contains("your window manager moves focus"), "{row:?}");
    }

    /// Each window lists the root keys it runs and no other: a companion window runs a move toward the next window
    /// (and words it that way); the editor-and-panel window answers HINT with a notice and has nothing above or
    /// below either pane.
    #[test]
    fn a_root_row_follows_what_the_window_runs() {
        let root = |key: &str, action: &str| eitri_core::keymap::KeymapOp::Set {
            table: "root".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        };
        let keymap = Keymap::apply_user(
            &[
                root("M-y", "select.left"),
                root("M-u", "hint"),
                root("M-i", "select.up"),
            ],
            &[],
        )
        .unwrap();
        let keys = ModuleKeys::built_in();
        let (companion, _) = help_tables(HelpScope::Companion, &keymap, &keys);
        let listed: Vec<&str> = companion.iter().skip(1).map(|row| row.keys.as_str()).collect();
        assert_eq!(listed, ["Alt+y", "Alt+u", "Alt+i"], "{companion:?}");
        assert!(companion[1].what.contains("window manager"), "{companion:?}");
        assert!(!companion[1].what.contains("module"), "{companion:?}");
        let (beside, _) = help_tables(HelpScope::PanelBesideEditor, &keymap, &keys);
        let listed: Vec<&str> = beside.iter().skip(1).map(|row| row.keys.as_str()).collect();
        assert_eq!(listed, ["Alt+y"], "no HINT and no move up or down here: {beside:?}");
    }
}
