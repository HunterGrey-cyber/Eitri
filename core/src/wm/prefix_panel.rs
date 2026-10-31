//! The hint panel's content while the prefix waits for a key: a heading, the groups of what each key does and
//! a footer saying how to leave and where the whole list is. Plain text, as every field of the chrome is.

use super::prefix_strip;
use crate::keymap::hints::{HintGroup, HintRow};
use crate::keymap::prefix::Waiting;
use crate::keymap::{Action, Keymap};
use crate::layout::{ModuleId, ModuleKeys, StripEntry};

/// What the hint panel shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefixPanel {
    pub heading: String,
    pub groups: Vec<HintGroup>,
    pub footer: String,
}

/// The panel for `waiting`, or `None` for a wait that has none (nothing, the digits, the workspace chooser).
/// After the prefix it lists the keymap's groups; after a split key, the module keys the strip lists, under the
/// strip's own heading. `module_entries` are the module keys after a split key and are read only then;
/// `focus_title` names the module with the keys.
pub fn prefix_panel(
    waiting: Waiting,
    keymap: &Keymap,
    module_keys: &ModuleKeys,
    module_entries: &[StripEntry],
    focus_title: &str,
    title: &dyn Fn(&ModuleId) -> String,
) -> Option<PrefixPanel> {
    let module_rows = |after_split: bool| {
        vec![HintGroup {
            title: "Modules",
            rows: module_entries
                .iter()
                .map(|entry| HintRow {
                    keys: entry.key.clone(),
                    label: prefix_strip::entry_label(entry, title, after_split),
                })
                .collect(),
        }]
    };
    let (heading, groups) = match waiting {
        Waiting::Command => (
            format!("{} \u{2026}", keymap.prefix().human()),
            keymap.panel_groups(module_keys, title, |_| true),
        ),
        Waiting::Module(axis) => (prefix_strip::module_heading(axis, focus_title), module_rows(true)),
        Waiting::Hide => (prefix_strip::HIDE_HEADING.to_string(), module_rows(false)),
        Waiting::No | Waiting::Number | Waiting::Workspace { .. } => return None,
    };
    // Only the wait after the prefix opens the keys sheet; after a split key every key is read as a module's, so
    // the sheet's key would only cancel the split.
    let footer = match (waiting, keymap.keys_for(&Action::PanelKeymap).first()) {
        (Waiting::Command, Some(key)) => format!("Esc cancel \u{00b7} {} all keys", key.human()),
        _ => "Esc cancel".to_string(),
    };
    Some(PrefixPanel {
        heading,
        groups,
        footer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeymapOp;
    use crate::layout::{Axis, Layout, Node};

    fn title(id: &ModuleId) -> String {
        id.as_str().to_string()
    }

    fn keys(keymap: &Keymap) -> ModuleKeys {
        ModuleKeys::build(&[], keymap).unwrap()
    }

    #[test]
    fn after_the_prefix_the_heading_names_it_and_the_groups_are_the_keymaps() {
        let keymap = Keymap::defaults();
        let keys = keys(&keymap);
        let panel = prefix_panel(Waiting::Command, &keymap, &keys, &[], "editor", &title).unwrap();
        assert_eq!(panel.heading, "Ctrl+b \u{2026}");
        assert_eq!(panel.groups, keymap.panel_groups(&keys, &title, |_| true));
        assert_eq!(panel.footer, "Esc cancel \u{00b7} ? all keys");
    }

    #[test]
    fn after_a_split_key_the_panel_lists_the_module_keys_under_the_strips_heading() {
        let keymap = Keymap::defaults();
        let keys = keys(&keymap);
        let layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::terminal()),
            ),
            ModuleId::terminal(),
        )
        .unwrap();
        let entries = crate::layout::strip(&keys, &layout);
        let panel = prefix_panel(Waiting::Module(Axis::Row), &keymap, &keys, &entries, "terminal", &title).unwrap();
        assert_eq!(panel.heading, "right of terminal:");
        assert_eq!(
            panel.footer, "Esc cancel",
            "the keys sheet's key is no use while a module key is awaited"
        );
        assert_eq!(panel.groups.len(), 1);
        assert_eq!(panel.groups[0].title, "Modules");
        let terminal = panel.groups[0].rows.iter().find(|row| row.keys == "t").unwrap();
        assert_eq!(terminal.label, "new terminal", "the keys are in a terminal: a new one");
        let editor = panel.groups[0].rows.iter().find(|row| row.keys == "e").unwrap();
        assert_eq!(editor.label, "editor");
    }

    #[test]
    fn the_wait_for_a_module_to_hide_lists_the_modules_on_screen() {
        let keymap = Keymap::defaults();
        let keys = keys(&keymap);
        let title = |id: &ModuleId| id.as_str().to_string();
        let layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::terminal()),
            ),
            ModuleId::terminal(),
        )
        .unwrap();
        let entries = crate::layout::strip_for_hide(&keys, &layout);
        let panel = prefix_panel(Waiting::Hide, &keymap, &keys, &entries, "terminal", &title).unwrap();
        assert_eq!(panel.heading, "hide:");
        assert_eq!(panel.footer, "Esc cancel");
        let terminal = panel.groups[0].rows.iter().find(|row| row.keys == "t").unwrap();
        assert_eq!(terminal.label, "terminal", "a hide never says a new one");
    }

    #[test]
    fn the_footer_leaves_out_the_keys_sheet_when_nothing_opens_it() {
        let keymap = Keymap::apply_user(
            &[KeymapOp::Del {
                table: "prefix".into(),
                key: "?".into(),
            }],
            &[],
        )
        .unwrap();
        let panel = prefix_panel(Waiting::Command, &keymap, &keys(&keymap), &[], "editor", &title).unwrap();
        assert_eq!(panel.footer, "Esc cancel");
    }

    #[test]
    fn waits_with_a_list_of_their_own_have_no_panel() {
        let keymap = Keymap::defaults();
        let keys = keys(&keymap);
        for waiting in [Waiting::No, Waiting::Number, Waiting::Workspace { deleting: false }] {
            assert_eq!(
                prefix_panel(waiting, &keymap, &keys, &[], "editor", &title),
                None,
                "{waiting:?}"
            );
        }
    }
}
