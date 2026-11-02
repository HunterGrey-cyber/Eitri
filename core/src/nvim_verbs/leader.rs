//! `keymap.nvim_prefix`: a group of Normal-mode keys in the embedded nvim that run the same window and layout
//! actions the prefix table binds, through `:Eitri`.
//!
//! The setting is read here ([`parse_nvim_prefix`]); the keys are worked out here ([`leader_group`]) and handed to
//! the snippet (`eitri_commands.lua`), which maps them under the prefix only where nothing of the user's is in
//! the way.

use crate::keymap::{Action, KeySpec, Keymap};
use crate::layout::{Axis, ModuleKeys};
use crate::lua::layout::run_refusal;
use crate::nvim_verbs::parse_words;
use crate::pane_switch::{is_verb_word, VERB_MAX_WORDS};

/// The `init.lua` key.
pub const NVIM_PREFIX_KEY: &str = "keymap.nvim_prefix";

/// The longest `keymap.nvim_prefix`, in bytes.
const MAX_PREFIX_BYTES: usize = 16;

/// `keymap.nvim_prefix`: unset, `false` or `"off"` is no group. Anything else is the left-hand side the keys go
/// under, written as for `:map` (`"<leader>W"`, `"<Space>w"`): 1 to 16 bytes of printable ASCII without a space,
/// whose `<` and `>` pair up. `Err` is the text after "eitri: " and names the key.
pub fn parse_nvim_prefix(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value else { return Ok(None) };
    if crate::config_switch::is_off(value) {
        return Ok(None);
    }
    let refuse = |why: &str| Err(format!("eitri.config.set(\"{NVIM_PREFIX_KEY}\", {value:?}): {why}"));
    if value.is_empty() || value.len() > MAX_PREFIX_BYTES {
        return refuse(&format!("it must be 1 to {MAX_PREFIX_BYTES} bytes long"));
    }
    if !value.bytes().all(|b| b.is_ascii_graphic()) {
        return refuse("it may hold printable ASCII only, and no space (write <Space>)");
    }
    let mut open = false;
    for b in value.bytes() {
        match (b, open) {
            (b'<', false) => open = true,
            (b'>', true) => open = false,
            (b'<' | b'>', _) => return refuse("its < and > must pair up, as in <leader>W"),
            _ => {}
        }
    }
    if open {
        return refuse("its < and > must pair up, as in <leader>W");
    }
    Ok(Some(value.to_string()))
}

/// One key of the group: pressed after the prefix, it sends `words` as `:Eitri` would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderKey {
    /// The keys after the group's prefix, in nvim's notation (`z`, `<C-x>`, `%a`).
    pub suffix: String,
    /// What `:Eitri` is given.
    pub words: Vec<String>,
    /// What the key's description says after `Eitri: `.
    pub desc: String,
}

