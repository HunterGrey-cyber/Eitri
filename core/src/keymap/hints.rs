//! What the hint panel lists after the prefix: the `keymap.which_key_delay` setting, and the keymap's bindings
//! grouped by what they do, each row a key (or keys) and a short label.

use std::time::Duration;

use super::action::Action;
use super::key::{KeyName, KeySpec};
use super::table::{Keymap, Source};
use crate::layout::{ModuleId, ModuleKeys, ModuleKind};
use crate::wm::verbs::{family, Family};

/// The `init.lua` key that says how long the prefix waits before the hint panel shows.
pub const WHICH_KEY_DELAY_KEY: &str = "keymap.which_key_delay";

/// `keymap.which_key_delay` as `init.lua` left it: unset, `false` and `"off"` show no panel, a whole number of
/// milliseconds from 0 to 5000 is how long the prefix waits first. Anything else is a refusal naming the key.
pub fn parse_which_key_delay(value: Option<&str>) -> Result<Option<Duration>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if crate::config_switch::is_off(value) {
        return Ok(None);
    }
    value
        .parse::<u32>()
        .ok()
        .filter(|ms| *ms <= 5000)
        .map(|ms| Some(Duration::from_millis(u64::from(ms))))
        .ok_or_else(|| {
            format!("{WHICH_KEY_DELAY_KEY} {value:?}: must be a whole number from 0 to 5000, or false or \"off\"")
        })
}

/// One line of the panel: the keys that run something and what it is.
#[derive(Debug, Clone, PartialEq)]
pub struct HintRow {
    pub keys: String,
    pub label: String,
}

/// A titled block of rows.
#[derive(Debug, Clone, PartialEq)]
pub struct HintGroup {
    pub title: &'static str,
    pub rows: Vec<HintRow>,
}

/// The groups in the order the panel shows them, each with the families whose bindings it lists.
const GROUPS: [(&str, &[Family]); 7] = [
    ("Tabs", &[Family::Panel]),
    ("Modules", &[Family::Modules, Family::Focus, Family::Close]),
    ("Layout", &[Family::Tiling]),
    ("Workspaces", &[Family::Floats, Family::Workspaces]),
    ("Windows", &[Family::Windows]),
    ("Projects", &[Family::Project]),
    ("Other", &[Family::Keys, Family::Chrome, Family::TextSize]),
];

const MODULES: usize = 1;

fn group_of(family: Family) -> usize {
    GROUPS
        .iter()
        .position(|(_, families)| families.contains(&family))
        .expect("every family has a group")
}

/// How a key reads in a row: its human spelling, the four arrows as arrows.
fn key_text(key: &KeySpec) -> String {
    let human = key.human();
    let (name, glyph) = match key.key {
        KeyName::Up => ("Up", "\u{2191}"),
        KeyName::Down => ("Down", "\u{2193}"),
        KeyName::Left => ("Left", "\u{2190}"),
        KeyName::Right => ("Right", "\u{2192}"),
        _ => return human,
    };
    match human.strip_suffix(name) {
        Some(modifiers) => format!("{modifiers}{glyph}"),
        None => human,
    }
}

/// The digit a plain, unmodified digit key is.
fn digit(key: &KeySpec) -> Option<u32> {
    match key.key {
        KeyName::Char(c) if !key.ctrl && !key.meta && !key.shift => c.to_digit(10),
        _ => None,
    }
}

/// Keys joined with a space; three or more consecutive digits count as a range (`1-9`).
fn join_keys(keys: &[KeySpec]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut at = 0;
    while at < keys.len() {
        let mut end = at;
        if let Some(first) = digit(&keys[at]) {
            while keys.get(end + 1).and_then(digit) == Some(first + (end - at) as u32 + 1) {
                end += 1;
            }
        }
        if end - at >= 2 {
            parts.push(format!(
                "{}-{}",
                digit(&keys[at]).unwrap_or(0),
                digit(&keys[end]).unwrap_or(0)
            ));
            at = end + 1;
        } else {
            parts.push(key_text(&keys[at]));
            at += 1;
        }
    }
    parts.join(" ")
}

impl Keymap {
    /// The panel's groups: every binding `keep` accepts, in table order, under the group of its family, plus a row
    /// for each Lua panel's module key (named by `title`) in `"Modules"`. Adjacent bindings with the same label
    /// share a row. An empty group is left out.
    pub fn panel_groups(
        &self,
        keys: &ModuleKeys,
        title: &dyn Fn(&ModuleId) -> String,
        keep: impl Fn(&Action) -> bool,
    ) -> Vec<HintGroup> {
        // Per group: the rows' labels with the keys that share each.
        let mut rows: Vec<Vec<(String, Vec<KeySpec>)>> = vec![Vec::new(); GROUPS.len()];
        for binding in self.bindings().iter().filter(|b| keep(&b.action)) {
            let mut label = match &binding.action {
                Action::Module(id) if id.kind() == ModuleKind::LuaWebview => title(id),
                action => action.short_label().to_string(),
            };
            if binding.repeatable {
                label.push_str(" (repeats)");
            }
            if binding.source == Source::Tmux {
                label.push_str(" (tmux)");
            }
            let group = &mut rows[group_of(family(&binding.action))];
            match group.last_mut() {
                Some((last, keys)) if *last == label => keys.push(binding.key),
                _ => group.push((label, vec![binding.key])),
            }
        }
        let mut groups: Vec<HintGroup> = GROUPS
            .iter()
            .zip(rows)
            .map(|((name, _), rows)| HintGroup {
                title: name,
                rows: rows
                    .into_iter()
                    .map(|(label, keys)| HintRow {
                        keys: join_keys(&keys),
                        label,
                    })
                    .collect(),
            })
            .collect();
        for (key, id) in keys.entries() {
            if id.kind() == ModuleKind::LuaWebview && keep(&Action::Module(id.clone())) {
                groups[MODULES].rows.push(HintRow {
                    keys: key.to_string(),
                    label: title(id),
                });
            }
        }
        groups.retain(|group| !group.rows.is_empty());
        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeymapOp;

    fn title(id: &ModuleId) -> String {
        id.as_str().trim_start_matches("lua:").to_string()
    }

    fn plain_keys(keymap: &Keymap) -> ModuleKeys {
        ModuleKeys::build(&[], keymap).unwrap()
    }

    fn set(key: &str, action: &str) -> KeymapOp {
        KeymapOp::Set {
            table: "prefix".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        }
    }

    /// The keys a row names, ranges spelled out.
    fn row_keys(row: &HintRow) -> Vec<String> {
        row.keys
            .split(' ')
            .flat_map(|part| match part.as_bytes() {
                [a, b'-', b] if a.is_ascii_digit() && b.is_ascii_digit() => {
                    (*a..=*b).map(|c| (c as char).to_string()).collect()
                }
                _ => vec![part.to_string()],
            })
            .collect()
    }

    #[test]
    fn parse_which_key_delay_reads_off_and_a_range_of_milliseconds() {
        assert_eq!(parse_which_key_delay(None), Ok(None));
        for off in ["off", "false"] {
            assert_eq!(parse_which_key_delay(Some(off)), Ok(None), "{off}");
        }
        for (text, ms) in [("0", 0), ("200", 200), ("5000", 5000)] {
            assert_eq!(
                parse_which_key_delay(Some(text)),
                Ok(Some(Duration::from_millis(ms))),
                "{text}"
            );
        }
        for bad in ["5001", "-1", "1.5", "on", ""] {
            let err = parse_which_key_delay(Some(bad)).unwrap_err();
            assert!(err.contains(WHICH_KEY_DELAY_KEY), "{bad}: {err}");
            assert!(
                err.contains("must be a whole number from 0 to 5000, or false or \"off\""),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn panel_groups_over_the_defaults_lists_every_default_binding_once() {
        let keymap = Keymap::defaults();
        let groups = keymap.panel_groups(&plain_keys(&keymap), &title, |_| true);
        let titles: Vec<&str> = groups.iter().map(|g| g.title).collect();
        assert_eq!(
            titles,
            ["Tabs", "Modules", "Layout", "Workspaces", "Projects", "Other"],
            "no default binding is a Windows one, and an empty group is left out"
        );
        for binding in keymap.bindings() {
            let group = GROUPS[group_of(family(&binding.action))].0;
            let text = key_text(&binding.key);
            let hits: Vec<(&str, &HintRow)> = groups
                .iter()
                .flat_map(|g| g.rows.iter().map(move |row| (g.title, row)))
                .filter(|(_, row)| row_keys(row).contains(&text))
                .collect();
            assert_eq!(hits.len(), 1, "{text} for {:?}: {hits:?}", binding.action);
            let (title, row) = hits[0];
            assert_eq!(title, group, "{text} sits in its family's group");
            assert!(
                row.label.starts_with(binding.action.short_label()),
                "{text}: {} for {:?}",
                row.label,
                binding.action
            );
        }
    }

    #[test]
    fn a_tmux_imported_and_a_user_binding_are_listed_with_their_keys() {
        let home = crate::test_scratch_dir::ScratchDir::new("eitri-hints", "tmux");
        std::fs::write(home.join(".tmux.conf"), "bind m kill-pane\n").unwrap();
        let read = crate::keymap::tmux::read(&crate::keymap::tmux::tests::env_for(&home));
        let (imported, _) = Keymap::with_tmux(&read, &crate::keymap::LuaClaims::default());
        let groups = imported.panel_groups(&plain_keys(&imported), &title, |_| true);
        let modules = groups.iter().find(|g| g.title == "Modules").unwrap();
        assert!(
            modules.rows.contains(&HintRow {
                keys: "m".into(),
                label: "close module (tmux)".into()
            }),
            "{modules:?}"
        );
        let user = imported.then_user(&[set("g", "zoom")], &[]).unwrap();
        let groups = user.panel_groups(&plain_keys(&user), &title, |_| true);
        let layout = groups.iter().find(|g| g.title == "Layout").unwrap();
        assert!(
            layout
                .rows
                .iter()
                .any(|row| row_keys(row).contains(&"g".to_string()) && row.label == "zoom"),
            "{layout:?}"
        );
    }

    #[test]
    fn a_keep_that_drops_layout_drops_its_rows() {
        let keymap = Keymap::defaults();
        let all = keymap.panel_groups(&plain_keys(&keymap), &title, |_| true);
        assert!(all.iter().any(|g| g.title == "Layout"));
        let kept = keymap.panel_groups(&plain_keys(&keymap), &title, |a| family(a) != Family::Tiling);
        assert!(kept.iter().all(|g| g.title != "Layout"), "{kept:?}");
        assert!(kept.iter().any(|g| g.title == "Modules"));
    }

    #[test]
    fn a_lua_panel_key_gets_a_row_with_its_title() {
        let keymap = Keymap::defaults();
        let free = ('a'..='z')
            .chain('A'..='Z')
            .find(|c| ModuleKeys::build(&[(ModuleId::lua("notes"), Some(c.to_string()))], &keymap).is_ok())
            .expect("some letter is free");
        let keys = ModuleKeys::build(&[(ModuleId::lua("notes"), Some(free.to_string()))], &keymap).unwrap();
        let groups = keymap.panel_groups(&keys, &title, |_| true);
        let modules = groups.iter().find(|g| g.title == "Modules").unwrap();
        assert_eq!(
            modules.rows.last(),
            Some(&HintRow {
                keys: free.to_string(),
                label: "notes".into()
            })
        );
        let none = keymap.panel_groups(&keys, &title, |a| *a != Action::Module(ModuleId::lua("notes")));
        assert!(none.iter().all(|g| g.rows.iter().all(|row| row.label != "notes")));
    }

    #[test]
    fn repeatable_rows_say_so() {
        let keymap = Keymap::defaults();
        let groups = keymap.panel_groups(&plain_keys(&keymap), &title, |_| true);
        let repeating = keymap
            .bindings()
            .iter()
            .find(|b| b.repeatable)
            .expect("a default repeats");
        let text = key_text(&repeating.key);
        let row = groups
            .iter()
            .flat_map(|g| &g.rows)
            .find(|row| row_keys(row).contains(&text))
            .unwrap();
        assert!(row.label.ends_with(" (repeats)"), "{row:?}");
        let plain = keymap.bindings().iter().find(|b| !b.repeatable).unwrap();
        let row = groups
            .iter()
            .flat_map(|g| &g.rows)
            .find(|row| row_keys(row).contains(&key_text(&plain.key)))
            .unwrap();
        assert!(!row.label.contains("repeats"), "{row:?}");
    }

    #[test]
    fn arrows_print_as_arrows_and_keep_their_modifiers() {
        assert_eq!(key_text(&KeySpec::parse("Up").unwrap()), "\u{2191}");
        assert_eq!(key_text(&KeySpec::parse("C-Left").unwrap()), "Ctrl+\u{2190}");
        assert_eq!(key_text(&KeySpec::parse("M-Right").unwrap()), "Alt+\u{2192}");
        assert_eq!(key_text(&KeySpec::parse("x").unwrap()), "x");
    }

    #[test]
    fn digits_join_as_a_range() {
        let keys = |names: &[&str]| -> Vec<KeySpec> { names.iter().map(|n| KeySpec::parse(n).unwrap()).collect() };
        assert_eq!(join_keys(&keys(&["1", "2", "3", "4", "5", "6", "7", "8", "9"])), "1-9");
        assert_eq!(join_keys(&keys(&["1", "2"])), "1 2");
        assert_eq!(join_keys(&keys(&["1", "2", "3", "x"])), "1-3 x");
        assert_eq!(join_keys(&keys(&["1", "3", "4", "5"])), "1 3-5");
        assert_eq!(join_keys(&keys(&["M-1", "M-2", "M-3"])), "Alt+1 Alt+2 Alt+3");
    }

    #[test]
    fn project_choose_has_its_own_family_and_group() {
        assert_eq!(family(&Action::ProjectChoose), Family::Project);
        assert_eq!(GROUPS[group_of(Family::Project)], ("Projects", &[Family::Project][..]));
        let map = Keymap::defaults();
        let groups = map.panel_groups(&plain_keys(&map), &title, |_| true);
        let projects = groups.iter().find(|group| group.title == "Projects").unwrap();
        assert_eq!(
            projects.rows,
            [HintRow {
                keys: "O".into(),
                label: "open a project".into()
            }]
        );
    }
}