/// The keys of the prefix table `:Eitri` can run, in table order: each binding whose action
/// `eitri.layout.run` runs, and each split binding once per module key (`%a` is `split right agent`). A
/// binding whose action waits for a further key or is no layout action (`run_refusal`: tabs, copy mode, the
/// prefix itself), and one whose words could not travel in a `W` line, is left out. `lua_panels` are the ids of
/// every Lua panel `init.lua` registered, with a module key or without (`module.<id>` can be bound to either).
pub fn leader_group(keymap: &Keymap, keys: &ModuleKeys, lua_panels: &[String]) -> Vec<LeaderKey> {
    let runs = |words: &[String]| {
        (1..=VERB_MAX_WORDS).contains(&words.len())
            && words.iter().all(|word| is_verb_word(word))
            && parse_words(words, lua_panels).is_ok()
    };
    let mut out = Vec::new();
    for binding in keymap.bindings() {
        let suffix = binding.key.to_vim();
        match &binding.action {
            Action::Split { axis, .. } => {
                let side = match axis {
                    Axis::Row => "right",
                    Axis::Column => "below",
                };
                for (key, id) in keys.entries() {
                    let words: Vec<String> = ["split", side, id.as_str()].map(str::to_string).into();
                    if runs(&words) {
                        out.push(LeaderKey {
                            suffix: format!("{suffix}{}", KeySpec::char(*key, false, false).to_vim()),
                            desc: words.join(" "),
                            words,
                        });
                    }
                }
            }
            action if run_refusal(&action.name(), action).is_none() => {
                let mut words = vec![action.name()];
                for (key, value) in action.options() {
                    words.push(format!("{key}={}", option_text(&value)));
                }
                if runs(&words) {
                    out.push(LeaderKey {
                        suffix,
                        words,
                        desc: action.short_label().to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn option_text(value: &crate::keymap::OptValue) -> String {
    use crate::keymap::OptValue;
    match value {
        OptValue::Int(n) => n.to_string(),
        OptValue::Bool(b) => b.to_string(),
        OptValue::Str(s) => s.clone(),
        OptValue::Other(kind) => kind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(value: &str) -> Result<Option<String>, String> {
        parse_nvim_prefix(Some(value))
    }

    #[test]
    fn a_prefix_is_off_unless_init_lua_gives_one() {
        assert_eq!(parse_nvim_prefix(None), Ok(None));
        assert_eq!(prefix("off"), Ok(None));
        assert_eq!(prefix("false"), Ok(None));
    }

    #[test]
    fn a_prefix_is_a_map_lhs_of_printable_ascii() {
        assert_eq!(prefix("<leader>W"), Ok(Some("<leader>W".to_string())));
        assert_eq!(prefix("<Space>w"), Ok(Some("<Space>w".to_string())));
        assert_eq!(prefix(&"a".repeat(16)), Ok(Some("a".repeat(16))));
    }

    #[test]
    fn a_prefix_that_cannot_be_a_lhs_names_its_key() {
        for bad in [
            &"a".repeat(17)[..],
            "<leader",
            " ",
            "",
            "a b",
            "leader>",
            "<<a>>",
            "caf\u{e9}",
        ] {
            let err = prefix(bad).expect_err(bad);
            assert!(err.contains(NVIM_PREFIX_KEY), "{bad:?}: {err}");
        }
    }

    fn group() -> Vec<LeaderKey> {
        leader_group(&Keymap::defaults(), &ModuleKeys::built_in(), &[])
    }

    fn key(suffix: &str) -> LeaderKey {
        group()
            .into_iter()
            .find(|k| k.suffix == suffix)
            .unwrap_or_else(|| panic!("no key {suffix:?}"))
    }

    #[test]
    fn the_default_table_gives_zoom_and_the_split_pairs() {
        assert_eq!(key("z").words, ["zoom"]);
        assert_eq!(key("z").desc, "zoom");
        assert_eq!(key("%a").words, ["split", "right", "agent"]);
        assert_eq!(key("\"t").words, ["split", "below", "terminal"]);
        assert_eq!(key("\"t").desc, "split below terminal");
    }

    #[test]
    fn one_split_binding_gives_one_key_per_module_key() {
        let splits: Vec<_> = group().into_iter().filter(|k| k.suffix.starts_with('%')).collect();
        assert_eq!(splits.len(), ModuleKeys::built_in().entries().len());
    }

    #[test]
    fn keys_that_wait_or_are_no_layout_action_are_left_out() {
        let words: Vec<Vec<String>> = group().into_iter().map(|k| k.words).collect();
        for refused in [
            "send-prefix",
            "tab.next",
            "copy-mode",
            "select.number",
            "workspace.choose",
        ] {
            assert!(
                words.iter().all(|w| w[0] != refused),
                "{refused} must not be in the group"
            );
        }
        for suffix in ["c", "n", "p", "1", "2", "9", ","] {
            assert!(
                group().iter().all(|k| k.suffix != suffix),
                "{suffix} is a tab key and has no leader key"
            );
        }
    }

    #[test]
    fn a_binding_to_a_lua_panel_without_a_module_key_has_a_key() {
        use crate::keymap::KeymapOp;
        let ops = [KeymapOp::Set {
            table: "prefix".into(),
            key: "N".into(),
            action: "module.notes".into(),
            opts: Vec::new(),
        }];
        let keymap = Keymap::apply_user(&ops, &["notes".to_string()]).expect("module.notes binds");
        let keyless = ModuleKeys::built_in();
        let with = leader_group(&keymap, &keyless, &["notes".to_string()]);
        let found = with.iter().find(|k| k.suffix == "N").expect("the key for module.notes");
        assert_eq!(found.words, ["module.notes"]);
        let without = leader_group(&keymap, &keyless, &[]);
        assert!(without.iter().all(|k| k.suffix != "N"), "an unknown panel is no key");
    }

    #[test]
    fn a_workspace_whose_name_reads_as_a_number_keeps_its_key() {
        use crate::keymap::{KeymapOp, OptValue};
        let ops = [KeymapOp::Set {
            table: "prefix".into(),
            key: "N".into(),
            action: "workspace.switch".into(),
            opts: vec![("name".to_string(), OptValue::Str("123".to_string()))],
        }];
        let keymap = Keymap::apply_user(&ops, &[]).expect("workspace.switch binds");
        let group = leader_group(&keymap, &ModuleKeys::built_in(), &[]);
        let found = group
            .iter()
            .find(|k| k.suffix == "N")
            .expect("the key for the workspace");
        assert_eq!(found.words, ["workspace.switch", "name=123"]);
    }

    #[test]
    fn every_key_is_words_eitri_would_run() {
        for key in group() {
            assert!(
                parse_words(&key.words, &[]).is_ok(),
                "{:?} would be refused by :Eitri",
                key.words
            );
        }
    }

    #[test]
    fn the_leader_group_lists_it() {
        assert_eq!(key("O").words, ["project.choose"]);
        assert_eq!(key("O").desc, "open a project");
    }
}
